//! Bounded streaming upload preparation and durable task publication.
use super::*;

impl Upload {
    pub(crate) fn seal(&mut self) -> Result<(), WebTaskError> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| WebTaskError::Conflict("upload is finished".into()))?;
        file.flush()?;
        file.sync_all()?;
        self.directory
            .as_ref()
            .ok_or_else(|| WebTaskError::Conflict("upload is finished".into()))?
            .sync()
            .map_err(|e| WebTaskError::Unsafe(e.to_string()))?;
        Ok(())
    }
    /// Append one body chunk after cancellation and all quota checks.
    pub fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), WebTaskError> {
        if let Some(id) = &self.receipt_id {
            if self.backend.receipt(id)?.state == "cancelled" {
                return Err(WebTaskError::Cancelled);
            }
        }
        let amount = u64::try_from(chunk.len())
            .map_err(|_| WebTaskError::Limit("chunk length overflow".into()))?;
        let next = self
            .bytes
            .checked_add(amount)
            .ok_or_else(|| WebTaskError::Limit("upload length overflow".into()))?;
        if next > self.request.options.limits.max_input_bytes {
            return Err(WebTaskError::Limit("file exceeds max_input_bytes".into()));
        }
        let mut reservation = QuotaReservation::acquire(&self.backend.owner.shared, amount)?;
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| WebTaskError::Conflict("upload is already finished".into()))?;
        let mut remaining = chunk;
        while !remaining.is_empty() {
            let written = file.write(remaining)?;
            if written == 0 {
                return Err(WebTaskError::Io("file write made no progress".into()));
            }
            if let Some((hash, _)) = &mut self.input_hash {
                hash.update(&remaining[..written]);
            }
            self.bytes += written as u64;
            reservation.commit(written as u64);
            remaining = &remaining[written..];
        }
        debug_assert_eq!(self.bytes, next);
        Ok(())
    }

    /// fsync input, bind fingerprints/token/task ID, and enqueue conversion.
    pub fn finish(mut self) -> Result<TaskRecord, WebTaskError> {
        let _timing = timing::Stage::new("receivePrepare", None, self.receipt_id.as_deref(), 0);
        let mut file = self
            .file
            .take()
            .ok_or_else(|| WebTaskError::Conflict("upload is already finished".into()))?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        let directory = self
            .directory
            .as_ref()
            .ok_or_else(|| WebTaskError::Conflict("upload is already finished".into()))?;
        directory.sync().map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        validate_private_directory_handle(directory)?;
        let payload = self
            .directory
            .as_ref()
            .ok_or_else(|| WebTaskError::Conflict("upload is already finished".into()))?
            .open_regular_private(std::ffi::OsStr::new("payload"))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        validate_private_file(&payload)?;
        let options = self.request.options.clone();
        let hint = FormatHint {
            format: self.request.format,
            filename: Some(self.name.clone()),
            ..FormatHint::default()
        };
        let (input_fingerprint, options_fingerprint) =
            self.fingerprints(payload, &hint, &options)?;
        let ocr_enabled = options.ocr.policy != OcrPolicy::Off;
        let preserve_layout = options.ai.layout_repair != AiMode::Off;
        let persisted = PersistedRequest {
            schema_version: 1,
            workflow: self.request.workflow,
            name: self.name.clone(),
            hint,
            batch_id: self.request.batch_id.clone(),
            options,
            receipt_id: self.receipt_id.clone(),
            retry_of: self.retry_of.clone(),
        };
        let request_json = bounded_json(&persisted, 64 * 1024, "persisted request")?;
        let shared = Arc::clone(&self.backend.owner.shared);
        let _metadata_lease = DiskLease::acquire_interruptible(
            &shared,
            1024 * 1024,
            &CancellationToken::new(),
            Instant::now() + Duration::from_secs(1),
            None,
        )?;
        let token = self
            .backend
            .owner
            .shared
            .recovery
            .create_token()
            .map_err(|error| WebTaskError::Io(error.to_string()))?;
        let record = metadata_store_mutation(
            &self.backend.owner.shared,
            STORE_MUTATION_RESERVATION,
            |store| {
                Ok(store.create(NewTask {
                    input: InputReference {
                        schema_version: 1,
                        input_fingerprint,
                        options_fingerprint,
                        byte_len: self.bytes,
                        recovery_token: token.as_str().to_owned(),
                    },
                    configuration: ConfigurationSnapshot {
                        schema_version: 1,
                        output_format: into_markdown::OutputFormat::Markdown,
                        ocr_enabled,
                        preserve_layout,
                    },
                })?)
            },
        )?;
        let finalized = self.publish_input(&record, &request_json);
        match finalized {
            Ok(()) => {}
            Err(error) => {
                if terminal_transition(
                    &self.backend.owner.shared,
                    &record.id,
                    TaskStatus::Interrupted,
                    DiagnosticCode::RecoveryCheckpointMissing,
                )
                .is_err()
                {
                    stop_unhealthy(&self.backend.owner.shared);
                }
                return Err(error);
            }
        }
        self.enqueue_received(record, token, persisted)
    }

    fn fingerprints(
        &mut self,
        payload: File,
        hint: &FormatHint,
        options: &ConversionOptions,
    ) -> Result<(String, String), WebTaskError> {
        Ok(if let Some((mut hash, expected)) = self.input_hash.take() {
            if expected != self.bytes {
                return Err(WebTaskError::Invalid(
                    "upload length differs from declared size".into(),
                ));
            }
            let metadata = serde_json::to_vec(&(
                Some(self.name.as_str()),
                None::<&str>,
                None::<&str>,
                self.bytes,
            ))
            .map_err(|e| WebTaskError::Io(e.to_string()))?;
            hash.update((metadata.len() as u64).to_le_bytes());
            hash.update(metadata);
            let (_, options_hash) =
                Engine::recoverable_fingerprints(&[], Some(&self.name), &hint, &options)
                    .map_err(|e| WebTaskError::Io(e.to_string()))?;
            (format!("{:x}", hash.finalize()), options_hash)
        } else {
            Engine::recoverable_fingerprints_reader(
                payload,
                self.bytes,
                Some(&self.name),
                &hint,
                &options,
            )
            .map_err(|e| WebTaskError::Io(e.to_string()))?
        })
    }

    fn publish_input(
        &mut self,
        record: &TaskRecord,
        request_json: &[u8],
    ) -> Result<(), WebTaskError> {
        self.backend
            .owner
            .shared
            .objects
            .verify_private_namespace()
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let task = self
            .backend
            .owner
            .shared
            .objects
            .create_child_private(std::ffi::OsStr::new(record.id.as_str()))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        write_private_handle(&task, "request.json", &request_json)?;
        self.directory
            .as_ref()
            .ok_or_else(|| WebTaskError::Conflict("upload is already finished".into()))?
            .rename_child_private_to_no_replace(
                std::ffi::OsStr::new("payload"),
                &task,
                std::ffi::OsStr::new("input"),
            )
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        // Windows refuses to remove a directory while its pinned handle is
        // live. The payload is already durably published, so release that
        // handle before removing the now-empty incoming directory.
        drop(self.directory.take());
        self.backend
            .owner
            .shared
            .incoming
            .remove_empty_child_private(std::ffi::OsStr::new(&self.nonce))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        Ok::<_, WebTaskError>(())
    }

    fn enqueue_received(
        &mut self,
        record: TaskRecord,
        token: RecoveryToken,
        persisted: PersistedRequest,
    ) -> Result<TaskRecord, WebTaskError> {
        timing::accepted(&record.id, self.receipt_id.as_deref());
        self.backend.index_metadata(&record.id, &persisted)?;
        if let Some(id) = &self.receipt_id {
            let mut receipt = self.backend.receipt(id)?;
            if receipt.state == "cancelled" {
                terminal_transition(
                    &self.backend.owner.shared,
                    &record.id,
                    TaskStatus::Cancelled,
                    DiagnosticCode::Cancelled,
                )?;
                self.committed = true;
                return Err(WebTaskError::Cancelled);
            }
            self.backend.change_receipt(&mut receipt, "accepted", Some(record.id.clone()), None)?;
        }
        self.committed = true;
        if let Err(error) = self.backend.enqueue(Job {
            id: record.id.clone(),
            token,
            request: persisted,
            cancellation: CancellationToken::new(),
            admission_ticket: None,
        }) {
            if terminal_transition(
                &self.backend.owner.shared,
                &record.id,
                TaskStatus::Interrupted,
                DiagnosticCode::RecoveryCheckpointMissing,
            )
            .is_err()
            {
                stop_unhealthy(&self.backend.owner.shared);
            }
            return Err(error);
        }
        self.backend.owner.shared.events.publish_snapshot(&record);
        Ok(record)
    }
}

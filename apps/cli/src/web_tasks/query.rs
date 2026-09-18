//! Bounded task queries and independently cached, authenticated text previews.
use super::*;

#[derive(Clone)]
pub(super) struct Metadata {
    pub(super) name: String,
    pub(super) batch: Option<String>,
    pub(super) workflow: WebWorkflow,
    pub(super) format: Option<InputFormat>,
    pub(super) retry_of: Option<TaskId>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskSummary {
    #[serde(flatten)]
    task: WebTaskRecord,
    generation: String,
    sequence: u64,
    execution: Option<ProgressEvent>,
    waiting_reason: Option<&'static str>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResultPreview {
    text: String,
    truncated: bool,
    content_type: String,
}

const PREVIEW_BYTES: usize = 256 * 1024;

pub(super) fn bounded_preview(bytes: &[u8], content_type: &str) -> ResultPreview {
    let mut end = bytes.len().min(PREVIEW_BYTES);
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
        if bytes.len().min(PREVIEW_BYTES) - end > 3 {
            break;
        }
    }
    ResultPreview {
        text: String::from_utf8_lossy(&bytes[..end]).into_owned(),
        truncated: end < bytes.len(),
        content_type: content_type.into(),
    }
}

impl WebTaskBackend {
    pub(crate) fn detail(&self, id: &TaskId) -> Result<WebTaskRecord, WebTaskError> {
        let record = lock(&self.owner.shared.task_store).get(id)?.ok_or(WebTaskError::NotFound)?;
        self.web_record(record)
    }

    pub(super) fn index_metadata(
        &self,
        id: &TaskId,
        request: &PersistedRequest,
    ) -> Result<(), WebTaskError> {
        let workflow = match request.workflow {
            WebWorkflow::Conversion => "conversion",
            WebWorkflow::MeetingTranscript => "meetingTranscript",
        };
        metadata_store_mutation(&self.owner.shared, STORE_MUTATION_RESERVATION, |store| {
            store
                .index_web_task(id, &request.name, request.batch_id.as_deref(), workflow)
                .map_err(Into::into)
        })?;
        lock(&self.owner.shared.metadata).insert(
            id.clone(),
            Metadata {
                name: request.name.clone(),
                batch: request.batch_id.clone(),
                workflow: request.workflow,
                format: request.hint.format,
                retry_of: request.retry_of.clone(),
            },
        );
        Ok(())
    }

    pub(super) fn rebuild_metadata(&self) -> Result<(), WebTaskError> {
        let mut cursor = None;
        loop {
            let page = lock(&self.owner.shared.task_store).list(100, cursor.as_ref())?;
            for record in &page {
                if let Some(request) = self.persisted_request(&record.id)? {
                    self.index_metadata(&record.id, &request)?;
                    if let Some(id) = &request.receipt_id {
                        if let Ok(mut receipt) = self.receipt(id) {
                            if receipt.task_id.is_none() {
                                self.change_receipt(
                                    &mut receipt,
                                    "accepted",
                                    Some(record.id.clone()),
                                    None,
                                )?;
                            }
                        }
                    }
                }
            }
            let Some(last) = page.last() else { break };
            cursor = Some(TaskCursor { updated_at_ms: last.updated_at_ms, id: last.id.clone() });
        }
        Ok(())
    }

    pub(crate) fn summaries(&self, ids: &[TaskId]) -> Result<Vec<TaskSummary>, WebTaskError> {
        self.ensure_available()?;
        if ids.len() > 100 {
            return Err(WebTaskError::Invalid("too many task IDs".into()));
        }
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let cached = {
                let events = lock(&self.owner.shared.events.state);
                events.logs.get(id).map(|log| {
                    let mut record = log.latest_record.clone();
                    let latest = log.events.back();
                    if let Some(event) = latest {
                        record.progress_millionths = event.progress_millionths;
                    }
                    (
                        record,
                        log.next_sequence.saturating_sub(1),
                        latest.and_then(|event| event.execution.clone()),
                    )
                })
            };
            let (record, sequence, execution) = if let Some(snapshot) = cached {
                snapshot
            } else {
                let Some(record) = lock(&self.owner.shared.task_store).get_summary(id)? else {
                    continue;
                };
                (
                    record,
                    self.owner.shared.events.sequence.load(Ordering::Relaxed).saturating_sub(1),
                    None,
                )
            };
            let waiting_reason = (record.status == TaskStatus::Pending).then(|| {
                if lock(&self.owner.shared.queue).jobs.iter().any(|job| job.id == record.id) {
                    "worker"
                } else {
                    "disk"
                }
            });
            result.push(TaskSummary {
                waiting_reason,
                task: self.web_record(record)?,
                generation: self.owner.shared.events.generation.clone(),
                sequence,
                execution,
            });
        }
        Ok(result)
    }

    pub(crate) fn query(
        &self,
        limit: u32,
        after: Option<&TaskCursor>,
        status: Option<TaskStatus>,
        pinned: Option<bool>,
        batch: Option<&str>,
        workflow: Option<&str>,
        search: &str,
        active: Option<bool>,
    ) -> Result<TaskHistoryPage, WebTaskError> {
        let tasks = lock(&self.owner.shared.task_store)
            .list_web(limit, after, status, pinned, batch, workflow, search, active)?;
        let next = if tasks.len() == limit as usize {
            tasks
                .last()
                .map(|last| TaskCursor { updated_at_ms: last.updated_at_ms, id: last.id.clone() })
        } else {
            None
        };
        Ok(TaskHistoryPage { tasks, next })
    }

    pub(crate) fn preview(
        &self,
        id: &TaskId,
        key: &str,
        cancellation: &CancellationToken,
    ) -> Result<ResultPreview, WebTaskError> {
        validate_key(key)?;
        let record = lock(&self.owner.shared.task_store).get(id)?.ok_or(WebTaskError::NotFound)?;
        if record.status != TaskStatus::Succeeded {
            return Err(WebTaskError::Conflict("result is being published".into()));
        }
        let _timing = timing::Stage::new("previewRead", Some(id), None, record.artifact_generation);
        let reference = record
            .artifacts
            .iter()
            .find(|item| item.storage_key == key)
            .ok_or(WebTaskError::NotFound)?;
        if !matches!(reference.kind, ArtifactKind::Markdown | ArtifactKind::Diagnostics) {
            return Err(WebTaskError::Invalid("artifact has no text preview".into()));
        }
        let cache_key =
            format!("{}:{}:{key}:{}", id.as_str(), record.artifact_generation, reference.sha256);
        if let Some(wire) =
            lock(&self.owner.shared.task_store).web_preview(id, key, &reference.sha256)?
        {
            return serde_json::from_str(&wire).map_err(|e| WebTaskError::Unsafe(e.to_string()));
        }
        if let Some((_, preview)) =
            lock(&self.owner.shared.previews).iter().find(|(key, _)| *key == cache_key)
        {
            return Ok(preview.clone());
        }
        let task = self
            .owner
            .shared
            .objects
            .open_child_private(std::ffi::OsStr::new(id.as_str()))
            .map_err(|e| WebTaskError::Unsafe(e.to_string()))?;
        let publication = task
            .open_child_private(std::ffi::OsStr::new(&publication_directory_name(&record)))
            .map_err(|e| WebTaskError::Unsafe(e.to_string()))?;
        let mut file = publication
            .open_regular_private(std::ffi::OsStr::new(key))
            .map_err(|e| WebTaskError::Unsafe(e.to_string()))?;
        validate_private_file(&file)?;
        let mut hash = Sha256::new();
        let mut prefix = Vec::new();
        let mut bytes = 0u64;
        let mut buffer = [0u8; COPY_CHUNK];
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            cancelled(cancellation)?;
            if Instant::now() > deadline {
                return Err(WebTaskError::Limit("preview preparation timed out".into()));
            }
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            bytes = bytes
                .checked_add(read as u64)
                .ok_or_else(|| WebTaskError::Limit("preview size overflow".into()))?;
            if bytes > reference.byte_len {
                return Err(WebTaskError::Unsafe("artifact grew while previewing".into()));
            }
            hash.update(&buffer[..read]);
            let keep = read.min(PREVIEW_BYTES.saturating_sub(prefix.len()));
            prefix.extend_from_slice(&buffer[..keep]);
        }
        if bytes != reference.byte_len || format!("{:x}", hash.finalize()) != reference.sha256 {
            return Err(WebTaskError::Unsafe("preview artifact digest changed".into()));
        }
        let mut preview = bounded_preview(
            &prefix,
            if reference.kind == ArtifactKind::Markdown {
                "text/markdown"
            } else {
                "application/json"
            },
        );
        preview.truncated |= bytes > prefix.len() as u64;
        let wire = serde_json::to_string(&preview).map_err(|e| WebTaskError::Io(e.to_string()))?;
        metadata_store_mutation(&self.owner.shared, STORE_METADATA_HEADROOM, |store| {
            store.write_web_preview(id, key, &reference.sha256, &wire).map_err(Into::into)
        })?;
        let mut cache = lock(&self.owner.shared.previews);
        if cache.len() >= 32 {
            cache.pop_front();
        }
        cache.push_back((cache_key, preview.clone()));
        Ok(preview)
    }
}

pub(super) fn publish_previews(
    shared: &Shared,
    id: &TaskId,
    result: &into_markdown::ConversionResult,
    entries: &[ArtifactReference],
) -> Result<(), WebTaskError> {
    for reference in entries
        .iter()
        .filter(|item| matches!(item.kind, ArtifactKind::Markdown | ArtifactKind::Diagnostics))
    {
        let preview = if reference.kind == ArtifactKind::Markdown {
            bounded_preview(result.markdown.as_bytes(), "text/markdown")
        } else {
            let diagnostics = &result.diagnostics[..result.diagnostics.len().min(100)];
            let mut summary = serde_json::json!({
                "schemaVersion": 1, "diagnostics": diagnostics,
                "outcome": match result.outcome() { into_markdown::ConversionOutcome::Complete if !diagnostics::bundle_unavailable(result) => "complete", _ => "degraded" },
                "ocrRuntime": result.ocr_runtime_usage(),
                "diagnosticCount": result.diagnostics.len(),
                "summaryTruncated": result.diagnostics.len() > diagnostics.len()
            });
            let mut bytes =
                serde_json::to_vec(&summary).map_err(|e| WebTaskError::Io(e.to_string()))?;
            while bytes.len() > PREVIEW_BYTES {
                let Some(items) =
                    summary.get_mut("diagnostics").and_then(serde_json::Value::as_array_mut)
                else {
                    break;
                };
                if items.pop().is_none() {
                    break;
                }
                summary["summaryTruncated"] = serde_json::Value::Bool(true);
                bytes =
                    serde_json::to_vec(&summary).map_err(|e| WebTaskError::Io(e.to_string()))?;
            }
            bounded_preview(&bytes, "application/json")
        };
        let wire = serde_json::to_string(&preview).map_err(|e| WebTaskError::Io(e.to_string()))?;
        metadata_store_mutation(shared, STORE_METADATA_HEADROOM, |store| {
            store
                .write_web_preview(id, &reference.storage_key, &reference.sha256, &wire)
                .map_err(Into::into)
        })?;
    }
    Ok(())
}

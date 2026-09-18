//! Validated task row decoding with optional artifact enumeration.
use super::*;
impl TaskStore {
    pub(super) fn get_record(
        &self,
        id: &TaskId,
        with_artifacts: bool,
    ) -> Result<Option<TaskRecord>, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        let base = self
            .connection
            .query_row(
                "SELECT created_at_ms, updated_at_ms, status, progress, input_fingerprint, options_fingerprint, recovery_token, input_bytes, config_json, artifact_generation, pinned FROM tasks WHERE id=?1",
                [id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, i64>(10)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| self.map_sqlite(error))?;
        let Some((
            created,
            updated,
            status,
            progress,
            input_fingerprint,
            options_fingerprint,
            recovery_token,
            input_bytes,
            configuration,
            artifact_generation,
            pinned,
        )) = base
        else {
            return Ok(None);
        };
        if created < 0 || updated < created || !(0..=1_000_000).contains(&progress) {
            return Err(TaskStoreError::Corrupt("task timestamps or progress are invalid".into()));
        }
        let status = TaskStatus::parse(&status)?;
        if status == TaskStatus::Succeeded && progress != 1_000_000 {
            return Err(TaskStoreError::Corrupt(
                "succeeded task does not have complete progress".into(),
            ));
        }
        if input_bytes < 0 {
            return Err(TaskStoreError::Corrupt("task input byte length is invalid".into()));
        }
        let input = InputReference {
            schema_version: 1,
            input_fingerprint,
            options_fingerprint,
            recovery_token,
            byte_len: u64::try_from(input_bytes)
                .map_err(|_| TaskStoreError::Corrupt("task input byte length is invalid".into()))?,
        };
        validate_input(&input)?;
        let configuration: ConfigurationSnapshot = decode_bounded(&configuration, "configuration")?;
        validate_configuration(&configuration)?;
        let diagnostics = if with_artifacts { self.load_diagnostics(id)? } else { Vec::new() };
        let artifacts = if with_artifacts { self.load_artifacts(id)? } else { Vec::new() };
        let artifact_generation = u64::try_from(artifact_generation)
            .map_err(|_| TaskStoreError::Corrupt("artifact generation is invalid".into()))?;
        Ok(Some(TaskRecord {
            id: id.clone(),
            created_at_ms: created,
            updated_at_ms: updated,
            status,
            progress_millionths: u32::try_from(progress)
                .map_err(|_| TaskStoreError::Corrupt("task progress is invalid".into()))?,
            input,
            configuration,
            diagnostics,
            artifacts,
            artifact_generation,
            pinned: match pinned {
                0 => false,
                1 => true,
                _ => return Err(TaskStoreError::Corrupt("task pinned marker is invalid".into())),
            },
        }))
    }
}

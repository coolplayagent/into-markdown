//! Durable upload identity separates transport completion from task preparation.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UploadReceipt {
    pub(crate) id: String,
    pub(crate) state: String,
    pub(crate) name: String,
    pub(crate) size: u64,
    #[serde(default)]
    pub(crate) local_copy: bool,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) error: Option<String>,
    generation: String,
    request: WebTaskRequest,
    #[serde(default)]
    nonce: Option<String>,
}

impl UploadReceipt {
    pub(crate) fn wire(&self) -> serde_json::Value {
        serde_json::json!({"id":self.id,"state":self.state,"name":self.name,"size":self.size,"localCopy":self.local_copy,"taskId":self.task_id,"error":self.error})
    }
}

impl WebTaskBackend {
    pub(crate) fn ensure_available(&self) -> Result<(), WebTaskError> {
        if lock(&self.owner.shared.queue).stopped { Err(WebTaskError::Unavailable) } else { Ok(()) }
    }

    pub(crate) fn create_receipt(
        &self,
        id: &str,
        name: &str,
        size: u64,
        request: WebTaskRequest,
    ) -> Result<UploadReceipt, WebTaskError> {
        self.ensure_available()?;
        validate_key(id)?;
        validate_display_name(name)?;
        validate_web_task_request(&request)?;
        if size > request.options.limits.max_input_bytes {
            return Err(WebTaskError::Limit("file exceeds max_input_bytes".into()));
        }
        let receipt = UploadReceipt {
            id: id.into(),
            state: "waiting".into(),
            name: name.into(),
            size,
            local_copy: false,
            task_id: None,
            error: None,
            generation: self.owner.shared.events.generation.clone(),
            request,
            nonce: None,
        };
        let json = serde_json::to_string(&receipt).map_err(|e| WebTaskError::Io(e.to_string()))?;
        let created =
            metadata_store_mutation(&self.owner.shared, STORE_MUTATION_RESERVATION, |store| {
                store.write_web_receipt(id, None, &receipt.state, None, &json).map_err(Into::into)
            })?;
        if !created {
            let store = lock(&self.owner.shared.task_store);
            let previous: UploadReceipt =
                serde_json::from_str(&store.web_receipt(id)?.ok_or(WebTaskError::NotFound)?)
                    .map_err(|e| WebTaskError::Io(e.to_string()))?;
            if previous.name != name
                || previous.size != size
                || serde_json::to_value(&previous.request).ok()
                    != serde_json::to_value(&receipt.request).ok()
            {
                return Err(WebTaskError::Conflict(
                    "submission identity already belongs to another upload".into(),
                ));
            }
            return Ok(previous);
        }
        Ok(receipt)
    }

    pub(crate) fn receipt(&self, id: &str) -> Result<UploadReceipt, WebTaskError> {
        validate_key(id)?;
        let json =
            lock(&self.owner.shared.task_store).web_receipt(id)?.ok_or(WebTaskError::NotFound)?;
        let mut receipt: UploadReceipt =
            serde_json::from_str(&json).map_err(|e| WebTaskError::Unsafe(e.to_string()))?;
        if receipt.task_id.is_none()
            && matches!(receipt.state.as_str(), "waiting" | "uploading" | "receiving")
        {
            self.ensure_available()?;
        }
        if receipt.generation != self.owner.shared.events.generation
            && receipt.task_id.is_none()
            && matches!(receipt.state.as_str(), "waiting" | "uploading" | "receiving")
        {
            self.change_receipt(&mut receipt, "interrupted", None, Some("uploadInterrupted"))?;
        }
        Ok(receipt)
    }

    pub(crate) fn change_receipt(
        &self,
        receipt: &mut UploadReceipt,
        state: &str,
        task: Option<TaskId>,
        error: Option<&str>,
    ) -> Result<(), WebTaskError> {
        let expected = receipt.state.clone();
        let mut next = receipt.clone();
        next.state = state.into();
        next.task_id = task;
        next.error = error.map(str::to_owned);
        let json = serde_json::to_string(&next).map_err(|e| WebTaskError::Io(e.to_string()))?;
        if !metadata_store_mutation(&self.owner.shared, STORE_MUTATION_RESERVATION, |store| {
            store
                .write_web_receipt(
                    &receipt.id,
                    Some(&expected),
                    state,
                    next.task_id.as_ref(),
                    &json,
                )
                .map_err(Into::into)
        })? {
            return Err(WebTaskError::Conflict("upload receipt changed".into()));
        }
        *receipt = next;
        Ok(())
    }

    pub(crate) fn receive_upload(&self, id: &str) -> Result<(UploadReceipt, Upload), WebTaskError> {
        self.ensure_available()?;
        let mut receipt = self.receipt(id)?;
        if receipt.state != "waiting" {
            return Err(WebTaskError::Conflict("upload body already received".into()));
        }
        self.change_receipt(&mut receipt, "uploading", None, None)?;
        let mut upload = self
            .begin_upload_configured(&receipt.name, Some(receipt.size), receipt.request.clone())
            .map_err(|error| {
                let _ = self.change_receipt(&mut receipt, "failed", None, Some("uploadFailed"));
                error
            })?;
        upload.receipt_id = Some(id.into());
        receipt.nonce = Some(upload.nonce.clone());
        self.change_receipt(&mut receipt, "uploading", None, None)?;
        Ok((receipt, upload))
    }

    pub(super) fn recover_receipts(&self) -> Result<(), WebTaskError> {
        let rows = lock(&self.owner.shared.task_store).pending_web_receipts()?;
        for row in rows {
            let mut receipt: UploadReceipt =
                serde_json::from_str(&row).map_err(|e| WebTaskError::Unsafe(e.to_string()))?;
            if receipt.state == "receiving" {
                if let Some(nonce) = receipt.nonce.clone() {
                    validate_key(&nonce)?;
                    if let Some(directory) = self
                        .owner
                        .shared
                        .incoming
                        .open_child_private_optional(std::ffi::OsStr::new(&nonce))
                        .map_err(|e| WebTaskError::Unsafe(e.to_string()))?
                    {
                        let file = directory
                            .open_regular_private(std::ffi::OsStr::new("payload"))
                            .map_err(|e| WebTaskError::Unsafe(e.to_string()))?;
                        validate_private_file(&file)?;
                        if file.metadata()?.len() == receipt.size {
                            receipt.generation = self.owner.shared.events.generation.clone();
                            self.change_receipt(&mut receipt, "receiving", None, None)?;
                            let upload = Upload {
                                backend: self.clone(),
                                directory: Some(directory),
                                nonce: nonce.clone(),
                                file: Some(file),
                                name: receipt.name.clone(),
                                request: receipt.request.clone(),
                                bytes: receipt.size,
                                committed: false,
                                receipt_id: Some(receipt.id.clone()),
                                retry_of: None,
                                input_hash: None,
                            };
                            if upload.finish().is_ok() {
                                continue;
                            }
                        }
                    }
                }
            }
            let _ =
                self.change_receipt(&mut receipt, "interrupted", None, Some("uploadInterrupted"));
        }
        Ok(())
    }

    pub(crate) fn cancel_receipt(&self, id: &str) -> Result<UploadReceipt, WebTaskError> {
        let mut receipt = self.receipt(id)?;
        if let Some(task) = &receipt.task_id {
            self.cancel(task)?;
        }
        if !matches!(receipt.state.as_str(), "cancelled" | "failed" | "interrupted") {
            let task = receipt.task_id.clone();
            self.change_receipt(&mut receipt, "cancelled", task, None)?;
        }
        Ok(receipt)
    }
}

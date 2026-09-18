//! Session-scoped native selection grants. Paths never cross the Web boundary.
use super::*;
use clipboard_rs::{Clipboard, ClipboardContext, common::RustImage};
use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::time::Instant as Clock;

const TTL: Duration = Duration::from_secs(30 * 60);
const MAX_SELECTIONS: usize = 1000;
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Default)]
pub(super) struct LocalSources {
    selections: Mutex<HashMap<String, Arc<Selection>>>,
    operations: Mutex<HashMap<String, (Clock, serde_json::Value)>>,
    busy: AtomicBool,
}
struct Selection {
    file: File,
    metadata: Metadata,
    name: String,
    touched: Mutex<Clock>,
    _temporary: Option<tempfile::NamedTempFile>,
}
impl LocalSources {
    pub(super) fn clear(&self) {
        self.operations.lock().unwrap().clear();
        self.selections.lock().unwrap().clear();
    }
    pub(super) fn prune(&self) {
        self.selections
            .lock()
            .unwrap()
            .retain(|_, value| value.touched.lock().unwrap().elapsed() < TTL);
        self.operations.lock().unwrap().retain(|_, (time, _)| time.elapsed() < TTL);
    }
    fn add(
        &self,
        file: File,
        name: String,
        temporary: Option<tempfile::NamedTempFile>,
    ) -> Result<serde_json::Value, &'static str> {
        let metadata = file.metadata().map_err(|_| "localFileUnavailable")?;
        if !metadata.is_file() {
            return Err("localRegularFileRequired");
        }
        let id = new_session().map_err(|_| "internal")?;
        let wire = serde_json::json!({"id":id,"name":name,"size":metadata.len()});
        let mut selections = self.selections.lock().unwrap();
        let snapshot_bytes: u64 =
            selections.values().filter(|s| s._temporary.is_some()).map(|s| s.metadata.len()).sum();
        if temporary.is_some()
            && snapshot_bytes.saturating_add(metadata.len()) > 4 * MAX_IMAGE_BYTES
        {
            return Err("resourceLimit");
        }
        if selections.len() >= MAX_SELECTIONS {
            return Err("resourceLimit");
        }
        selections.insert(
            id,
            Arc::new(Selection {
                file,
                metadata,
                name,
                touched: Mutex::new(Clock::now()),
                _temporary: temporary,
            }),
        );
        Ok(wire)
    }
    fn files(&self, paths: Vec<PathBuf>) -> Result<Vec<serde_json::Value>, &'static str> {
        if paths.len() > MAX_SELECTIONS {
            return Err("resourceLimit");
        }
        let mut added = Vec::new();
        for path in paths {
            let result = (|| {
                let name =
                    path.file_name().and_then(|s| s.to_str()).ok_or("invalidFilename")?.to_owned();
                // Open the selected object once; later namespace replacement cannot redirect it.
                if !path.is_absolute() || !std::fs::metadata(&path).is_ok_and(|meta| meta.is_file())
                {
                    return Err("localRegularFileRequired");
                }
                let mut options = std::fs::OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NONBLOCK);
                }
                let file = options.open(&path).map_err(|_| "localFileUnavailable")?;
                self.add(file, name, None)
            })();
            match result {
                Ok(value) => added.push(value),
                Err(error) => {
                    let mut selections = self.selections.lock().unwrap();
                    for item in added {
                        if let Some(id) = item["id"].as_str() {
                            selections.remove(id);
                        }
                    }
                    return Err(error);
                }
            }
        }
        Ok(added)
    }
    fn clipboard(&self) -> Result<Vec<serde_json::Value>, &'static str> {
        let clipboard = ClipboardContext::new().map_err(|_| "localSourceUnavailable")?;
        if let Ok(files) = clipboard.get_files() {
            if !files.is_empty() {
                let paths = files
                    .into_iter()
                    .map(|value| {
                        if value.starts_with("file:") {
                            let url =
                                url::Url::parse(&value).map_err(|_| "localFileUnavailable")?;
                            url.to_file_path().map_err(|_| "localFileUnavailable")
                        } else {
                            Ok(PathBuf::from(value))
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                return self.files(paths);
            }
        }
        let image = clipboard.get_image().map_err(|_| "clipboardEmpty")?;
        let (width, height) = image.get_size();
        if u64::from(width) * u64::from(height) * 4 > MAX_IMAGE_BYTES {
            return Err("resourceLimit");
        }
        let png = image.to_png().map_err(|_| "clipboardImageFailed")?;
        if png.get_bytes().len() as u64 > MAX_IMAGE_BYTES {
            return Err("resourceLimit");
        }
        let mut temporary = tempfile::NamedTempFile::new().map_err(|_| "localFileUnavailable")?;
        temporary.write_all(png.get_bytes()).map_err(|_| "localFileUnavailable")?;
        temporary.as_file().sync_all().map_err(|_| "localFileUnavailable")?;
        let file = temporary.reopen().map_err(|_| "localFileUnavailable")?;
        let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        Ok(vec![self.add(file, format!("clipboard-{time}.png"), Some(temporary))?])
    }
}
fn desktop_available() -> bool {
    !cfg!(target_os = "linux")
        || std::env::var_os("DISPLAY").is_some()
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
}
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/local-sources", get(capabilities))
        .route("/local-sources/pick", post(pick))
        .route("/local-sources/paste", post(paste))
        .route("/local-sources/operations/{id}", get(operation).delete(cancel_operation))
        .route("/local-sources/selections/{id}", axum::routing::delete(release))
}
async fn capabilities() -> Response {
    Json(serde_json::json!({"available":desktop_available()})).into_response()
}
async fn pick(State(state): State<AppState>) -> Response {
    start(state, false).await
}
async fn paste(State(state): State<AppState>) -> Response {
    start(state, true).await
}
async fn start(state: AppState, clipboard: bool) -> Response {
    if !desktop_available() {
        return rejection(StatusCode::SERVICE_UNAVAILABLE, "localSourceUnavailable");
    }
    let sources = state.local_sources.clone();
    sources.prune();
    if sources.operations.lock().unwrap().len() >= MAX_SELECTIONS {
        return rejection(StatusCode::TOO_MANY_REQUESTS, "resourceLimit");
    }
    if sources.busy.swap(true, Ordering::SeqCst) {
        return rejection(StatusCode::CONFLICT, "localSourceBusy");
    }
    let Ok(id) = new_session() else {
        sources.busy.store(false, Ordering::SeqCst);
        return rejection(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    };
    sources
        .operations
        .lock()
        .unwrap()
        .insert(id.clone(), (Clock::now(), serde_json::json!({"state":"pending"})));
    let operation_id = id.clone();
    tokio::spawn(async move {
        let result = if clipboard {
            let source = sources.clone();
            tokio::task::spawn_blocking(move || source.clipboard())
                .await
                .unwrap_or(Err("localSourceUnavailable"))
        } else {
            let source = sources.clone();
            let id = operation_id.clone();
            tokio::task::spawn_blocking(move || native_pick(&source, &id))
                .await
                .unwrap_or(Err("localSourceUnavailable"))
        };
        let mut operations = sources.operations.lock().unwrap();
        if operations.contains_key(&operation_id) {
            let value = match result {
                Ok(files) => serde_json::json!({"state":"ready","files":files}),
                Err(code) => serde_json::json!({"state":"failed","error":code}),
            };
            operations.insert(operation_id, (Clock::now(), value));
        } else if let Ok(files) = result {
            let mut selections = sources.selections.lock().unwrap();
            for file in files {
                if let Some(id) = file["id"].as_str() {
                    selections.remove(id);
                }
            }
        }
        sources.busy.store(false, Ordering::SeqCst);
    });
    (StatusCode::ACCEPTED, Json(serde_json::json!({"id":id}))).into_response()
}
async fn operation(State(state): State<AppState>, AxumPath(id): AxumPath<String>) -> Response {
    state.local_sources.prune();
    match state.local_sources.operations.lock().unwrap().get(&id) {
        Some((_, value)) => Json(value.clone()).into_response(),
        None => rejection(StatusCode::NOT_FOUND, "localSelectionExpired"),
    }
}
async fn cancel_operation(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Some((_, value)) = state.local_sources.operations.lock().unwrap().remove(&id) {
        if let Some(files) = value["files"].as_array() {
            let mut selections = state.local_sources.selections.lock().unwrap();
            for file in files {
                if let Some(id) = file["id"].as_str() {
                    selections.remove(id);
                }
            }
        }
    }
    Json(serde_json::json!({"ok":true})).into_response()
}
async fn release(State(state): State<AppState>, AxumPath(id): AxumPath<String>) -> Response {
    state.local_sources.selections.lock().unwrap().remove(&id);
    Json(serde_json::json!({"ok":true})).into_response()
}

pub(super) async fn receive(
    state: AppState,
    id: String,
    selection_id: String,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    state.local_sources.prune();
    let selection = state.local_sources.selections.lock().unwrap().get(&selection_id).cloned();
    let Some(selection) = selection else {
        return rejection(StatusCode::NOT_FOUND, "localSelectionExpired");
    };
    *selection.touched.lock().unwrap() = Clock::now();
    match start_selected(
        state.tasks.clone(),
        state.local_sources.clone(),
        id,
        selection_id,
        selection,
        permit,
    )
    .await
    {
        Ok(Ok(wire)) => (StatusCode::ACCEPTED, Json(wire)).into_response(),
        Ok(Err(response)) => response,
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}
fn start_selected(
    backend: WebTaskBackend,
    sources: Arc<LocalSources>,
    id: String,
    selection_id: String,
    selection: Arc<Selection>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> tokio::sync::oneshot::Receiver<Result<serde_json::Value, Response>> {
    let (ready, response) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let prepared = prepare_selected(&backend, &id, &selection);
        let (wire, start_copy) = match prepared {
            Ok(value) => value,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        // The owned worker continues even when the browser loses this response.
        let _ = ready.send(Ok(wire));
        if !start_copy {
            return;
        }
        let result = copy_selected(&backend, &id, &selection);
        if let Err(code) = result {
            if let Ok(mut receipt) = backend.receipt(&id) {
                if receipt.state != "cancelled" {
                    let _ = backend.change_receipt(&mut receipt, "failed", None, Some(code));
                }
            }
        }
        if result.is_ok() {
            sources.selections.lock().unwrap().remove(&selection_id);
        }
    });
    response
}
fn prepare_selected(
    backend: &WebTaskBackend,
    id: &str,
    selection: &Selection,
) -> Result<(serde_json::Value, bool), Response> {
    let mut receipt = backend.receipt(id).map_err(web_task_rejection)?;
    if receipt.name != selection.name || receipt.size != selection.metadata.len() {
        return Err(rejection(StatusCode::CONFLICT, "localSourceChanged"));
    }
    if receipt.state != "waiting" {
        return Ok((receipt.wire(), false));
    }
    receipt.local_copy = true;
    backend.change_receipt(&mut receipt, "waiting", None, None).map_err(web_task_rejection)?;
    Ok((receipt.wire(), true))
}

fn unchanged(selection: &Selection) -> bool {
    selection.file.metadata().is_ok_and(|current| {
        current.len() == selection.metadata.len()
            && current.modified().ok() == selection.metadata.modified().ok()
    })
}
fn copy_selected(
    backend: &WebTaskBackend,
    id: &str,
    selection: &Selection,
) -> Result<(), &'static str> {
    if !unchanged(selection) {
        return Err("localSourceChanged");
    }
    let (mut receipt, mut upload) = backend.receive_upload(id).map_err(|_| "uploadFailed")?;
    let mut file = selection.file.try_clone().map_err(|_| "localFileUnavailable")?;
    file.seek(SeekFrom::Start(0)).map_err(|_| "localFileUnavailable")?;
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let count = file.read(&mut buffer).map_err(|_| "localFileUnavailable")?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > receipt.size {
            return Err("localSourceChanged");
        }
        upload.write_chunk(&buffer[..count]).map_err(|error| match error {
            WebTaskError::Cancelled => "cancelled",
            WebTaskError::Limit(_) => "resourceLimit",
            WebTaskError::Unsafe(_) => "unsafeStorage",
            _ => "uploadFailed",
        })?;
    }
    if total != receipt.size || !unchanged(selection) {
        return Err("localSourceChanged");
    }
    upload.seal().map_err(|_| "uploadFailed")?;
    backend.change_receipt(&mut receipt, "receiving", None, None).map_err(|_| "uploadFailed")?;
    upload.finish().map_err(|error| match error {
        WebTaskError::Cancelled => "cancelled",
        WebTaskError::Unsafe(_) => "unsafeStorage",
        _ => "uploadFailed",
    })?;
    Ok(())
}

// A helper process gives Cocoa its main thread without blocking the HTTP runtime.
// Only this private pipe receives paths; the HTTP response contains grants.
pub(crate) fn native_picker_main() {
    let files = futures::executor::block_on(rfd::AsyncFileDialog::new().pick_files());
    let paths: Vec<PathBuf> =
        files.unwrap_or_default().into_iter().map(|file| file.path().to_path_buf()).collect();
    let _ = serde_json::to_writer(std::io::stdout().lock(), &paths);
}
fn native_pick(
    sources: &LocalSources,
    operation_id: &str,
) -> Result<Vec<serde_json::Value>, &'static str> {
    let executable = std::env::current_exe().map_err(|_| "localSourceUnavailable")?;
    let mut child = Command::new(executable)
        .arg("--internal-local-picker")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "localSourceUnavailable")?;
    let stdout = child.stdout.take().ok_or("localSourceUnavailable")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(1024 * 1024 + 1).read_to_end(&mut bytes).map(|_| bytes)
    });
    let started = Clock::now();
    let status = loop {
        if started.elapsed() >= TTL
            || !sources.operations.lock().unwrap().contains_key(operation_id)
        {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err("cancelled");
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err("localSourceUnavailable");
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| "localSourceUnavailable")?
        .map_err(|_| "localSourceUnavailable")?;
    if !status.success() || bytes.len() > 1024 * 1024 {
        return Err("localSourceUnavailable");
    }
    let paths: Vec<PathBuf> =
        serde_json::from_slice(&bytes).map_err(|_| "localSourceUnavailable")?;
    sources.files(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn slow_local_receipt_keeps_runtime_responsive_and_survives_lost_response() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("slow.txt");
        std::fs::write(&path, b"complete despite the lost response").unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let sources = Arc::new(LocalSources::default());
        let grants = sources.files(vec![path]).unwrap();
        let selection_id = grants[0]["id"].as_str().unwrap().to_owned();
        let selection = sources.selections.lock().unwrap().get(&selection_id).unwrap().clone();
        let id = "d".repeat(32);
        backend
            .create_receipt(
                &id,
                &selection.name,
                selection.metadata.len(),
                crate::web_tasks::WebTaskRequest::default(),
            )
            .unwrap();
        let (locked, acquired) = std::sync::mpsc::channel();
        let blocker = backend.clone();
        let thread = std::thread::spawn(move || {
            blocker.test_with_store_lock(|| {
                locked.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(400));
            })
        });
        acquired.recv().unwrap();
        let gate = Arc::new(Semaphore::new(1));
        let started = Clock::now();
        let response = start_selected(
            backend.clone(),
            sources.clone(),
            id.clone(),
            selection_id,
            selection,
            gate.clone().acquire_owned().await.unwrap(),
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "local import blocked the HTTP runtime"
        );
        drop(response);
        tokio::task::spawn_blocking(move || thread.join().unwrap()).await.unwrap();
        let deadline = Clock::now() + Duration::from_secs(10);
        loop {
            let receipt = backend.receipt(&id).unwrap();
            if let Some(task_id) = receipt.task_id {
                let task = backend.get(&task_id).unwrap();
                if task.status == into_markdown::TaskStatus::Succeeded {
                    break;
                }
                assert!(!matches!(
                    task.status,
                    into_markdown::TaskStatus::Failed | into_markdown::TaskStatus::Interrupted
                ));
            }
            assert!(Clock::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(gate.available_permits(), 1);
        assert!(sources.selections.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn twenty_local_csv_xlsx_batches_complete_without_manual_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let csv = temporary.path().join("本地表格.csv");
        let text = format!(
            "id,value\n{}",
            (0..1000).map(|index| format!("{index},local row {index}\n")).collect::<String>()
        );
        std::fs::write(&csv, text).unwrap();
        let xlsx =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/small/xlsx/normal.xlsx");
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let sources = Arc::new(LocalSources::default());
        let gate = Arc::new(Semaphore::new(1));
        for batch in 0..20 {
            let mut tasks = Vec::new();
            for (index, path) in [csv.clone(), xlsx.clone()].into_iter().enumerate() {
                let grants = sources.files(vec![path]).unwrap();
                let selection_id = grants[0]["id"].as_str().unwrap().to_owned();
                let selection =
                    sources.selections.lock().unwrap().get(&selection_id).unwrap().clone();
                let id = format!("{:032x}", batch * 2 + index + 1);
                backend
                    .create_receipt(
                        &id,
                        &selection.name,
                        selection.metadata.len(),
                        crate::web_tasks::WebTaskRequest::default(),
                    )
                    .unwrap();
                let response = start_selected(
                    backend.clone(),
                    sources.clone(),
                    id.clone(),
                    selection_id,
                    selection,
                    gate.clone().acquire_owned().await.unwrap(),
                );
                assert!(response.await.unwrap().is_ok());
                let deadline = Clock::now() + Duration::from_secs(10);
                loop {
                    let receipt = backend.receipt(&id).unwrap();
                    if let Some(task_id) = receipt.task_id {
                        tasks.push(task_id);
                        break;
                    }
                    assert!(!matches!(
                        receipt.state.as_str(),
                        "failed" | "cancelled" | "interrupted"
                    ));
                    assert!(Clock::now() < deadline);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            let deadline = Clock::now() + Duration::from_secs(10);
            loop {
                let records: Vec<_> = tasks.iter().map(|id| backend.get(id).unwrap()).collect();
                if records.iter().all(|task| task.status == into_markdown::TaskStatus::Succeeded) {
                    break;
                }
                assert!(records.iter().all(|task| !matches!(
                    task.status,
                    into_markdown::TaskStatus::Failed | into_markdown::TaskStatus::Interrupted
                )));
                assert!(Clock::now() < deadline);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        assert_eq!(gate.available_permits(), 1);
        assert!(sources.selections.lock().unwrap().is_empty());
    }

    #[test]
    fn grants_hide_paths_expire_and_reject_modified_sources() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("资料.txt");
        std::fs::write(&path, b"original").unwrap();
        let sources = LocalSources::default();
        let grants = sources.files(vec![path.clone()]).unwrap();
        let id = grants[0]["id"].as_str().unwrap();
        assert_eq!(grants[0]["name"], "资料.txt");
        assert!(!grants[0].to_string().contains(temporary.path().to_str().unwrap()));
        let selection = sources.selections.lock().unwrap().get(id).unwrap().clone();
        assert!(unchanged(&selection));
        std::fs::write(&path, b"changed and longer").unwrap();
        assert!(!unchanged(&selection));
        *selection.touched.lock().unwrap() = Clock::now() - TTL;
        sources.prune();
        assert!(sources.selections.lock().unwrap().is_empty());
        assert!(sources.files(vec![temporary.path().to_owned()]).is_err());
    }
    #[test]
    fn selected_file_copy_uses_the_shared_receipt_and_conversion_pipeline() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("source.txt");
        std::fs::write(&path, b"local import evidence").unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let sources = LocalSources::default();
        let grants = sources.files(vec![path]).unwrap();
        let id = grants[0]["id"].as_str().unwrap();
        let selection = sources.selections.lock().unwrap().get(id).unwrap().clone();
        let submission = "a".repeat(32);
        backend
            .create_receipt(
                &submission,
                &selection.name,
                selection.metadata.len(),
                crate::web_tasks::WebTaskRequest::default(),
            )
            .unwrap();
        copy_selected(&backend, &submission, &selection).unwrap();
        let receipt = backend.receipt(&submission).unwrap();
        assert_eq!(receipt.state, "accepted");
        assert!(receipt.task_id.is_some());
        assert_eq!(
            std::fs::read(temporary.path().join("source.txt")).unwrap(),
            b"local import evidence"
        );
    }
}

pub(super) fn schedule(state: &AppState) {
    let local_sources = Arc::downgrade(&state.local_sources);
    let mut local_shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = local_shutdown.changed() => { if let Some(sources) = local_sources.upgrade() { sources.clear(); } break; },
                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    let Some(sources) = local_sources.upgrade() else { break; };
                    sources.prune();
                }
            }
        }
    });
}

//! Loopback-only Web entry point and its security boundary.

use crate::args::UiArgs;
use crate::error::{CliError, ExitClass};
use crate::web_tasks::{
    ArtifactSnapshot, RetentionPolicy, TaskEventDto, TaskEventKind, WebTaskBackend, WebTaskError,
    WebTaskRecord, decode_web_task_request,
};
use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::StreamExt as _;
use serde::Serialize;
use std::convert::Infallible;
use std::future::{Future, IntoFuture as _};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::{Duration, Instant};

const SESSION_HEADER: HeaderName = HeaderName::from_static("x-into-md-session");
const TASK_REQUEST_HEADER: HeaderName = HeaderName::from_static("x-into-md-request");
const TASK_FILENAME_HEADER: HeaderName = HeaderName::from_static("x-into-md-filename-b64");
const SEC_FETCH_MODE_HEADER: HeaderName = HeaderName::from_static("sec-fetch-mode");
const SEC_FETCH_SITE_HEADER: HeaderName = HeaderName::from_static("sec-fetch-site");
const SESSION_FRAGMENT: &str = "into-md-session";
const SESSION_BYTES: usize = 32;
const SESSION_ENCODED_LEN: usize = 43;
#[cfg(not(test))]
const REQUEST_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const REQUEST_IDLE_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(not(test))]
const REQUEST_TOTAL_TIMEOUT: Duration = Duration::from_mins(30);
#[cfg(test)]
const REQUEST_TOTAL_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(not(test))]
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
#[cfg(test)]
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
#[cfg(not(test))]
const SSE_HEARTBEAT: Duration = Duration::from_secs(15);
#[cfg(test)]
const SSE_HEARTBEAT: Duration = Duration::from_millis(100);

#[derive(Clone)]
struct AppState {
    authority: Arc<str>,
    origin: Arc<str>,
    session: Arc<str>,
    tasks: WebTaskBackend,
    shutdown: watch::Receiver<bool>,
}

struct DownloadSlot {
    snapshot: Option<ArtifactSnapshot>,
    expired: bool,
}

struct DownloadTimerStop {
    sender: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl Drop for DownloadTimerStop {
    fn drop(&mut self) {
        if let Ok(sender) = self.sender.get_mut()
            && let Some(sender) = sender.take()
        {
            let _ = sender.send(());
        }
    }
}

fn expire_download_slot(slot: &std::sync::Mutex<DownloadSlot>) {
    let snapshot = {
        let mut slot = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.expired = true;
        slot.snapshot.take()
    };
    drop(snapshot);
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusDto {
    schema_version: u32,
    local_api: ComponentDto,
    document_console: ComponentDto,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ComponentDto {
    available: bool,
    code: &'static str,
    detail: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskListDto {
    schema_version: u32,
    tasks: Vec<WebTaskRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<TaskCursorDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskCursorDto {
    updated_at_ms: i64,
    id: into_markdown::TaskId,
}

struct TaskListQuery {
    limit: Option<u32>,
    after_updated_at_ms: Option<i64>,
    after_id: Option<String>,
    status: Option<into_markdown::TaskStatus>,
    pinned: Option<bool>,
    batch_id: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PinTaskDto {
    pinned: bool,
}

/// Opens a URL without invoking a command shell.
trait BrowserOpener: Send + Sync {
    fn kind(&self) -> &'static str;
    fn open(&self, url: &str) -> std::io::Result<()>;
}

struct SystemBrowser;

impl BrowserOpener for SystemBrowser {
    fn kind(&self) -> &'static str {
        browser_command().0
    }

    fn open(&self, url: &str) -> std::io::Result<()> {
        let (program, prefix) = browser_command();
        let status = Command::new(program)
            .args(prefix)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other("browser command returned failure"))
        }
    }
}

#[cfg(target_os = "macos")]
fn browser_command() -> (&'static str, &'static [&'static str]) {
    ("open", &[])
}

#[cfg(target_os = "windows")]
fn browser_command() -> (&'static str, &'static [&'static str]) {
    ("rundll32", &["url.dll,FileProtocolHandler"])
}

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn browser_command() -> (&'static str, &'static [&'static str]) {
    ("xdg-open", &[])
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
fn browser_command() -> (&'static str, &'static [&'static str]) {
    ("unsupported-browser-opener", &[])
}

/// Runs the CLI service until Ctrl-C.
pub async fn run_cli(
    arguments: UiArgs,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), CliError> {
    let data_dir = match arguments.data_dir {
        Some(path) => path,
        None => directories::ProjectDirs::from("org", "into-markdown", "into-markdown")
            .ok_or_else(|| CliError::component("local data directory is unavailable"))?
            .data_local_dir()
            .join("ui"),
    };
    prepare_data_dir(&data_dir)?;
    let tasks = WebTaskBackend::open(data_dir.join("tasks"))
        .map_err(|error| CliError::new(ExitClass::Io, "uiTaskBackendFailed", error.to_string()))?;

    let listener = bind_loopback(arguments.port).await?;
    let address = listener.local_addr()?;
    let session = new_session()?;
    let origin = format!("http://127.0.0.1:{}", address.port());
    let launch_url = format!("{origin}/#{SESSION_FRAGMENT}={session}");
    announce_and_open(&SystemBrowser, arguments.no_open, &origin, &launch_url, stdout, stderr)?;

    serve(listener, session, tasks, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

fn announce_and_open(
    opener: &dyn BrowserOpener,
    no_open: bool,
    origin: &str,
    launch_url: &str,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), CliError> {
    writeln!(stdout, "into-md local service: {origin}/")?;
    if no_open {
        // This is an explicit user handoff, not an access or diagnostic log.
        writeln!(stdout, "open this private session URL: {launch_url}")?;
    } else if opener.open(launch_url).is_err() {
        writeln!(
            stderr,
            "into-md: browser launch failed ({}); local service is still running",
            opener.kind()
        )?;
        writeln!(stdout, "open this private session URL: {launch_url}")?;
    }
    Ok(())
}

async fn bind_loopback(port: u16) -> Result<TcpListener, CliError> {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await.map_err(|error| {
        CliError::new(ExitClass::Io, "uiBindFailed", format!("bind 127.0.0.1:{port}: {error}"))
    })
}

async fn serve<F>(
    listener: TcpListener,
    session: String,
    tasks: WebTaskBackend,
    shutdown: F,
) -> Result<(), CliError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let address = listener.local_addr()?;
    if address.ip() != Ipv4Addr::LOCALHOST {
        return Err(CliError::internal("local Web listener is not IPv4 loopback"));
    }
    let authority: Arc<str> = format!("127.0.0.1:{}", address.port()).into();
    let origin: Arc<str> = format!("http://{authority}").into();
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let state = AppState {
        authority,
        origin,
        session: session.into(),
        tasks,
        shutdown: shutdown_receiver.clone(),
    };
    let api = Router::new()
        .route("/status", post(status).fallback(api_method_not_allowed))
        .route("/tasks", get(list_tasks).post(upload_task).fallback(api_method_not_allowed))
        .route("/tasks/{id}", get(task_status).delete(cancel_task).fallback(api_method_not_allowed))
        .route("/tasks/{id}/cancel", post(cancel_task).fallback(api_method_not_allowed))
        .route("/tasks/{id}/retry", post(retry_task).fallback(api_method_not_allowed))
        .route("/tasks/{id}/pin", post(pin_task).fallback(api_method_not_allowed))
        .route(
            "/tasks/{id}/history",
            axum::routing::delete(delete_task).fallback(api_method_not_allowed),
        )
        .route("/tasks/cleanup", post(cleanup_tasks).fallback(api_method_not_allowed))
        .route("/tasks/{id}/events", get(task_events).fallback(api_method_not_allowed))
        .route(
            "/tasks/{id}/artifacts/{key}",
            get(download_artifact).fallback(api_method_not_allowed),
        )
        .fallback(api_not_found)
        .layer(middleware::from_fn_with_state(state.clone(), api_security));
    let app = Router::new()
        .route("/", get(index))
        .route("/status", get(index))
        .route("/assets/{*path}", get(asset))
        .nest("/api", api)
        .fallback(static_fallback)
        .layer(middleware::from_fn_with_state(state.clone(), host_security))
        .layer(middleware::from_fn(response_security))
        .with_state(state);
    let mut graceful_receiver = shutdown_receiver;
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            while !*graceful_receiver.borrow() {
                if graceful_receiver.changed().await.is_err() {
                    break;
                }
            }
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result
            .map_err(|error| CliError::new(ExitClass::Io, "uiServeFailed", error.to_string())),
        () = shutdown => {
            let _ = shutdown_sender.send(true);
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
                Ok(result) => result.map_err(|error| {
                    CliError::new(ExitClass::Io, "uiServeFailed", error.to_string())
                }),
                Err(_) => Ok(()),
            }
        }
    }
}

async fn host_security(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if single_ascii_header(request.headers(), header::HOST) != Some(state.authority.as_ref()) {
        return rejection(StatusCode::BAD_REQUEST, "invalidHost");
    }
    next.run(request).await
}

async fn response_security(request: Request, next: Next) -> Response {
    apply_security_headers(next.run(request).await)
}

async fn api_security(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if !api_origin_matches(&state, &request) {
        return rejection(StatusCode::FORBIDDEN, "invalidOrigin");
    }
    let supplied = single_ascii_header(request.headers(), SESSION_HEADER.clone()).unwrap_or("");
    if !session_matches(state.session.as_bytes(), supplied.as_bytes()) {
        return rejection(StatusCode::UNAUTHORIZED, "invalidSession");
    }
    next.run(request).await
}

fn api_origin_matches(state: &AppState, request: &Request) -> bool {
    match single_ascii_header(request.headers(), header::ORIGIN) {
        Some(origin) => origin == state.origin.as_ref(),
        None => {
            matches!(*request.method(), Method::GET | Method::HEAD)
                && single_ascii_header(request.headers(), SEC_FETCH_SITE_HEADER)
                    == Some("same-origin")
                && single_ascii_header(request.headers(), SEC_FETCH_MODE_HEADER) == Some("cors")
        }
    }
}

fn single_ascii_header(headers: &HeaderMap, name: HeaderName) -> Option<&str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let text = value.to_str().ok()?;
    (text.is_ascii() && !text.contains(',')).then_some(text)
}

fn session_matches(expected: &[u8], supplied: &[u8]) -> bool {
    if expected.len() != SESSION_ENCODED_LEN || supplied.len() != SESSION_ENCODED_LEN {
        return false;
    }
    let mut difference = 0_u8;
    for index in 0..SESSION_ENCODED_LEN {
        difference |= expected[index] ^ supplied[index];
    }
    difference == 0
}

async fn index() -> impl IntoResponse {
    asset_response(&crate::ui_assets::INDEX)
}

async fn asset(uri: Uri) -> Response {
    match crate::ui_assets::by_path(uri.path()) {
        Some(asset) => asset_response(asset),
        None => rejection(StatusCode::NOT_FOUND, "notFound"),
    }
}

fn asset_response(asset: &crate::ui_assets::Asset) -> Response {
    let mut response = Response::new(Body::from(asset.bytes));
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.mime));
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", asset.sha256))
            .expect("checked-in SHA-256 is a valid header value"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if asset.immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-store"
        }),
    );
    response
}

async fn static_fallback(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let accepts_html =
        headers.get_all(header::ACCEPT).iter().filter_map(|value| value.to_str().ok()).any(
            |value| {
                value.split(',').any(|media| media.trim().split(';').next() == Some("text/html"))
            },
        );
    if matches!(method, Method::GET | Method::HEAD)
        && accepts_html
        && !uri.path().starts_with("/api/")
        && !uri.path().starts_with("/assets/")
    {
        asset_response(&crate::ui_assets::INDEX)
    } else {
        rejection(StatusCode::NOT_FOUND, "notFound")
    }
}

async fn status(headers: HeaderMap) -> Response {
    if headers.contains_key(header::CONTENT_TYPE) {
        return rejection(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unexpectedContentType");
    }
    if !request_body_is_empty(&headers) {
        return rejection(StatusCode::BAD_REQUEST, "requestBodyNotAllowed");
    }
    Json(StatusDto {
        schema_version: 1,
        local_api: ComponentDto {
            available: true,
            code: "available",
            detail: "loopback API security boundary is active",
        },
        document_console: ComponentDto {
            available: true,
            code: "available",
            detail: "local upload and conversion workbench is active",
        },
    })
    .into_response()
}

async fn upload_task(State(state): State<AppState>, request: Request) -> Response {
    let deadline = Instant::now() + REQUEST_TOTAL_TIMEOUT;
    let mut shutdown = state.shutdown.clone();
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let name = if let Some(encoded) =
        single_ascii_header(request.headers(), TASK_FILENAME_HEADER.clone())
    {
        match URL_SAFE_NO_PAD.decode(encoded).ok().and_then(|bytes| String::from_utf8(bytes).ok()) {
            Some(name) => name,
            None => return rejection(StatusCode::BAD_REQUEST, "invalidFilename"),
        }
    } else if let Some(name) =
        single_ascii_header(request.headers(), HeaderName::from_static("x-into-md-filename"))
    {
        name.to_owned()
    } else {
        return rejection(StatusCode::BAD_REQUEST, "missingFilename");
    };
    let task_request = match single_ascii_header(request.headers(), TASK_REQUEST_HEADER.clone()) {
        None => crate::web_tasks::WebTaskRequest::default(),
        Some(encoded) => {
            let decoded = match URL_SAFE_NO_PAD.decode(encoded) {
                Ok(decoded) => decoded,
                Err(_) => return rejection(StatusCode::BAD_REQUEST, "invalidTaskOptions"),
            };
            match decode_web_task_request(&decoded) {
                Ok(request) => request,
                Err(error) => return web_task_rejection(error),
            }
        }
    };
    let backend = state.tasks.clone();
    let response_backend = state.tasks.clone();
    let mut upload = match tokio::task::spawn_blocking(move || {
        backend.begin_upload_configured(&name, declared, task_request)
    })
    .await
    {
        Ok(Ok(upload)) => upload,
        Ok(Err(error)) => return web_task_rejection(error),
        Err(_) => return rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    };
    let mut stream = request.into_body().into_data_stream();
    loop {
        let chunk = tokio::select! {
            changed = shutdown.changed() => {
                let _ = changed;
                return rejection(StatusCode::SERVICE_UNAVAILABLE, "shuttingDown");
            }
            chunk = tokio::time::timeout_at(
                deadline.min(Instant::now() + REQUEST_IDLE_TIMEOUT),
                stream.next(),
            ) => match chunk {
                Ok(chunk) => chunk,
                Err(_) => return rejection(StatusCode::REQUEST_TIMEOUT, "uploadTimeout"),
            },
        };
        let Some(chunk) = chunk else { break };
        let Ok(chunk) = chunk else {
            return rejection(StatusCode::BAD_REQUEST, "uploadDisconnected");
        };
        upload = match tokio::task::spawn_blocking(move || {
            upload.write_chunk(&chunk)?;
            Ok::<_, WebTaskError>(upload)
        })
        .await
        {
            Ok(Ok(upload)) => upload,
            Ok(Err(error)) => return web_task_rejection(error),
            Err(_) => return rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
        };
    }
    let finish = tokio::task::spawn_blocking(move || upload.finish());
    let finished = tokio::select! {
        changed = shutdown.changed() => {
            let _ = changed;
            return rejection(StatusCode::SERVICE_UNAVAILABLE, "shuttingDown");
        }
        result = tokio::time::timeout_at(deadline, finish) => match result {
            Ok(result) => result,
            Err(_) => return rejection(StatusCode::REQUEST_TIMEOUT, "uploadTimeout"),
        },
    };
    match finished {
        Ok(Ok(record)) => match response_backend.web_record(record) {
            Ok(record) => (StatusCode::ACCEPTED, Json(record)).into_response(),
            Err(error) => web_task_rejection(error),
        },
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}

async fn list_tasks(State(state): State<AppState>, request: Request) -> Response {
    let Ok(query) = parse_task_list_query(request.uri().query()) else {
        return rejection(StatusCode::BAD_REQUEST, "invalidHistoryQuery");
    };
    let headers = request.headers();
    if headers.contains_key(header::CONTENT_TYPE) || !request_body_is_empty(headers) {
        return rejection(StatusCode::BAD_REQUEST, "requestBodyNotAllowed");
    }
    let after = match (query.after_updated_at_ms, query.after_id) {
        (None, None) => None,
        (Some(updated_at_ms), Some(id)) => match into_markdown::TaskId::parse(id) {
            Ok(id) => Some(into_markdown::TaskCursor { updated_at_ms, id }),
            Err(_) => return rejection(StatusCode::BAD_REQUEST, "invalidCursor"),
        },
        _ => return rejection(StatusCode::BAD_REQUEST, "invalidCursor"),
    };
    match tokio::task::spawn_blocking(move || {
        let backend = state.tasks;
        let page = match query.batch_id.as_deref() {
            Some(batch_id) => backend.list_batch(
                query.limit.unwrap_or(25),
                after.as_ref(),
                query.status,
                query.pinned,
                batch_id,
            )?,
            None => backend.list(
                query.limit.unwrap_or(25),
                after.as_ref(),
                query.status,
                query.pinned,
            )?,
        };
        let tasks = page
            .tasks
            .into_iter()
            .map(|record| backend.web_record(record))
            .collect::<Result<Vec<_>, _>>()?;
        Ok::<_, WebTaskError>((tasks, page.next))
    })
    .await
    {
        Ok(Ok((tasks, next))) => Json(TaskListDto {
            schema_version: 1,
            tasks,
            next_cursor: next
                .map(|cursor| TaskCursorDto { updated_at_ms: cursor.updated_at_ms, id: cursor.id }),
        })
        .into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}

fn parse_task_list_query(value: Option<&str>) -> Result<TaskListQuery, ()> {
    let mut query = TaskListQuery {
        limit: None,
        after_updated_at_ms: None,
        after_id: None,
        status: None,
        pinned: None,
        batch_id: None,
    };
    let Some(value) = value else { return Ok(query) };
    for field in value.split('&') {
        let (name, value) = field.split_once('=').ok_or(())?;
        if value.is_empty() || value.contains('%') || value.contains('+') {
            return Err(());
        }
        match name {
            "limit" if query.limit.is_none() => query.limit = Some(value.parse().map_err(|_| ())?),
            "afterUpdatedAtMs" if query.after_updated_at_ms.is_none() => {
                query.after_updated_at_ms = Some(value.parse().map_err(|_| ())?);
            }
            "afterId" if query.after_id.is_none() => query.after_id = Some(value.to_owned()),
            "pinned" if query.pinned.is_none() => {
                query.pinned = Some(match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err(()),
                });
            }
            "batchId"
                if query.batch_id.is_none()
                    && value.len() == 32
                    && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && !value.bytes().any(|byte| byte.is_ascii_uppercase()) =>
            {
                query.batch_id = Some(value.to_owned());
            }
            "status" if query.status.is_none() => {
                query.status = Some(match value {
                    "pending" => into_markdown::TaskStatus::Pending,
                    "running" => into_markdown::TaskStatus::Running,
                    "converted" => into_markdown::TaskStatus::Converted,
                    "succeeded" => into_markdown::TaskStatus::Succeeded,
                    "failed" => into_markdown::TaskStatus::Failed,
                    "interrupted" => into_markdown::TaskStatus::Interrupted,
                    "cancelled" => into_markdown::TaskStatus::Cancelled,
                    _ => return Err(()),
                });
            }
            _ => return Err(()),
        }
    }
    Ok(query)
}

async fn task_status(State(state): State<AppState>, AxumPath(id): AxumPath<String>) -> Response {
    let Ok(id) = into_markdown::TaskId::parse(id) else {
        return rejection(StatusCode::BAD_REQUEST, "invalidTaskId");
    };
    match tokio::task::spawn_blocking(move || {
        let record = state.tasks.get(&id)?;
        state.tasks.web_record(record)
    })
    .await
    {
        Ok(Ok(record)) => Json(record).into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}

async fn cancel_task(State(state): State<AppState>, AxumPath(id): AxumPath<String>) -> Response {
    let Ok(id) = into_markdown::TaskId::parse(id) else {
        return rejection(StatusCode::BAD_REQUEST, "invalidTaskId");
    };
    match tokio::task::spawn_blocking(move || {
        let record = state.tasks.cancel(&id)?;
        state.tasks.web_record(record)
    })
    .await
    {
        Ok(Ok(record)) => Json(record).into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}

async fn retry_task(State(state): State<AppState>, AxumPath(id): AxumPath<String>) -> Response {
    let Ok(id) = into_markdown::TaskId::parse(id) else {
        return rejection(StatusCode::BAD_REQUEST, "invalidTaskId");
    };
    match tokio::task::spawn_blocking(move || {
        let record = state.tasks.retry(&id)?;
        state.tasks.web_record(record)
    })
    .await
    {
        Ok(Ok(record)) => (StatusCode::ACCEPTED, Json(record)).into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}

async fn pin_task(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    request: Request,
) -> Response {
    let Ok(id) = into_markdown::TaskId::parse(id) else {
        return rejection(StatusCode::BAD_REQUEST, "invalidTaskId");
    };
    if request.headers().get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok())
        != Some("application/json")
    {
        return rejection(StatusCode::UNSUPPORTED_MEDIA_TYPE, "invalidContentType");
    }
    let bytes = match axum::body::to_bytes(request.into_body(), 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return rejection(StatusCode::BAD_REQUEST, "invalidPinRequest"),
    };
    let request: PinTaskDto = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(_) => return rejection(StatusCode::BAD_REQUEST, "invalidPinRequest"),
    };
    match tokio::task::spawn_blocking(move || {
        let record = state.tasks.set_pinned(&id, request.pinned)?;
        state.tasks.web_record(record)
    })
    .await
    {
        Ok(Ok(record)) => Json(record).into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}

async fn delete_task(State(state): State<AppState>, AxumPath(id): AxumPath<String>) -> Response {
    let Ok(id) = into_markdown::TaskId::parse(id) else {
        return rejection(StatusCode::BAD_REQUEST, "invalidTaskId");
    };
    match tokio::task::spawn_blocking(move || state.tasks.delete(&id)).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}

async fn cleanup_tasks(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if headers.contains_key(header::CONTENT_TYPE) || !request_body_is_empty(&headers) {
        return rejection(StatusCode::BAD_REQUEST, "requestBodyNotAllowed");
    }
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(value) => i64::try_from(value.as_millis()).unwrap_or(i64::MAX),
        Err(_) => return rejection(StatusCode::INTERNAL_SERVER_ERROR, "invalidSystemClock"),
    };
    match tokio::task::spawn_blocking(move || state.tasks.cleanup(RetentionPolicy::default(), now))
        .await
    {
        Ok(Ok(summary)) => Json(summary).into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}

fn last_event_cursor(headers: &HeaderMap) -> Result<Option<(String, u64)>, ()> {
    let name = HeaderName::from_static("last-event-id");
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else { return Ok(None) };
    if values.next().is_some() {
        return Err(());
    }
    let value = value.to_str().map_err(|_| ())?;
    let (generation, sequence) = value.split_once(':').ok_or(())?;
    if generation.len() != 32
        || !generation.bytes().all(|byte| byte.is_ascii_hexdigit())
        || generation.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        return Err(());
    }
    let sequence = sequence.parse::<u64>().map_err(|_| ())?;
    if sequence == 0 {
        return Err(());
    }
    Ok(Some((generation.to_owned(), sequence)))
}

fn sse_event(event: &TaskEventDto) -> Event {
    let kind = match event.kind {
        TaskEventKind::Snapshot => "snapshot",
        TaskEventKind::Progress => "progress",
    };
    let data = serde_json::to_string(event)
        .unwrap_or_else(|_| "{\"schemaVersion\":1,\"kind\":\"serializationError\"}".into());
    Event::default().event(kind).id(event.event_id.clone()).data(data)
}

async fn task_events(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let Ok(id) = into_markdown::TaskId::parse(id) else {
        return rejection(StatusCode::BAD_REQUEST, "invalidTaskId");
    };
    let Ok(cursor) = last_event_cursor(&headers) else {
        return rejection(StatusCode::BAD_REQUEST, "invalidLastEventId");
    };
    let backend = state.tasks.clone();
    let subscription_backend = backend.clone();
    let subscription_id = id.clone();
    let subscription = tokio::task::spawn_blocking(move || {
        subscription_backend.events(
            &subscription_id,
            cursor.as_ref().map(|(generation, sequence)| (generation.as_str(), *sequence)),
        )
    })
    .await;
    let subscription = match subscription {
        Ok(Ok(subscription)) => subscription,
        Ok(Err(error)) => return web_task_rejection(error),
        Err(_) => return rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    };
    let shutdown = state.shutdown.clone();
    let stream = futures::stream::unfold(
        (id, backend, subscription, shutdown),
        |(id, backend, mut subscription, mut shutdown)| async move {
            loop {
                if let Some(event) = subscription.replay.pop_front() {
                    return Some((
                        Ok::<Event, Infallible>(sse_event(&event)),
                        (id, backend, subscription, shutdown),
                    ));
                }
                tokio::select! {
                    changed = shutdown.changed() => {
                        let _ = changed;
                        return None;
                    }
                    received = subscription.receiver.recv() => match received {
                        Ok(event) if event.task_id == id => {
                            return Some((
                                Ok::<Event, Infallible>(sse_event(&event)),
                                (id, backend, subscription, shutdown),
                            ));
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let fresh_backend = backend.clone();
                            let fresh_id = id.clone();
                            match tokio::task::spawn_blocking(move || fresh_backend.events(&fresh_id, None)).await {
                                Ok(Ok(fresh)) => subscription = fresh,
                                _ => return None,
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    }
                }
            }
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(SSE_HEARTBEAT).text("heartbeat"))
        .into_response()
}

async fn download_artifact(
    State(state): State<AppState>,
    AxumPath((id, key)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Ok(id) = into_markdown::TaskId::parse(id) else {
        return rejection(StatusCode::BAD_REQUEST, "invalidTaskId");
    };
    let mut shutdown = state.shutdown.clone();
    let deadline = Instant::now() + REQUEST_TOTAL_TIMEOUT;
    let (mut file, reference) =
        match tokio::task::spawn_blocking(move || state.tasks.artifact(&id, &key)).await {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return web_task_rejection(error),
            Err(_) => return rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
        };
    let range = match requested_range(&headers, reference.byte_len) {
        Ok(range) => range,
        Err(()) => {
            let mut response = rejection(StatusCode::RANGE_NOT_SATISFIABLE, "invalidRange");
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{}", reference.byte_len))
                    .expect("bounded artifact length is a valid header"),
            );
            return response;
        }
    };
    let (start, end) = range.unwrap_or_else(|| (0, reference.byte_len.saturating_sub(1)));
    if start > 0 && std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start)).is_err() {
        return rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendIo");
    }
    let response_bytes = if reference.byte_len == 0 { 0 } else { end - start + 1 };
    let slot =
        Arc::new(std::sync::Mutex::new(DownloadSlot { snapshot: Some(file), expired: false }));
    let (timer_sender, timer_receiver) = tokio::sync::oneshot::channel();
    let timer_stop =
        Arc::new(DownloadTimerStop { sender: std::sync::Mutex::new(Some(timer_sender)) });
    let timer_slot = Arc::clone(&slot);
    tokio::spawn(async move {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => {}
            () = async {
                if !*shutdown.borrow() {
                    let _ = shutdown.changed().await;
                }
            } => {}
            _ = timer_receiver => {}
        }
        expire_download_slot(&timer_slot);
    });
    let stream = futures::stream::try_unfold(
        (slot, timer_stop, response_bytes),
        |(slot, timer_stop, remaining)| async move {
            if remaining == 0 {
                return Ok(None);
            }
            let snapshot = {
                let mut slot_guard = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if slot_guard.expired {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "download deadline expired",
                    ));
                }
                slot_guard.snapshot.take().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "download snapshot is unavailable",
                    )
                })?
            };
            let read = tokio::task::spawn_blocking(move || {
                let mut snapshot = snapshot;
                let mut bytes = Vec::new();
                let chunk = usize::try_from(remaining.min(64 * 1024)).unwrap_or(64 * 1024);
                bytes.try_reserve_exact(chunk).map_err(std::io::Error::other)?;
                bytes.resize(chunk, 0);
                let read = std::io::Read::read(&mut snapshot, &mut bytes)?;
                bytes.truncate(read);
                Ok::<_, std::io::Error>((bytes, snapshot))
            })
            .await
            .map_err(std::io::Error::other)?;
            let (bytes, snapshot) = read?;
            {
                let mut slot_guard = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if slot_guard.expired {
                    drop(slot_guard);
                    drop(snapshot);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "download deadline expired",
                    ));
                }
                if bytes.is_empty() {
                    drop(slot_guard);
                    drop(snapshot);
                    return Ok(None);
                }
                slot_guard.snapshot = Some(snapshot);
            }
            let remaining = remaining.saturating_sub(bytes.len() as u64);
            Ok(Some((Bytes::from(bytes), (slot, timer_stop, remaining))))
        },
    );
    let content_type = artifact_content_type(&reference);
    let mut response = Body::from_stream(stream).into_response();
    if range.is_some() {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{}", reference.byte_len))
                .expect("bounded artifact range is a valid header"),
        );
    }
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(disposition) = HeaderValue::from_str(&content_disposition(&reference)) {
        response.headers_mut().insert(header::CONTENT_DISPOSITION, disposition);
    }
    if let Ok(length) = HeaderValue::from_str(&response_bytes.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, length);
    }
    response
}

fn artifact_content_type(reference: &into_markdown::ArtifactReference) -> &str {
    match reference.kind {
        into_markdown::ArtifactKind::Markdown => "text/markdown; charset=utf-8",
        into_markdown::ArtifactKind::DocumentIr | into_markdown::ArtifactKind::Diagnostics => {
            "application/json"
        }
        into_markdown::ArtifactKind::Bundle => "application/zip",
        into_markdown::ArtifactKind::Asset => {
            reference.media_type.as_deref().unwrap_or("application/octet-stream")
        }
    }
}

fn requested_range(headers: &HeaderMap, length: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(value) = single_ascii_header(headers, header::RANGE) else {
        return if headers.contains_key(header::RANGE) { Err(()) } else { Ok(None) };
    };
    let value = value.strip_prefix("bytes=").ok_or(())?;
    if value.contains(',') || length == 0 {
        return Err(());
    }
    let (left, right) = value.split_once('-').ok_or(())?;
    let (start, end) = if left.is_empty() {
        let suffix = right.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        (length.saturating_sub(suffix), length - 1)
    } else {
        let start = left.parse::<u64>().map_err(|_| ())?;
        if start >= length {
            return Err(());
        }
        let end = if right.is_empty() {
            length - 1
        } else {
            right.parse::<u64>().map_err(|_| ())?.min(length - 1)
        };
        if end < start {
            return Err(());
        }
        (start, end)
    };
    Ok(Some((start, end)))
}

fn artifact_filename(reference: &into_markdown::ArtifactReference) -> &str {
    match reference.kind {
        into_markdown::ArtifactKind::Markdown => "result.md",
        into_markdown::ArtifactKind::DocumentIr => "document-ir.json",
        into_markdown::ArtifactKind::Diagnostics => "diagnostics.json",
        into_markdown::ArtifactKind::Bundle => "result.zip",
        into_markdown::ArtifactKind::Asset => reference.filename.as_deref().unwrap_or("asset.bin"),
    }
}

fn content_disposition(reference: &into_markdown::ArtifactReference) -> String {
    let filename = artifact_filename(reference);
    let fallback: String = filename
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || ".-_".contains(character) {
                character
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    let mut encoded = String::new();
    for byte in filename.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            encoded.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

#[allow(clippy::needless_pass_by_value)]
fn web_task_rejection(error: WebTaskError) -> Response {
    let (status, code) = match error {
        WebTaskError::Unsafe(_) => (StatusCode::BAD_REQUEST, "unsafeStorage"),
        WebTaskError::Limit(_) => (StatusCode::PAYLOAD_TOO_LARGE, "resourceLimit"),
        WebTaskError::Cancelled => (StatusCode::CONFLICT, "cancelled"),
        WebTaskError::NotFound => (StatusCode::NOT_FOUND, "notFound"),
        WebTaskError::Conflict(_) => (StatusCode::CONFLICT, "taskConflict"),
        WebTaskError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalidTaskOptions"),
        WebTaskError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "backendIo"),
    };
    rejection(status, code)
}

fn request_body_is_empty(headers: &HeaderMap) -> bool {
    if headers.contains_key(header::TRANSFER_ENCODING) {
        return false;
    }
    let mut lengths = headers.get_all(header::CONTENT_LENGTH).iter();
    match (lengths.next(), lengths.next()) {
        (None, None) => true,
        (Some(value), None) => value.as_bytes() == b"0",
        _ => false,
    }
}

async fn api_not_found() -> Response {
    rejection(StatusCode::NOT_FOUND, "apiNotFound")
}

async fn api_method_not_allowed() -> Response {
    rejection(StatusCode::METHOD_NOT_ALLOWED, "methodNotAllowed")
}

fn rejection(status: StatusCode, code: &'static str) -> Response {
    (status, Json(serde_json::json!({"schemaVersion": 1, "code": code}))).into_response()
}

fn apply_security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"),
    );
    response
}

fn new_session() -> Result<String, CliError> {
    let mut bytes = [0_u8; SESSION_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| CliError::component(format!("generate local Web session: {error}")))?;
    let encoded = URL_SAFE_NO_PAD.encode(bytes);
    if encoded.len() != SESSION_ENCODED_LEN {
        return Err(CliError::internal("local Web session encoding has an invalid length"));
    }
    Ok(encoded)
}

fn normalized_absolute(path: &Path) -> Result<PathBuf, CliError> {
    let path =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(unsafe_data_dir("data directory escapes its filesystem root"));
                }
            }
        }
    }
    Ok(normalized)
}

#[cfg(unix)]
fn prepare_data_dir(path: &Path) -> Result<PathBuf, CliError> {
    use std::fs::File;
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::PermissionsExt as _;

    let target = normalized_absolute(path)?;
    let mut fd: OwnedFd = rustix::fs::open(
        "/",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    let components = target
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    if components.is_empty() {
        return Err(unsafe_data_dir("filesystem root cannot be used as the data directory"));
    }
    for (index, name) in components.iter().enumerate() {
        let opened = rustix::fs::openat(
            &fd,
            *name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        );
        fd = match opened {
            Ok(opened) => opened,
            Err(rustix::io::Errno::NOENT) => {
                rustix::fs::mkdirat(
                    &fd,
                    *name,
                    rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
                )
                .map_err(|error| {
                    unsafe_data_dir(format!("create private data directory: {error}"))
                })?;
                rustix::fs::fsync(&fd)?;
                rustix::fs::openat(
                    &fd,
                    *name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map_err(|error| {
                    unsafe_data_dir(format!("verify private data directory: {error}"))
                })?
            }
            Err(error) => {
                return Err(unsafe_data_dir(format!(
                    "open data directory without following links: {error}"
                )));
            }
        };
        if index + 1 == components.len() {
            let metadata = File::from(rustix::io::dup(&fd)?).metadata()?;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(unsafe_data_dir(
                    "data directory permissions must deny group and other access",
                ));
            }
        }
    }
    Ok(target)
}

#[cfg(windows)]
fn prepare_data_dir(path: &Path) -> Result<PathBuf, CliError> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let target = normalized_absolute(path)?;
    let mut current = PathBuf::new();
    let mut identities = Vec::new();
    for component in target.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if !metadata.is_dir()
                    || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                {
                    return Err(unsafe_data_dir(
                        "data directory contains a reparse point or non-directory",
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?;
                let metadata = std::fs::symlink_metadata(&current)?;
                if !metadata.is_dir()
                    || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                {
                    return Err(unsafe_data_dir(
                        "created data directory is not a private physical directory",
                    ));
                }
            }
            Err(error) => return Err(error.into()),
        }
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(&current)?;
        let information = winapi_util::file::information(&handle)?;
        if information.file_attributes() & u64::from(FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return Err(unsafe_data_dir("data directory contains a reparse point"));
        }
        identities.push((
            current.clone(),
            information.volume_serial_number(),
            information.file_index(),
            handle,
        ));
    }
    // Reopen every component and compare physical identity after creation. No
    // state mutation occurs after these authenticated handles are released.
    for (path, volume, index, _held) in &identities {
        let reopened = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let information = winapi_util::file::information(&reopened)?;
        if information.file_attributes() & u64::from(FILE_ATTRIBUTE_REPARSE_POINT) != 0
            || information.volume_serial_number() != *volume
            || information.file_index() != *index
        {
            return Err(unsafe_data_dir("data directory identity changed during verification"));
        }
    }
    Ok(target)
}

fn unsafe_data_dir(message: impl Into<String>) -> CliError {
    CliError::new(ExitClass::Policy, "unsafeDataDirectory", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::sync::oneshot;

    async fn request(port: u16, request: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    async fn start() -> (
        tempfile::TempDir,
        u16,
        String,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<(), CliError>>,
    ) {
        let listener = bind_loopback(0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let session = new_session().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(directory.path().join("backend")).unwrap();
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(serve(listener, session.clone(), backend, async {
            let _ = receiver.await;
        }));
        (directory, port, session, sender, task)
    }

    fn assert_security_headers(response: &str) {
        let lowercase = response.to_ascii_lowercase();
        assert!(lowercase.contains("cache-control: no-store\r\n"), "{response}");
        assert!(lowercase.contains("referrer-policy: no-referrer\r\n"), "{response}");
        assert!(lowercase.contains("x-content-type-options: nosniff\r\n"), "{response}");
        assert!(lowercase.contains("content-security-policy: default-src 'none';"), "{response}");
    }

    fn assert_schema_error(response: &str, status: &str, code: &str) {
        assert!(response.starts_with(status), "{response}");
        assert!(response.contains("\"schemaVersion\":1"), "{response}");
        assert!(response.contains(&format!("\"code\":\"{code}\"")), "{response}");
        assert_security_headers(response);
    }

    #[test]
    fn last_event_id_requires_a_canonical_generation_and_positive_sequence() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "last-event-id",
            HeaderValue::from_static("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:42"),
        );
        assert_eq!(
            last_event_cursor(&headers).unwrap(),
            Some(("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(), 42))
        );
        for invalid in [
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:0",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA:1",
            "short:1",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:not-a-number",
        ] {
            headers.insert("last-event-id", HeaderValue::from_str(invalid).unwrap());
            assert!(last_event_cursor(&headers).is_err(), "accepted {invalid}");
        }
        headers.append(
            "last-event-id",
            HeaderValue::from_static("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb:2"),
        );
        assert!(last_event_cursor(&headers).is_err());
    }

    #[test]
    fn history_query_requires_bounded_canonical_filters_and_cursor_pairs() {
        let id = "a".repeat(32);
        let query = parse_task_list_query(Some(&format!(
            "limit=25&afterUpdatedAtMs=42&afterId={id}&status=succeeded&pinned=true&batchId={id}"
        )))
        .unwrap();
        assert_eq!(query.limit, Some(25));
        assert_eq!(query.after_updated_at_ms, Some(42));
        assert_eq!(query.after_id.as_deref(), Some(id.as_str()));
        assert_eq!(query.status, Some(into_markdown::TaskStatus::Succeeded));
        assert_eq!(query.pinned, Some(true));
        assert_eq!(query.batch_id.as_deref(), Some(id.as_str()));
        for invalid in [
            "unknown=x",
            "limit=1&limit=2",
            "status=unknown",
            "pinned=1",
            "afterId=%2e%2e",
            "batchId=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "batchId=short",
        ] {
            assert!(parse_task_list_query(Some(invalid)).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn download_ranges_and_names_are_canonical_and_header_safe() {
        let mut headers = HeaderMap::new();
        assert_eq!(requested_range(&headers, 10), Ok(None));
        for (value, expected) in [
            ("bytes=2-5", (2, 5)),
            ("bytes=7-", (7, 9)),
            ("bytes=-3", (7, 9)),
            ("bytes=0-99", (0, 9)),
        ] {
            headers.insert(header::RANGE, HeaderValue::from_str(value).unwrap());
            assert_eq!(requested_range(&headers, 10), Ok(Some(expected)), "{value}");
        }
        for invalid in ["items=0-1", "bytes=", "bytes=3-2", "bytes=10-", "bytes=0-1,3-4"] {
            headers.insert(header::RANGE, HeaderValue::from_str(invalid).unwrap());
            assert_eq!(requested_range(&headers, 10), Err(()), "accepted {invalid}");
        }
        let reference = into_markdown::ArtifactReference {
            storage_key: "a".repeat(32),
            kind: into_markdown::ArtifactKind::Asset,
            byte_len: 10,
            sha256: "b".repeat(64),
            asset_id: Some("asset-1".into()),
            filename: Some("报告 \"final\".png".into()),
            media_type: Some("image/png".into()),
        };
        let disposition = content_disposition(&reference);
        assert!(disposition.starts_with("attachment; filename=\"____final_.png\";"));
        assert!(disposition.contains("filename*=UTF-8''%E6%8A%A5%E5%91%8A%20%22final%22.png"));
        assert!(!disposition.contains('\r') && !disposition.contains('\n'));
        assert_eq!(artifact_content_type(&reference), "image/png");
        let mut reference = reference;
        for (kind, mime, filename) in [
            (into_markdown::ArtifactKind::Markdown, "text/markdown; charset=utf-8", "result.md"),
            (into_markdown::ArtifactKind::DocumentIr, "application/json", "document-ir.json"),
            (into_markdown::ArtifactKind::Diagnostics, "application/json", "diagnostics.json"),
            (into_markdown::ArtifactKind::Bundle, "application/zip", "result.zip"),
        ] {
            reference.kind = kind;
            assert_eq!(artifact_content_type(&reference), mime);
            assert_eq!(artifact_filename(&reference), filename);
        }
    }

    #[tokio::test]
    async fn real_loopback_server_requires_exact_host_origin_and_session() {
        let (_directory, port, session, shutdown, task) = start().await;
        let host = format!("127.0.0.1:{port}");
        let origin = format!("http://{host}");
        // Same-origin browser fetches naturally include Origin on non-GET
        // methods. The script sends an empty POST and cannot set Origin itself.
        let valid = request(port, &format!("POST /api/status HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).await;
        assert!(valid.starts_with("HTTP/1.1 200"), "{valid:?}");
        assert!(valid.contains("\"schemaVersion\":1"));
        assert!(valid.contains("\"available\":"));
        assert!(!valid.contains(&session));
        assert_security_headers(&valid);

        let browser_get = request(
            port,
            &format!("GET /api/status HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_schema_error(&browser_get, "HTTP/1.1 403", "invalidOrigin");

        let same_origin_browser_get = request(
            port,
            &format!(
                "GET /api/status HTTP/1.1\r\nHost: {host}\r\nSec-Fetch-Site: same-origin\r\nSec-Fetch-Mode: cors\r\nX-Into-Md-Session: {session}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_schema_error(&same_origin_browser_get, "HTTP/1.1 405", "methodNotAllowed");

        for (site, mode) in [("cross-site", "cors"), ("same-origin", "navigate")] {
            let rejected = request(
                port,
                &format!(
                    "GET /api/status HTTP/1.1\r\nHost: {host}\r\nSec-Fetch-Site: {site}\r\nSec-Fetch-Mode: {mode}\r\nX-Into-Md-Session: {session}\r\nConnection: close\r\n\r\n"
                ),
            )
            .await;
            assert_schema_error(&rejected, "HTTP/1.1 403", "invalidOrigin");
        }

        let authenticated_get = request(port, &format!("GET /api/status HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nConnection: close\r\n\r\n")).await;
        assert_schema_error(&authenticated_get, "HTTP/1.1 405", "methodNotAllowed");

        for bad_headers in [
            format!(
                "Host: {host}\r\nOrigin: http://localhost:{port}\r\nX-Into-Md-Session: {session}"
            ),
            format!("Host: localhost:{port}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}"),
            format!("Host: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: wrong"),
            format!(
                "Host: {host}\r\nOrigin: {origin}, http://evil.invalid\r\nX-Into-Md-Session: {session}"
            ),
        ] {
            let response = request(
                port,
                &format!("POST /api/status HTTP/1.1\r\n{bad_headers}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert!(!response.starts_with("HTTP/1.1 200"), "{response}");
            assert!(!response.contains(&session));
            assert_security_headers(&response);
        }
        let preflight = request(port, &format!("OPTIONS /api/status HTTP/1.1\r\nHost: {host}\r\nOrigin: http://evil.invalid\r\nAccess-Control-Request-Method: GET\r\nAccess-Control-Request-Headers: X-Into-Md-Session\r\nConnection: close\r\n\r\n")).await;
        assert!(preflight.starts_with("HTTP/1.1 403"), "{preflight}");
        assert_security_headers(&preflight);
        assert!(!preflight.to_ascii_lowercase().contains("access-control-allow-origin"));
        let unknown_api = request(
            port,
            &format!("GET /api/future HTTP/1.1\r\nHost: {host}\r\nOrigin: http://evil.invalid\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert!(unknown_api.starts_with("HTTP/1.1 403"), "{unknown_api}");
        assert_security_headers(&unknown_api);

        let authenticated_unknown = request(port, &format!("POST /api/future HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).await;
        assert_schema_error(&authenticated_unknown, "HTTP/1.1 404", "apiNotFound");

        let unexpected_type = request(port, &format!("POST /api/status HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nContent-Type: application/json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).await;
        assert_schema_error(&unexpected_type, "HTTP/1.1 415", "unexpectedContentType");
        let unexpected_body = request(port, &format!("POST /api/status HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx")).await;
        assert_schema_error(&unexpected_body, "HTTP/1.1 400", "requestBodyNotAllowed");
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn task_event_stream_emits_versioned_snapshot_and_rejects_bad_cursor() {
        let (_directory, port, session, shutdown, task) = start().await;
        let host = format!("127.0.0.1:{port}");
        let origin = format!("http://{host}");
        let uploaded = request(
            port,
            &format!(
                "POST /api/tasks HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nX-Into-Md-Filename: events.txt\r\nContent-Length: 6\r\nConnection: close\r\n\r\nevents"
            ),
        )
        .await;
        let body = uploaded.split_once("\r\n\r\n").unwrap().1;
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        let id = value["id"].as_str().unwrap();

        let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
        stream
            .write_all(
                format!(
                    "GET /api/tasks/{id}/events HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), async {
            let mut response = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let read = stream.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0, "SSE ended before its first event");
                response.extend_from_slice(&chunk[..read]);
                if response.windows(b"event: snapshot".len()).any(|part| part == b"event: snapshot")
                    && response.windows(b"\n\n".len()).any(|part| part == b"\n\n")
                {
                    break String::from_utf8(response).unwrap();
                }
            }
        })
        .await
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.to_ascii_lowercase().contains("content-type: text/event-stream"));
        assert!(response.contains("\"schemaVersion\":1"), "{response}");
        assert!(response.contains("\"sequence\":"), "{response}");
        assert!(response.contains("id: "), "{response}");
        drop(stream); // Browser close must only release this subscriber.

        let invalid = request(
            port,
            &format!(
                "GET /api/tasks/{id}/events HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nLast-Event-ID: invalid\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_schema_error(&invalid, "HTTP/1.1 400", "invalidLastEventId");
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn fragment_never_enters_static_request_or_embedded_assets_and_shutdown_is_graceful() {
        let (_directory, port, session, shutdown, task) = start().await;
        let host = format!("127.0.0.1:{port}");
        let response =
            request(port, &format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"))
                .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response:?}");
        assert!(!response.contains(&session));
        assert_security_headers(&response);

        let css_asset = crate::ui_assets::ASSETS
            .iter()
            .find(|asset| asset.mime.starts_with("text/css"))
            .unwrap();
        let css = request(
            port,
            &format!(
                "GET {} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n",
                css_asset.path
            ),
        )
        .await;
        assert!(css.starts_with("HTTP/1.1 200"), "{css}");
        assert!(css.to_ascii_lowercase().contains("content-type: text/css; charset=utf-8"));
        assert!(
            css.to_ascii_lowercase().contains("cache-control: public, max-age=31536000, immutable")
        );
        assert!(css.to_ascii_lowercase().contains(&format!("etag: \"{}\"", css_asset.sha256)));
        assert!(css.to_ascii_lowercase().contains("x-content-type-options: nosniff"));
        assert!(css.to_ascii_lowercase().contains("content-security-policy: default-src 'none';"));

        let spa = request(
            port,
            &format!("GET /future-route HTTP/1.1\r\nHost: {host}\r\nAccept: text/html\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert!(spa.starts_with("HTTP/1.1 200"), "{spa}");
        assert!(spa.contains("<div id=\"app\">"));

        let missing = request(
            port,
            &format!("GET /missing HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_schema_error(&missing, "HTTP/1.1 404", "notFound");

        let bad_host =
            request(port, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await;
        assert_schema_error(&bad_host, "HTTP/1.1 400", "invalidHost");
        let bootstrap = std::str::from_utf8(
            crate::ui_assets::ASSETS
                .iter()
                .find(|asset| asset.path.starts_with("/assets/bootstrap."))
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert!(bootstrap.find("replaceState").unwrap() < bootstrap.find("import(").unwrap());
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn authenticated_upload_status_and_download_are_real_and_disconnect_cleans_stage() {
        let (directory, port, session, shutdown, task) = start().await;
        let host = format!("127.0.0.1:{port}");
        let origin = format!("http://{host}");
        let uploaded = request(
            port,
            &format!(
                "POST /api/tasks HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nX-Into-Md-Filename: note.txt\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
            ),
        )
        .await;
        assert!(uploaded.starts_with("HTTP/1.1 202"), "{uploaded}");
        let body = uploaded.split_once("\r\n\r\n").unwrap().1;
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        let id = value["id"].as_str().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let artifact = loop {
            let status = request(
                port,
                &format!(
                    "GET /api/tasks/{id} HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nConnection: close\r\n\r\n"
                ),
            )
            .await;
            let status: serde_json::Value =
                serde_json::from_str(status.split_once("\r\n\r\n").unwrap().1).unwrap();
            if status["status"] == "succeeded" {
                break status["artifacts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|artifact| artifact["kind"] == "markdown")
                    .unwrap()["storageKey"]
                    .as_str()
                    .unwrap()
                    .to_owned();
            }
            assert!(std::time::Instant::now() < deadline, "{status}");
            tokio::task::yield_now().await;
        };
        let downloaded = request(
            port,
            &format!(
                "GET /api/tasks/{id}/artifacts/{artifact} HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert!(downloaded.starts_with("HTTP/1.1 200"), "{downloaded}");
        assert!(downloaded.to_ascii_lowercase().contains("accept-ranges: bytes"));
        assert!(downloaded.to_ascii_lowercase().contains("content-type: text/markdown"));
        assert!(
            downloaded
                .to_ascii_lowercase()
                .contains("content-disposition: attachment; filename=\"result.md\"")
        );
        assert!(downloaded.ends_with("hello\n"), "{downloaded}");

        let partial = request(
            port,
            &format!(
                "GET /api/tasks/{id}/artifacts/{artifact} HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nRange: bytes=1-3\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert!(partial.starts_with("HTTP/1.1 206"), "{partial}");
        assert!(partial.to_ascii_lowercase().contains("content-range: bytes 1-3/6"));
        assert!(partial.to_ascii_lowercase().contains("content-length: 3"));
        assert!(partial.ends_with("ell"), "{partial}");

        let invalid_range = request(
            port,
            &format!(
                "GET /api/tasks/{id}/artifacts/{artifact} HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nRange: bytes=99-\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_schema_error(&invalid_range, "HTTP/1.1 416", "invalidRange");
        assert!(invalid_range.to_ascii_lowercase().contains("content-range: bytes */6"));

        let mut disconnected =
            tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
        disconnected
            .write_all(
                format!(
                    "POST /api/tasks HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nX-Into-Md-Filename: partial.txt\r\nContent-Length: 100\r\n\r\nshort"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(disconnected);
        let incoming = directory.path().join("backend/incoming");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if std::fs::read_dir(&incoming).unwrap().next().is_none() {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn slow_upload_and_slow_reader_cannot_block_bounded_shutdown_or_storage_release() {
        let listener = bind_loopback(0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let session = new_session().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(directory.path().join("backend")).unwrap();
        let retained = backend.clone();
        let (sender, receiver) = oneshot::channel();
        let server = tokio::spawn(serve(listener, session.clone(), backend, async {
            let _ = receiver.await;
        }));
        let host = format!("127.0.0.1:{port}");
        let origin = format!("http://{host}");

        let mut slow_upload =
            tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
        slow_upload
            .write_all(
                format!(
                    "POST /api/tasks HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nX-Into-Md-Filename: slow.txt\r\nContent-Length: 1000000\r\n\r\nx"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut timeout_response = vec![0_u8; 4096];
        let read =
            tokio::time::timeout(Duration::from_secs(1), slow_upload.read(&mut timeout_response))
                .await
                .unwrap()
                .unwrap();
        let timeout_response = String::from_utf8_lossy(&timeout_response[..read]);
        assert!(timeout_response.starts_with("HTTP/1.1 408"), "{timeout_response}");
        drop(slow_upload);
        let incoming = directory.path().join("backend/incoming");
        let deadline = Instant::now() + Duration::from_secs(2);
        while std::fs::read_dir(&incoming).unwrap().next().is_some() {
            assert!(Instant::now() < deadline);
            tokio::task::yield_now().await;
        }

        let mut shutdown_upload =
            tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
        shutdown_upload
            .write_all(
                format!(
                    "POST /api/tasks HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nX-Into-Md-Filename: shutdown.txt\r\nContent-Length: 1000000\r\n\r\nx"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        sender.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
        drop(shutdown_upload);
        let deadline = Instant::now() + Duration::from_secs(2);
        while std::fs::read_dir(&incoming).unwrap().next().is_some() {
            assert!(Instant::now() < deadline);
            tokio::task::yield_now().await;
        }

        // Start another server around the retained backend and deliberately do
        // not consume a large response body, forcing the shutdown grace bound
        // to release the anonymous artifact snapshot.
        let mut upload = retained.begin_upload("large.txt", None).unwrap();
        let chunk = ("x".repeat(1023) + "\n").repeat(256).into_bytes();
        for _ in 0..8 {
            upload.write_chunk(&chunk).unwrap();
        }
        let submitted = upload.finish().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let artifact = loop {
            let current = retained.get(&submitted.id).unwrap();
            if current.status == into_markdown::TaskStatus::Succeeded {
                break current
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.kind == into_markdown::ArtifactKind::Markdown)
                    .unwrap()
                    .storage_key
                    .clone();
            }
            assert!(std::time::Instant::now() < deadline, "{current:?}");
            tokio::task::yield_now().await;
        };
        let listener = bind_loopback(0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, receiver) = oneshot::channel();
        let server_session = session.clone();
        let server_backend = retained.clone();
        let server = tokio::spawn(serve(listener, server_session, server_backend, async {
            let _ = receiver.await;
        }));
        let host = format!("127.0.0.1:{port}");
        let origin = format!("http://{host}");
        let mut slow_reader =
            tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
        slow_reader
            .write_all(
                format!(
                    "GET /api/tasks/{}/artifacts/{artifact} HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\n\r\n",
                    submitted.id.as_str()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut first_byte = [0_u8; 1];
        slow_reader.read_exact(&mut first_byte).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while retained.test_reserved_bytes() == 0 {
            assert!(Instant::now() < deadline, "download snapshot was not charged");
            tokio::task::yield_now().await;
        }
        // Do not signal shutdown: the absolute response deadline must revoke
        // the anonymous snapshot even while socket backpressure prevents more
        // Body polling.
        let deadline = Instant::now() + Duration::from_secs(4);
        while retained.test_reserved_bytes() != 0 {
            assert!(Instant::now() < deadline, "download deadline did not release quota");
            tokio::task::yield_now().await;
        }
        assert!(!server.is_finished(), "deadline unexpectedly stopped the listener");
        drop(slow_reader);

        // A second blocked response still observes explicit server shutdown.
        let mut shutdown_reader =
            tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
        shutdown_reader
            .write_all(
                format!(
                    "GET /api/tasks/{}/artifacts/{artifact} HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\n\r\n",
                    submitted.id.as_str()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        shutdown_reader.read_exact(&mut first_byte).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while retained.test_reserved_bytes() == 0 {
            assert!(Instant::now() < deadline, "shutdown snapshot was not charged");
            tokio::task::yield_now().await;
        }
        sender.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
        drop(shutdown_reader);
        let deadline = Instant::now() + Duration::from_secs(2);
        while retained.test_reserved_bytes() != 0 {
            assert!(Instant::now() < deadline, "download snapshot charge was not released");
            tokio::task::yield_now().await;
        }
        let snapshots = directory.path().join("backend/snapshots");
        let deadline = Instant::now() + Duration::from_secs(2);
        while std::fs::read_dir(&snapshots).unwrap().next().is_some() {
            assert!(Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        drop(retained);
    }

    #[tokio::test]
    async fn occupied_explicit_port_is_reported() {
        let occupied = bind_loopback(0).await.unwrap();
        let port = occupied.local_addr().unwrap().port();
        let error = bind_loopback(port).await.unwrap_err();
        assert_eq!(error.code(), "uiBindFailed");
    }

    struct RecordingOpener {
        urls: Mutex<Vec<String>>,
        fail: bool,
    }

    impl BrowserOpener for RecordingOpener {
        fn kind(&self) -> &'static str {
            "test-opener"
        }
        fn open(&self, url: &str) -> std::io::Result<()> {
            self.urls.lock().unwrap().push(url.to_owned());
            if self.fail { Err(std::io::Error::other("injected")) } else { Ok(()) }
        }
    }

    #[test]
    fn opener_injection_preserves_fragment_and_failure_diagnostic_omits_it() {
        let opener = RecordingOpener { urls: Mutex::new(Vec::new()), fail: true };
        let session = new_session().unwrap();
        let origin = "http://127.0.0.1:1";
        let url = format!("http://127.0.0.1:1/#{SESSION_FRAGMENT}={session}");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        announce_and_open(&opener, false, origin, &url, &mut stdout, &mut stderr).unwrap();
        assert_eq!(opener.urls.lock().unwrap().as_slice(), &[url]);
        let diagnostic = String::from_utf8(stderr).unwrap();
        let handoff = String::from_utf8(stdout).unwrap();
        assert!(!diagnostic.contains(&session));
        assert!(diagnostic.contains("test-opener"));
        assert!(handoff.contains(&session));
    }

    #[cfg(unix)]
    #[test]
    fn data_directory_is_private_and_rejects_symlinks_or_public_permissions() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let temp = tempfile::tempdir().unwrap();
        let physical_root = temp.path().canonicalize().unwrap();
        let private = physical_root.join("private");
        prepare_data_dir(&private).unwrap();
        assert_eq!(std::fs::metadata(&private).unwrap().permissions().mode() & 0o777, 0o700);

        let public = physical_root.join("public");
        std::fs::create_dir(&public).unwrap();
        std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(prepare_data_dir(&public).unwrap_err().code(), "unsafeDataDirectory");

        let link = physical_root.join("link");
        symlink(&private, &link).unwrap();
        assert_eq!(prepare_data_dir(&link).unwrap_err().code(), "unsafeDataDirectory");
    }

    #[test]
    fn session_has_fixed_url_safe_shape_and_constant_time_comparison() {
        let session = new_session().unwrap();
        assert_eq!(session.len(), SESSION_ENCODED_LEN);
        assert!(
            session
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        );
        assert!(session_matches(session.as_bytes(), session.as_bytes()));
        assert!(!session_matches(session.as_bytes(), b"short"));
    }
}

//! Loopback-only Web entry point and its security boundary.

use crate::args::UiArgs;
use crate::error::{CliError, ExitClass};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use std::future::Future;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use tokio::net::TcpListener;

const SESSION_HEADER: HeaderName = HeaderName::from_static("x-into-md-session");
const SESSION_FRAGMENT: &str = "into-md-session";
const SESSION_BYTES: usize = 32;
const SESSION_ENCODED_LEN: usize = 43;

#[derive(Clone)]
struct AppState {
    authority: Arc<str>,
    origin: Arc<str>,
    session: Arc<str>,
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

    let listener = bind_loopback(arguments.port).await?;
    let address = listener.local_addr()?;
    let session = new_session()?;
    let origin = format!("http://127.0.0.1:{}", address.port());
    let launch_url = format!("{origin}/#{SESSION_FRAGMENT}={session}");
    announce_and_open(&SystemBrowser, arguments.no_open, &origin, &launch_url, stdout, stderr)?;

    serve(listener, session, async {
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

async fn serve<F>(listener: TcpListener, session: String, shutdown: F) -> Result<(), CliError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let address = listener.local_addr()?;
    if address.ip() != Ipv4Addr::LOCALHOST {
        return Err(CliError::internal("local Web listener is not IPv4 loopback"));
    }
    let authority: Arc<str> = format!("127.0.0.1:{}", address.port()).into();
    let origin: Arc<str> = format!("http://{authority}").into();
    let state = AppState { authority, origin, session: session.into() };
    let api = Router::new()
        .route("/status", post(status).fallback(api_method_not_allowed))
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
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| CliError::new(ExitClass::Io, "uiServeFailed", error.to_string()))
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
    if single_ascii_header(request.headers(), header::ORIGIN) != Some(state.origin.as_ref()) {
        return rejection(StatusCode::FORBIDDEN, "invalidOrigin");
    }
    let supplied = single_ascii_header(request.headers(), SESSION_HEADER.clone()).unwrap_or("");
    if !session_matches(state.session.as_bytes(), supplied.as_bytes()) {
        return rejection(StatusCode::UNAUTHORIZED, "invalidSession");
    }
    next.run(request).await
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
            available: false,
            code: "componentUnavailable",
            detail: "document console is not included in this command",
        },
    })
    .into_response()
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

    async fn start()
    -> (u16, String, oneshot::Sender<()>, tokio::task::JoinHandle<Result<(), CliError>>) {
        let listener = bind_loopback(0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let session = new_session().unwrap();
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(serve(listener, session.clone(), async {
            let _ = receiver.await;
        }));
        (port, session, sender, task)
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

    #[tokio::test]
    async fn real_loopback_server_requires_exact_host_origin_and_session() {
        let (port, session, shutdown, task) = start().await;
        let host = format!("127.0.0.1:{port}");
        let origin = format!("http://{host}");
        // Same-origin browser fetches naturally include Origin on non-GET
        // methods. The script sends an empty POST and cannot set Origin itself.
        let valid = request(port, &format!("POST /api/status HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Into-Md-Session: {session}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).await;
        assert!(valid.starts_with("HTTP/1.1 200"), "{valid:?}");
        assert!(valid.contains("\"schemaVersion\":1"));
        assert!(valid.contains("\"available\":false"));
        assert!(!valid.contains(&session));
        assert_security_headers(&valid);

        let browser_get = request(
            port,
            &format!("GET /api/status HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_schema_error(&browser_get, "HTTP/1.1 403", "invalidOrigin");

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

    #[tokio::test]
    async fn fragment_never_enters_static_request_or_embedded_assets_and_shutdown_is_graceful() {
        let (port, session, shutdown, task) = start().await;
        let host = format!("127.0.0.1:{port}");
        let response =
            request(port, &format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"))
                .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response:?}");
        assert!(!response.contains(&session));
        assert_security_headers(&response);

        let css = request(
            port,
            &format!("GET /assets/app.0f97f210b0bda92b.css HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert!(css.starts_with("HTTP/1.1 200"), "{css}");
        assert!(css.to_ascii_lowercase().contains("content-type: text/css; charset=utf-8"));
        assert!(
            css.to_ascii_lowercase().contains("cache-control: public, max-age=31536000, immutable")
        );
        assert!(
            css.contains(
                "ETag: \"0f97f210b0bda92b80e08591dc5fa42af75299091fc4a0abedfc401b3dfbe7de\""
            ) || css.contains(
                "etag: \"0f97f210b0bda92b80e08591dc5fa42af75299091fc4a0abedfc401b3dfbe7de\""
            )
        );
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
            crate::ui_assets::by_path("/assets/bootstrap.d3fb04cfb97caf03.js").unwrap().bytes,
        )
        .unwrap();
        assert!(bootstrap.find("replaceState").unwrap() < bootstrap.find("import(").unwrap());
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
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

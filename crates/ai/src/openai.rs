//! Bounded, direct OpenAI-compatible transport.
//!
//! The implementation deliberately does not consult proxy environment variables,
//! platform certificate stores, `PATH`, or provider-supplied redirect locations.

use base64::Engine as _;
use flate2::read::GzDecoder;
use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation};
use rustls::pki_types::ServerName;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{self, Cursor, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::{Host, Url};
use zeroize::Zeroize;

const MAX_SECRET_BYTES: usize = 16 * 1024;
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 256 * 1024;
const MAX_OUTPUT_TOKENS: u32 = 16_384;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_DECOMPRESSED_BYTES: usize = 4 * 1024 * 1024;
const MAX_JSON_DEPTH: usize = 32;
const MAX_JSON_VALUES: usize = 20_000;
const MAX_JSON_STRING_BYTES: usize = 16 * 1024;
const MAX_MODELS: usize = 1_000;
const MAX_MODEL_ID_BYTES: usize = 512;
const IO_POLL: Duration = Duration::from_millis(100);
const MAX_RETRIES: u8 = 2;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(2);

/// Stable provider transport failure category. Free-form server text is never retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ProviderErrorCode {
    /// Current invocation did not authorize networking.
    NetworkDenied,
    /// Target host is outside the effective allowlist.
    HostDenied,
    /// A resolved non-public address lacks additional authorization.
    PrivateNetworkDenied,
    /// Provider configuration is ambiguous or unsupported.
    InvalidConfiguration,
    /// Referenced secret environment variable is absent.
    SecretMissing,
    /// Referenced secret is empty, non-Unicode, or otherwise unsafe.
    SecretInvalid,
    /// DNS resolution failed or returned no addresses.
    Dns,
    /// No authorized address accepted a connection.
    Connect,
    /// TLS setup, authentication, or handshake failed.
    Tls,
    /// Provider operation exceeded its deadline.
    Timeout,
    /// Caller cancelled the provider operation.
    Cancelled,
    /// Provider returned HTTP 401.
    Unauthorized,
    /// Provider returned HTTP 403.
    Forbidden,
    /// Provider returned HTTP 404.
    NotFound,
    /// Provider returned HTTP 409.
    Conflict,
    /// Provider returned HTTP 429 after bounded retries.
    RateLimited,
    /// Provider returned a server failure after bounded retries.
    ServerError,
    /// Provider attempted a redirect; redirects are disabled.
    RedirectDenied,
    /// HTTP, content type, JSON, or schema validation failed.
    InvalidResponse,
    /// A response framing or decoded-data limit was exceeded.
    ResponseTooLarge,
    /// Request-scoped memory policy rejected the operation.
    ResourceLimit,
}

/// Sanitized provider error suitable for CLI and structured diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    code: ProviderErrorCode,
}

impl ProviderError {
    fn new(code: ProviderErrorCode) -> Self {
        Self { code }
    }

    /// Stable machine-readable error category.
    #[must_use]
    pub const fn code(&self) -> ProviderErrorCode {
        self.code
    }

    /// Stable ASCII code used by command-line JSON.
    #[must_use]
    pub const fn code_str(&self) -> &'static str {
        match self.code {
            ProviderErrorCode::NetworkDenied => "networkDenied",
            ProviderErrorCode::HostDenied => "hostDenied",
            ProviderErrorCode::PrivateNetworkDenied => "privateNetworkDenied",
            ProviderErrorCode::InvalidConfiguration => "providerConfigurationInvalid",
            ProviderErrorCode::SecretMissing => "providerSecretMissing",
            ProviderErrorCode::SecretInvalid => "providerSecretInvalid",
            ProviderErrorCode::Dns => "providerDns",
            ProviderErrorCode::Connect => "providerConnect",
            ProviderErrorCode::Tls => "providerTls",
            ProviderErrorCode::Timeout => "providerTimeout",
            ProviderErrorCode::Cancelled => "cancelled",
            ProviderErrorCode::Unauthorized => "providerUnauthorized",
            ProviderErrorCode::Forbidden => "providerForbidden",
            ProviderErrorCode::NotFound => "providerNotFound",
            ProviderErrorCode::Conflict => "providerConflict",
            ProviderErrorCode::RateLimited => "providerRateLimited",
            ProviderErrorCode::ServerError => "providerServerError",
            ProviderErrorCode::RedirectDenied => "providerRedirectDenied",
            ProviderErrorCode::InvalidResponse => "providerInvalidResponse",
            ProviderErrorCode::ResponseTooLarge => "providerResponseTooLarge",
            ProviderErrorCode::ResourceLimit => "resourceLimit",
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code_str())
    }
}

impl std::error::Error for ProviderError {}

/// Network authorization supplied only by the current invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderNetworkPolicy {
    /// Master per-invocation network authorization.
    pub allow_network: bool,
    /// Additional authorization for non-public addresses.
    pub allow_private_network: bool,
    /// Canonical host allowlist. Empty permits any host allowed by the other rules.
    pub allowed_hosts: Vec<String>,
}

/// Validated provider configuration containing only a secret reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    base_url: Url,
    model: String,
    api_key_environment_variable: String,
    timeout: Duration,
    configured_capabilities: BTreeSet<String>,
}

impl ProviderConfig {
    /// Parse strict, canonical configuration. No secret is read here.
    ///
    /// # Errors
    ///
    /// Returns `providerConfigurationInvalid` for ambiguous URLs, identifiers,
    /// environment references, or timeouts.
    pub fn parse(
        base_url: &str,
        model: &str,
        api_key_environment_variable: &str,
        timeout: Duration,
        configured_capabilities: impl IntoIterator<Item = String>,
    ) -> Result<Self, ProviderError> {
        let parsed = Url::parse(base_url)
            .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
        if !matches!(parsed.scheme(), "https" | "http")
            || parsed.host().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.as_str() != base_url
            || model.is_empty()
            || model.len() > MAX_MODEL_ID_BYTES
            || !valid_environment_name(api_key_environment_variable)
            || timeout.is_zero()
        {
            return Err(ProviderError::new(ProviderErrorCode::InvalidConfiguration));
        }
        let configured_capabilities = configured_capabilities.into_iter().collect::<BTreeSet<_>>();
        if configured_capabilities.len() > 7
            || configured_capabilities.iter().any(|capability| {
                !matches!(
                    capability.as_str(),
                    "vision-ocr"
                        | "image-description"
                        | "layout-repair"
                        | "table-repair"
                        | "formula-repair"
                        | "audio-transcription"
                        | "markdown-postprocess"
                )
            })
        {
            return Err(ProviderError::new(ProviderErrorCode::InvalidConfiguration));
        }
        Ok(Self {
            base_url: parsed,
            model: model.to_owned(),
            api_key_environment_variable: api_key_environment_variable.to_owned(),
            timeout,
            configured_capabilities,
        })
    }

    /// Canonical, query-free base URL.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }
}

fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

struct Secret {
    bytes: Vec<u8>,
    _reservation: ResourceReservation,
}

impl Secret {
    fn from_environment(name: &str, context: &ExecutionContext) -> Result<Self, ProviderError> {
        let value = std::env::var_os(name)
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::SecretMissing))?;
        Self::from_os_string(value, context)
    }

    fn from_os_string(value: OsString, context: &ExecutionContext) -> Result<Self, ProviderError> {
        let text = os_string_into_string(value)
            .map_err(|()| ProviderError::new(ProviderErrorCode::SecretInvalid))?;
        if text.is_empty()
            || text.len() > MAX_SECRET_BYTES
            || text.bytes().any(|b| b <= 0x20 || b == 0x7f)
        {
            return Err(ProviderError::new(ProviderErrorCode::SecretInvalid));
        }
        let reservation = context.reserve_memory(text.len() as u64).map_err(map_context_error)?;
        Ok(Self { bytes: text.into_bytes(), _reservation: reservation })
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

fn os_string_into_string(value: OsString) -> Result<String, ()> {
    value.into_string().map_err(|_| ())
}

trait Resolver: Send + Sync {
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        (host, port).to_socket_addrs().map(Iterator::collect)
    }
}

/// Successful minimal connectivity/capability result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTestResult {
    /// Stable output schema.
    pub schema_version: u32,
    /// The configured model was listed by the endpoint.
    pub configured_model_available: bool,
    /// Configured capabilities retained after negotiation.
    pub capabilities: Vec<String>,
    /// Bounded number of valid model IDs observed.
    pub model_count: usize,
}

/// OpenAI-compatible generation endpoint selected explicitly by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationEndpoint {
    /// `POST /chat/completions`.
    ChatCompletions,
    /// `POST /responses`.
    Responses,
}

/// Bounded input accepted by the transport layer.
#[derive(Debug, Clone, Copy)]
pub enum GenerationInput<'a> {
    /// UTF-8 text or prompt content.
    Text(&'a str),
    /// One encoded image plus bounded prompt text.
    Image {
        /// PNG, JPEG, GIF, or WebP bytes.
        bytes: &'a [u8],
        /// Exact supported image media type.
        media_type: &'a str,
        /// Text instruction paired with the image.
        prompt: &'a str,
    },
}

/// One explicitly authorized generation request.
#[derive(Debug, Clone, Copy)]
pub struct GenerationRequest<'a> {
    /// Endpoint contract to encode.
    pub endpoint: GenerationEndpoint,
    /// Configured capability required for this operation.
    pub capability: &'a str,
    /// Typed bounded input.
    pub input: GenerationInput<'a>,
    /// Maximum provider output tokens.
    pub max_output_tokens: u32,
    /// Caller-generated ASCII idempotency key. Its presence permits bounded retries.
    pub idempotency_key: Option<&'a str>,
}

/// Bounded text returned by a validated compatible response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationResult {
    /// Stable output schema.
    pub schema_version: u32,
    /// Provider-generated text, not yet interpreted as an IR patch.
    pub text: String,
}

/// Direct client whose default implementation performs no proxy discovery.
pub struct OpenAiCompatibleClient {
    config: ProviderConfig,
    policy: ProviderNetworkPolicy,
    resolver: Arc<dyn Resolver>,
}

impl std::fmt::Debug for OpenAiCompatibleClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleClient")
            .field("base_url", &self.config.base_url)
            .field("model", &self.config.model)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatibleClient {
    /// Construct a client without reading DNS or secrets.
    #[must_use]
    pub fn new(config: ProviderConfig, policy: ProviderNetworkPolicy) -> Self {
        Self { config, policy, resolver: Arc::new(SystemResolver) }
    }

    /// Execute a minimal `GET /models` test. It sends no document or user content.
    ///
    /// # Errors
    ///
    /// Returns a stable [`ProviderError`] for policy, transport, HTTP, or schema failures.
    pub fn test(&self, context: &ExecutionContext) -> Result<ProviderTestResult, ProviderError> {
        let deadline = Instant::now()
            .checked_add(self.config.timeout)
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
        check_operation(context, deadline)?;
        if !self.policy.allow_network {
            return Err(ProviderError::new(ProviderErrorCode::NetworkDenied));
        }
        let endpoint = models_endpoint(&self.config.base_url);
        let host = canonical_host(&endpoint)?;
        self.check_host(&host)?;
        let port = endpoint
            .port_or_known_default()
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
        let addresses =
            resolve_checked(self.resolver.clone(), host.clone(), port, context, deadline)?;
        if addresses.is_empty() {
            return Err(ProviderError::new(ProviderErrorCode::Dns));
        }
        for address in &addresses {
            self.check_address(address.ip())?;
        }
        // Secret lookup is deliberately after URL, host, DNS, and address authorization.
        let secret = Secret::from_environment(&self.config.api_key_environment_variable, context)?;
        let mut retry = 0_u8;
        loop {
            check_operation(context, deadline)?;
            let response =
                Self::request_models(&endpoint, &host, &addresses, &secret, context, deadline)?;
            if retry < MAX_RETRIES && matches!(response.status, 429 | 500..=599) {
                retry += 1;
                let delay = response.retry_after.unwrap_or_else(|| {
                    Duration::from_millis(100_u64.saturating_mul(1_u64 << retry))
                });
                checked_sleep(delay.min(MAX_RETRY_AFTER), context, deadline)?;
                continue;
            }
            return parse_models_response(response, &self.config);
        }
    }

    /// Send one bounded text or image request after the same DNS and address checks as `test`.
    ///
    /// # Errors
    ///
    /// Returns a stable [`ProviderError`] for invalid input, policy, transport,
    /// HTTP, resource-limit, or response-schema failures.
    pub fn generate(
        &self,
        request: GenerationRequest<'_>,
        context: &ExecutionContext,
    ) -> Result<GenerationResult, ProviderError> {
        validate_generation_request(&request, &self.config)?;
        let deadline = Instant::now()
            .checked_add(self.config.timeout)
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
        check_operation(context, deadline)?;
        if !self.policy.allow_network {
            return Err(ProviderError::new(ProviderErrorCode::NetworkDenied));
        }
        let estimated_body = generation_body_bound(&request, self.config.model.len())?;
        let _body_reservation =
            context.reserve_memory(estimated_body as u64).map_err(map_context_error)?;
        let (path, body) = encode_generation_request(&request, &self.config.model)?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err(ProviderError::new(ProviderErrorCode::ResourceLimit));
        }
        let mut endpoint = self.config.base_url.clone();
        endpoint.set_path(&format!("{}{}", endpoint.path().trim_end_matches('/'), path));
        let host = canonical_host(&endpoint)?;
        self.check_host(&host)?;
        let port = endpoint
            .port_or_known_default()
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
        let addresses =
            resolve_checked(self.resolver.clone(), host.clone(), port, context, deadline)?;
        if addresses.is_empty() {
            return Err(ProviderError::new(ProviderErrorCode::Dns));
        }
        for address in &addresses {
            self.check_address(address.ip())?;
        }
        let secret = Secret::from_environment(&self.config.api_key_environment_variable, context)?;
        let spec = RequestSpec {
            method: "POST",
            body: Some(&body),
            idempotency_key: request.idempotency_key,
        };
        let mut retry = 0_u8;
        loop {
            let response =
                Self::request(&endpoint, &host, &addresses, &secret, context, deadline, spec)?;
            if request.idempotency_key.is_some()
                && retry < MAX_RETRIES
                && matches!(response.status, 429 | 500..=599)
            {
                retry += 1;
                checked_sleep(
                    response
                        .retry_after
                        .unwrap_or(Duration::from_millis(100_u64.saturating_mul(1_u64 << retry))),
                    context,
                    deadline,
                )?;
                continue;
            }
            return parse_generation_response(response, request.endpoint);
        }
    }

    fn request_models(
        endpoint: &Url,
        host: &str,
        addresses: &[SocketAddr],
        secret: &Secret,
        context: &ExecutionContext,
        deadline: Instant,
    ) -> Result<HttpResponse, ProviderError> {
        Self::request(
            endpoint,
            host,
            addresses,
            secret,
            context,
            deadline,
            RequestSpec { method: "GET", body: None, idempotency_key: None },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn request(
        endpoint: &Url,
        host: &str,
        addresses: &[SocketAddr],
        secret: &Secret,
        context: &ExecutionContext,
        deadline: Instant,
        spec: RequestSpec<'_>,
    ) -> Result<HttpResponse, ProviderError> {
        let mut last = ProviderError::new(ProviderErrorCode::Connect);
        for address in addresses {
            check_operation(context, deadline)?;
            match Self::request_one(endpoint, host, *address, secret, context, deadline, spec) {
                Ok(response) => return Ok(response),
                Err(error) if error.code() == ProviderErrorCode::Connect => last = error,
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }

    fn request_one(
        endpoint: &Url,
        host: &str,
        address: SocketAddr,
        secret: &Secret,
        context: &ExecutionContext,
        deadline: Instant,
        spec: RequestSpec<'_>,
    ) -> Result<HttpResponse, ProviderError> {
        let connect_timeout = blocking_slice(context, deadline)?;
        let stream =
            TcpStream::connect_timeout(&address, connect_timeout).map_err(map_connect_error)?;
        stream
            .set_read_timeout(Some(connect_timeout))
            .map_err(|_| ProviderError::new(ProviderErrorCode::Connect))?;
        stream
            .set_write_timeout(Some(connect_timeout))
            .map_err(|_| ProviderError::new(ProviderErrorCode::Connect))?;
        let mut stream: Box<dyn ReadWrite> = if endpoint.scheme() == "https" {
            let roots =
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect::<rustls::RootCertStore>();
            let tls =
                rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
            let server_name = ServerName::try_from(host.to_owned())
                .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
            let connection = rustls::ClientConnection::new(Arc::new(tls), server_name)
                .map_err(|_| ProviderError::new(ProviderErrorCode::Tls))?;
            Box::new(rustls::StreamOwned::new(connection, stream))
        } else {
            Box::new(stream)
        };
        let host_header = host_header(endpoint, host)?;
        let target = request_target(endpoint)?;
        let request_capacity = 512_usize
            .saturating_add(secret.bytes.len())
            .saturating_add(spec.body.map_or(0, <[u8]>::len));
        let _request_reservation =
            context.reserve_memory(request_capacity as u64).map_err(map_context_error)?;
        let mut request = Vec::with_capacity(request_capacity);
        write!(
            request,
            "{} {target} HTTP/1.1\r\nHost: {host_header}\r\nAuthorization: Bearer ",
            spec.method
        )
        .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
        request.extend_from_slice(&secret.bytes);
        request.extend_from_slice(b"\r\nAccept: application/json\r\nAccept-Encoding: gzip\r\nConnection: close\r\nUser-Agent: into-md/0\r\n");
        if let Some(key) = spec.idempotency_key {
            write!(request, "Idempotency-Key: {key}\r\n")
                .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
        }
        if let Some(body) = spec.body {
            write!(
                request,
                "Content-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n",
                body.len()
            )
            .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
        }
        request.extend_from_slice(b"\r\n");
        if let Some(body) = spec.body {
            request.extend_from_slice(body);
        }
        if request.len() > MAX_REQUEST_BYTES {
            request.zeroize();
            return Err(ProviderError::new(ProviderErrorCode::InvalidConfiguration));
        }
        let result = write_all_checked(&mut *stream, &request, context, deadline)
            .and_then(|()| read_response(&mut *stream, context, deadline));
        request.zeroize();
        result
    }

    fn check_host(&self, host: &str) -> Result<(), ProviderError> {
        if !self.policy.allowed_hosts.is_empty()
            && !self.policy.allowed_hosts.iter().any(|allowed| allowed == host)
        {
            return Err(ProviderError::new(ProviderErrorCode::HostDenied));
        }
        Ok(())
    }

    fn check_address(&self, address: IpAddr) -> Result<(), ProviderError> {
        if !is_public_ip(address) && !self.policy.allow_private_network {
            return Err(ProviderError::new(ProviderErrorCode::PrivateNetworkDenied));
        }
        if self.config.base_url.scheme() == "http" && is_public_ip(address) {
            return Err(ProviderError::new(ProviderErrorCode::InvalidConfiguration));
        }
        Ok(())
    }
}

fn resolve_checked(
    resolver: Arc<dyn Resolver>,
    host: String,
    port: u16,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<Vec<SocketAddr>, ProviderError> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("into-md-provider-dns".into())
        .spawn(move || {
            let _ = sender.send(resolver.resolve(&host, port));
        })
        .map_err(|_| ProviderError::new(ProviderErrorCode::Dns))?;
    loop {
        check_operation(context, deadline)?;
        match receiver.recv_timeout(blocking_slice(context, deadline)?) {
            Ok(Ok(addresses)) => return Ok(addresses),
            Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(ProviderError::new(ProviderErrorCode::Dns));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

#[derive(Clone, Copy)]
struct RequestSpec<'a> {
    method: &'static str,
    body: Option<&'a [u8]>,
    idempotency_key: Option<&'a str>,
}

fn validate_generation_request(
    request: &GenerationRequest<'_>,
    config: &ProviderConfig,
) -> Result<(), ProviderError> {
    if !config.configured_capabilities.contains(request.capability)
        || request.max_output_tokens == 0
        || request.max_output_tokens > MAX_OUTPUT_TOKENS
        || request.idempotency_key.is_some_and(|key| {
            key.is_empty()
                || key.len() > 128
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
    {
        return Err(ProviderError::new(ProviderErrorCode::InvalidConfiguration));
    }
    match request.input {
        GenerationInput::Text(text) if text.len() <= MAX_PROMPT_BYTES => Ok(()),
        GenerationInput::Image { bytes, media_type, prompt }
            if !bytes.is_empty()
                && bytes.len() <= MAX_IMAGE_BYTES
                && prompt.len() <= MAX_PROMPT_BYTES
                && matches!(
                    media_type,
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) =>
        {
            Ok(())
        }
        _ => Err(ProviderError::new(ProviderErrorCode::ResourceLimit)),
    }
}

fn generation_body_bound(
    request: &GenerationRequest<'_>,
    model_bytes: usize,
) -> Result<usize, ProviderError> {
    let input_bytes = match request.input {
        GenerationInput::Text(text) => text.len().saturating_mul(6),
        GenerationInput::Image { bytes, prompt, .. } => bytes
            .len()
            .checked_add(2)
            .and_then(|size| size.checked_div(3))
            .and_then(|groups| groups.checked_mul(4))
            .and_then(|encoded| encoded.checked_add(prompt.len().saturating_mul(6)))
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::ResourceLimit))?,
    };
    input_bytes
        .checked_add(model_bytes.saturating_mul(6))
        .and_then(|bytes| bytes.checked_add(2_048))
        .filter(|bytes| *bytes <= MAX_REQUEST_BYTES)
        .ok_or_else(|| ProviderError::new(ProviderErrorCode::ResourceLimit))
}

fn encode_generation_request(
    request: &GenerationRequest<'_>,
    model: &str,
) -> Result<(&'static str, Vec<u8>), ProviderError> {
    let content = match request.input {
        GenerationInput::Text(text) => serde_json::json!(text),
        GenerationInput::Image { bytes, media_type, prompt } => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            serde_json::json!([
                {"type": "text", "text": prompt},
                {"type": "image_url", "image_url": {"url": format!("data:{media_type};base64,{encoded}")}}
            ])
        }
    };
    let (path, value) = match request.endpoint {
        GenerationEndpoint::ChatCompletions => (
            "/chat/completions",
            serde_json::json!({"model": model, "messages": [{"role": "user", "content": content}], "max_tokens": request.max_output_tokens}),
        ),
        GenerationEndpoint::Responses => (
            "/responses",
            serde_json::json!({"model": model, "input": [{"role": "user", "content": content}], "max_output_tokens": request.max_output_tokens}),
        ),
    };
    serde_json::to_vec(&value)
        .map(|body| (path, body))
        .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidConfiguration))
}

fn models_endpoint(base: &Url) -> Url {
    let mut endpoint = base.clone();
    let path = format!("{}/models", base.path().trim_end_matches('/'));
    endpoint.set_path(&path);
    endpoint
}

fn canonical_host(url: &Url) -> Result<String, ProviderError> {
    let host =
        url.host().ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
    Ok(match host {
        Host::Domain(value) => value.trim_end_matches('.').to_ascii_lowercase(),
        Host::Ipv4(value) => value.to_string(),
        Host::Ipv6(value) => value.to_string(),
    })
}

fn host_header(url: &Url, host: &str) -> Result<String, ProviderError> {
    let display = if host.contains(':') { format!("[{host}]") } else { host.to_owned() };
    let default = match url.scheme() {
        "https" => 443,
        "http" => 80,
        _ => return Err(ProviderError::new(ProviderErrorCode::InvalidConfiguration)),
    };
    Ok(match url.port() {
        Some(port) if port != default => format!("{display}:{port}"),
        _ => display,
    })
}

fn request_target(url: &Url) -> Result<String, ProviderError> {
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ProviderError::new(ProviderErrorCode::InvalidConfiguration));
    }
    Ok(if url.path().is_empty() { "/".into() } else { url.path().into() })
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(mapped));
            }
            let segments = ip.segments();
            (segments[0] & 0xe000) == 0x2000 && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        }
    }
}

struct HttpResponse {
    status: u16,
    retry_after: Option<Duration>,
    body: Vec<u8>,
    _reservations: Vec<ResourceReservation>,
}

fn write_all_checked(
    stream: &mut dyn ReadWrite,
    bytes: &[u8],
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<(), ProviderError> {
    let mut written = 0;
    while written < bytes.len() {
        check_operation(context, deadline)?;
        match stream.write(&bytes[written..]) {
            Ok(0) => return Err(ProviderError::new(ProviderErrorCode::Connect)),
            Ok(count) => written += count,
            Err(error)
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => return Err(ProviderError::new(ProviderErrorCode::Connect)),
        }
    }
    stream.flush().map_err(|_| ProviderError::new(ProviderErrorCode::Connect))
}

#[allow(clippy::too_many_lines)]
fn read_response(
    stream: &mut dyn ReadWrite,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<HttpResponse, ProviderError> {
    let header = read_until(stream, b"\r\n\r\n", MAX_HEADER_BYTES, context, deadline)?;
    let text = std::str::from_utf8(&header)
        .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    let mut lines = text[..text.len() - 4].split("\r\n");
    let status_line =
        lines.next().ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    let mut status_parts = status_line.split(' ');
    if status_parts.next() != Some("HTTP/1.1") {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    let status = status_parts
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| (100..=599).contains(value))
        .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    let mut content_length = None;
    let mut chunked = false;
    let mut saw_transfer_encoding = false;
    let mut gzip = false;
    let mut json = false;
    let mut retry_after = None;
    let mut saw_content_encoding = false;
    let mut saw_content_type = false;
    let mut saw_retry_after = false;
    let mut header_count = 0_usize;
    for line in lines {
        header_count += 1;
        if header_count > 128 || line.len() > 8 * 1024 || line.starts_with([' ', '\t']) {
            return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
        if name.trim() != name
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b == b'\0' || b == b'\r' || b == b'\n')
        {
            return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
        }
        let name = name.to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => {
                let parsed = value
                    .parse::<usize>()
                    .ok()
                    .filter(|v| *v <= MAX_RESPONSE_BYTES)
                    .ok_or_else(|| ProviderError::new(ProviderErrorCode::ResponseTooLarge))?;
                if content_length.replace(parsed).is_some() {
                    return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
                }
            }
            "transfer-encoding" => {
                if saw_transfer_encoding || !value.eq_ignore_ascii_case("chunked") {
                    return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
                }
                saw_transfer_encoding = true;
                chunked = true;
            }
            "content-encoding" => {
                if saw_content_encoding {
                    return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
                }
                saw_content_encoding = true;
                if value.eq_ignore_ascii_case("gzip") {
                    gzip = true;
                } else if !value.eq_ignore_ascii_case("identity") {
                    return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
                }
            }
            "content-type" => {
                if saw_content_type {
                    return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
                }
                saw_content_type = true;
                json = valid_json_content_type(value);
            }
            "retry-after" => {
                if saw_retry_after {
                    return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
                }
                saw_retry_after = true;
                retry_after = parse_retry_after(value);
            }
            "connection"
                if value.split(',').any(|token| token.trim().eq_ignore_ascii_case("upgrade")) =>
            {
                return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
            }
            "upgrade" => return Err(ProviderError::new(ProviderErrorCode::InvalidResponse)),
            _ => {}
        }
    }
    if status < 200 || status == 101 {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    if status == 200 && !json {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    if saw_transfer_encoding && content_length.is_some() {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    let mut reservations = Vec::new();
    let compressed = if chunked {
        read_chunked(stream, context, deadline, &mut reservations)?
    } else if let Some(length) = content_length {
        reservations.push(context.reserve_memory(length as u64).map_err(map_context_error)?);
        read_exact_bounded(stream, length, context, deadline)?
    } else {
        read_to_eof_bounded(stream, context, deadline, &mut reservations)?
    };
    let body =
        if gzip { decompress_gzip(&compressed, context, &mut reservations)? } else { compressed };
    Ok(HttpResponse { status, retry_after, body, _reservations: reservations })
}

fn valid_json_content_type(value: &str) -> bool {
    let mut parts = value.split(';');
    let media = parts.next().unwrap_or("").trim();
    if !(media.eq_ignore_ascii_case("application/json")
        || media.to_ascii_lowercase().ends_with("+json"))
    {
        return false;
    }
    parts.all(|part| {
        let part = part.trim();
        part.is_empty()
            || part.eq_ignore_ascii_case("charset=utf-8")
            || part.eq_ignore_ascii_case("charset=\"utf-8\"")
    })
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    value.parse::<u64>().ok().map(Duration::from_secs).map(|delay| delay.min(MAX_RETRY_AFTER))
}

fn read_until(
    stream: &mut dyn ReadWrite,
    delimiter: &[u8],
    limit: usize,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<Vec<u8>, ProviderError> {
    let mut output = Vec::new();
    while output.len() < limit {
        let byte = read_one(stream, context, deadline)?;
        output.push(byte);
        if output.ends_with(delimiter) {
            return Ok(output);
        }
    }
    Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge))
}

fn read_one(
    stream: &mut dyn ReadWrite,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<u8, ProviderError> {
    let mut byte = [0];
    loop {
        check_operation(context, deadline)?;
        match stream.read(&mut byte) {
            Ok(1) => return Ok(byte[0]),
            Ok(0) => return Err(ProviderError::new(ProviderErrorCode::InvalidResponse)),
            Ok(_) => unreachable!(),
            Err(error)
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => return Err(ProviderError::new(ProviderErrorCode::Connect)),
        }
    }
}

fn read_exact_bounded(
    stream: &mut dyn ReadWrite,
    length: usize,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<Vec<u8>, ProviderError> {
    if length > MAX_RESPONSE_BYTES {
        return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
    }
    let mut output = vec![0; length];
    let mut read = 0;
    while read < length {
        check_operation(context, deadline)?;
        match stream.read(&mut output[read..]) {
            Ok(0) => return Err(ProviderError::new(ProviderErrorCode::InvalidResponse)),
            Ok(count) => read += count,
            Err(error)
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => return Err(ProviderError::new(ProviderErrorCode::Connect)),
        }
    }
    Ok(output)
}

fn read_to_eof_bounded(
    stream: &mut dyn ReadWrite,
    context: &ExecutionContext,
    deadline: Instant,
    reservations: &mut Vec<ResourceReservation>,
) -> Result<Vec<u8>, ProviderError> {
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        check_operation(context, deadline)?;
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(output),
            Ok(count) if output.len().saturating_add(count) <= MAX_RESPONSE_BYTES => {
                reservations.push(context.reserve_memory(count as u64).map_err(map_context_error)?);
                output.extend_from_slice(&buffer[..count]);
            }
            Ok(_) => return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge)),
            Err(error)
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => return Err(ProviderError::new(ProviderErrorCode::Connect)),
        }
    }
}

fn read_chunked(
    stream: &mut dyn ReadWrite,
    context: &ExecutionContext,
    deadline: Instant,
    reservations: &mut Vec<ResourceReservation>,
) -> Result<Vec<u8>, ProviderError> {
    let mut output = Vec::new();
    loop {
        let line = read_until(stream, b"\r\n", 128, context, deadline)?;
        let text = std::str::from_utf8(&line[..line.len() - 2])
            .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
        if text.contains(';') || text.is_empty() || text.len() > 16 {
            return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
        }
        let size_text = text;
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
        if size == 0 {
            let end = read_until(stream, b"\r\n", MAX_HEADER_BYTES, context, deadline)?;
            if end != b"\r\n" {
                return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
            }
            return Ok(output);
        }
        if output.len().saturating_add(size) > MAX_RESPONSE_BYTES {
            return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
        }
        reservations.push(context.reserve_memory(size as u64).map_err(map_context_error)?);
        let chunk = read_exact_bounded(stream, size, context, deadline)?;
        output.extend_from_slice(&chunk);
        if read_exact_bounded(stream, 2, context, deadline)? != b"\r\n" {
            return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
        }
    }
}

fn decompress_gzip(
    input: &[u8],
    context: &ExecutionContext,
    reservations: &mut Vec<ResourceReservation>,
) -> Result<Vec<u8>, ProviderError> {
    let mut decoder = GzDecoder::new(Cursor::new(input));
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = decoder
            .read(&mut chunk)
            .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
        if count == 0 {
            break;
        }
        if output.len().saturating_add(count) > MAX_DECOMPRESSED_BYTES {
            return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
        }
        reservations.push(context.reserve_memory(count as u64).map_err(map_context_error)?);
        output.extend_from_slice(&chunk[..count]);
    }
    Ok(output)
}

#[allow(clippy::needless_pass_by_value)]
fn parse_models_response(
    response: HttpResponse,
    config: &ProviderConfig,
) -> Result<ProviderTestResult, ProviderError> {
    match response.status {
        200 => {}
        401 => return Err(ProviderError::new(ProviderErrorCode::Unauthorized)),
        403 => return Err(ProviderError::new(ProviderErrorCode::Forbidden)),
        404 => return Err(ProviderError::new(ProviderErrorCode::NotFound)),
        408 => return Err(ProviderError::new(ProviderErrorCode::Timeout)),
        409 => return Err(ProviderError::new(ProviderErrorCode::Conflict)),
        429 => return Err(ProviderError::new(ProviderErrorCode::RateLimited)),
        500..=599 => return Err(ProviderError::new(ProviderErrorCode::ServerError)),
        300..=399 => return Err(ProviderError::new(ProviderErrorCode::RedirectDenied)),
        _ => return Err(ProviderError::new(ProviderErrorCode::InvalidResponse)),
    }
    let value: Value = serde_json::from_slice(&response.body)
        .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    validate_json_bounds(&value)?;
    let object =
        value.as_object().ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "object" | "data" | "has_more" | "first_id" | "last_id"))
    {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    let data = object
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    if data.len() > MAX_MODELS {
        return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
    }
    let mut ids = BTreeSet::new();
    for model in data {
        let entry = model
            .as_object()
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= MAX_MODEL_ID_BYTES
                    && !id.chars().any(char::is_control)
            })
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
        ids.insert(id);
    }
    Ok(ProviderTestResult {
        schema_version: 1,
        configured_model_available: ids.contains(config.model.as_str()),
        capabilities: config.configured_capabilities.iter().cloned().collect(),
        model_count: ids.len(),
    })
}

#[allow(clippy::needless_pass_by_value)]
fn parse_generation_response(
    response: HttpResponse,
    endpoint: GenerationEndpoint,
) -> Result<GenerationResult, ProviderError> {
    ensure_success_status(response.status)?;
    let value: Value = serde_json::from_slice(&response.body)
        .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    validate_json_bounds(&value)?;
    let text = match endpoint {
        GenerationEndpoint::ChatCompletions => value
            .get("choices")
            .and_then(Value::as_array)
            .filter(|choices| choices.len() == 1)
            .and_then(|choices| choices[0].get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str),
        GenerationEndpoint::Responses => value.get("output_text").and_then(Value::as_str),
    }
    .filter(|text| text.len() <= MAX_JSON_STRING_BYTES)
    .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    Ok(GenerationResult { schema_version: 1, text: text.to_owned() })
}

fn ensure_success_status(status: u16) -> Result<(), ProviderError> {
    match status {
        200..=299 => Ok(()),
        401 => Err(ProviderError::new(ProviderErrorCode::Unauthorized)),
        403 => Err(ProviderError::new(ProviderErrorCode::Forbidden)),
        404 => Err(ProviderError::new(ProviderErrorCode::NotFound)),
        408 => Err(ProviderError::new(ProviderErrorCode::Timeout)),
        409 => Err(ProviderError::new(ProviderErrorCode::Conflict)),
        429 => Err(ProviderError::new(ProviderErrorCode::RateLimited)),
        500..=599 => Err(ProviderError::new(ProviderErrorCode::ServerError)),
        300..=399 => Err(ProviderError::new(ProviderErrorCode::RedirectDenied)),
        _ => Err(ProviderError::new(ProviderErrorCode::InvalidResponse)),
    }
}

fn validate_json_bounds(root: &Value) -> Result<(), ProviderError> {
    let mut stack = vec![(root, 1_usize)];
    let mut values = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        values += 1;
        if values > MAX_JSON_VALUES || depth > MAX_JSON_DEPTH {
            return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
        }
        match value {
            Value::String(text) if text.len() > MAX_JSON_STRING_BYTES => {
                return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
            }
            Value::Array(items) => stack.extend(items.iter().map(|item| (item, depth + 1))),
            Value::Object(object) => {
                if object.keys().any(|key| key.len() > MAX_JSON_STRING_BYTES) {
                    return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
                }
                stack.extend(object.values().map(|item| (item, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

fn checked_sleep(
    delay: Duration,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<(), ProviderError> {
    let delay = delay.min(deadline.saturating_duration_since(Instant::now()));
    let started = std::time::Instant::now();
    while started.elapsed() < delay {
        check_operation(context, deadline)?;
        std::thread::sleep(IO_POLL.min(delay.saturating_sub(started.elapsed())));
    }
    Ok(())
}

fn check_operation(context: &ExecutionContext, deadline: Instant) -> Result<(), ProviderError> {
    context.checkpoint().map_err(map_context_error)?;
    if Instant::now() >= deadline {
        Err(ProviderError::new(ProviderErrorCode::Timeout))
    } else {
        Ok(())
    }
}

fn blocking_slice(
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<Duration, ProviderError> {
    check_operation(context, deadline)?;
    let local = deadline.saturating_duration_since(Instant::now());
    let request = context.remaining_time().unwrap_or(IO_POLL);
    Ok(IO_POLL.min(local).min(request).max(Duration::from_millis(1)))
}

#[allow(clippy::needless_pass_by_value)]
fn map_context_error(error: ConversionError) -> ProviderError {
    match error {
        ConversionError::Timeout => ProviderError::new(ProviderErrorCode::Timeout),
        ConversionError::Cancelled => ProviderError::new(ProviderErrorCode::Cancelled),
        ConversionError::ResourceLimit { .. } => {
            ProviderError::new(ProviderErrorCode::ResourceLimit)
        }
        _ => ProviderError::new(ProviderErrorCode::InvalidResponse),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn map_connect_error(error: io::Error) -> ProviderError {
    match error.kind() {
        io::ErrorKind::TimedOut => ProviderError::new(ProviderErrorCode::Timeout),
        _ => ProviderError::new(ProviderErrorCode::Connect),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits};
    use std::net::TcpListener;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    #[test]
    fn canonical_configuration_rejects_ambiguous_or_secret_bearing_urls() {
        for url in [
            "HTTPS://example.com/v1",
            "https://user@example.com/v1",
            "https://example.com/v1?key=canary",
            "https://example.com/v1#x",
            "file:///tmp/x",
        ] {
            assert_eq!(
                ProviderConfig::parse(url, "m", "API_KEY", Duration::from_secs(1), [])
                    .unwrap_err()
                    .code(),
                ProviderErrorCode::InvalidConfiguration
            );
        }
    }

    #[test]
    fn address_classification_denies_special_ranges() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "192.0.2.1",
            "198.18.0.1",
            "203.0.113.1",
            "224.0.0.1",
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(!is_public_ip(address.parse().unwrap()), "{address}");
        }
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn network_is_checked_before_dns_or_secret() {
        let config = ProviderConfig::parse(
            "https://example.com/v1",
            "m",
            "MISSING_CANARY",
            Duration::from_secs(1),
            [],
        )
        .unwrap();
        let client = OpenAiCompatibleClient::new(
            config,
            ProviderNetworkPolicy {
                allow_network: false,
                allow_private_network: false,
                allowed_hosts: vec![],
            },
        );
        assert_eq!(client.test(&context()).unwrap_err().code(), ProviderErrorCode::NetworkDenied);
    }

    #[test]
    fn parser_rejects_active_content_type_and_oversized_json_shape() {
        assert!(!valid_json_content_type("text/html"));
        assert!(!valid_json_content_type("application/json; charset=iso-8859-1"));
        assert!(valid_json_content_type("application/json; charset=utf-8"));
        let mut nested = Value::Null;
        for _ in 0..=MAX_JSON_DEPTH {
            nested = Value::Array(vec![nested]);
        }
        assert_eq!(
            validate_json_bounds(&nested).unwrap_err().code(),
            ProviderErrorCode::ResponseTooLarge
        );
    }

    #[test]
    fn secret_rejects_empty_control_and_oversized_values_without_echoing() {
        for value in [
            OsString::new(),
            OsString::from("contains space"),
            OsString::from("x".repeat(MAX_SECRET_BYTES + 1)),
        ] {
            let error = Secret::from_os_string(value, &context()).err().unwrap();
            assert_eq!(error.code(), ProviderErrorCode::SecretInvalid);
            assert_eq!(error.to_string(), "providerSecretInvalid");
        }
    }

    #[cfg(unix)]
    #[test]
    fn secret_rejects_non_unicode_environment_bytes() {
        use std::os::unix::ffi::OsStringExt as _;
        let value = OsString::from_vec(vec![0xff, 0xfe]);
        assert_eq!(
            Secret::from_os_string(value, &context()).err().unwrap().code(),
            ProviderErrorCode::SecretInvalid
        );
    }

    struct FixedResolver(Vec<SocketAddr>);

    impl Resolver for FixedResolver {
        fn resolve(&self, _: &str, _: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn mixed_public_and_private_dns_answer_fails_closed_before_secret_lookup() {
        let config = ProviderConfig::parse(
            "https://provider.example/v1",
            "m",
            "MISSING_DNS_REBINDING_CANARY",
            Duration::from_secs(1),
            [],
        )
        .unwrap();
        let mut client = OpenAiCompatibleClient::new(
            config,
            ProviderNetworkPolicy {
                allow_network: true,
                allow_private_network: false,
                allowed_hosts: vec!["provider.example".into()],
            },
        );
        client.resolver = Arc::new(FixedResolver(vec![
            "8.8.8.8:443".parse().unwrap(),
            "127.0.0.1:443".parse().unwrap(),
            "8.8.8.8:443".parse().unwrap(),
        ]));
        assert_eq!(
            client.test(&context()).unwrap_err().code(),
            ProviderErrorCode::PrivateNetworkDenied
        );
    }

    #[test]
    fn explicit_loopback_test_sends_only_fixed_models_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            assert!(request.starts_with(b"GET /v1/models HTTP/1.1\r\n"));
            assert!(!request.windows(8).any(|window| window == b"document"));
            assert!(!request.windows(6).any(|window| window == b"prompt"));
            let body = br#"{"object":"list","data":[{"id":"configured"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });
        let config = ProviderConfig::parse(
            &format!("http://{address}/v1"),
            "configured",
            "PATH",
            Duration::from_secs(2),
            ["image-description".into()],
        )
        .unwrap();
        let client = OpenAiCompatibleClient::new(
            config,
            ProviderNetworkPolicy {
                allow_network: true,
                allow_private_network: true,
                allowed_hosts: vec!["127.0.0.1".into()],
            },
        );
        let result = client.test(&context()).unwrap();
        assert!(result.configured_model_available);
        assert_eq!(result.capabilities, ["image-description"]);
        worker.join().unwrap();
    }
}

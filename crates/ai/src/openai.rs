//! Bounded, direct OpenAI-compatible transport.
//!
//! The implementation deliberately does not consult proxy environment variables,
//! platform certificate stores, `PATH`, or provider-supplied redirect locations.

use base64::Engine as _;
use flate2::read::GzDecoder;
use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation};
use rustls::pki_types::ServerName;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{self, Cursor, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use url::{Host, Url};
use zeroize::Zeroize;

const MAX_SECRET_BYTES: usize = 16 * 1024;
const MAX_BASE_URL_BYTES: usize = 4 * 1024;
const MAX_ENVIRONMENT_NAME_BYTES: usize = 256;
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
const IO_POLL: Duration = Duration::from_millis(10);
const MAX_RETRIES: u8 = 2;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(2);
const MAX_DNS_ADDRESSES: usize = 64;
const DNS_WORKERS: usize = 4;
const DNS_QUEUE_PER_WORKER: usize = 2;
const MAX_CHUNKS: usize = 4_096;
static RETRY_JITTER_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

type RetryJitter = fn(Duration, u8) -> Duration;

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
    /// A bounded discovery response declared additional pages.
    Incomplete,
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
            ProviderErrorCode::Incomplete => "providerIncomplete",
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
        if base_url.len() > MAX_BASE_URL_BYTES {
            return Err(ProviderError::new(ProviderErrorCode::InvalidConfiguration));
        }
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
            || model.chars().any(char::is_control)
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
    if value.len() > MAX_ENVIRONMENT_NAME_BYTES {
        return false;
    }
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

struct Secret {
    bytes: Vec<u8>,
}

struct SecretCandidate {
    bytes: Vec<u8>,
}

impl Secret {
    fn from_environment(name: &str, memory: &mut MemoryBudget) -> Result<Self, ProviderError> {
        let value = std::env::var_os(name)
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::SecretMissing))?;
        Self::from_os_string(value, memory)
    }

    fn from_os_string(value: OsString, memory: &mut MemoryBudget) -> Result<Self, ProviderError> {
        SecretCandidate::from_os_string(value).into_secret(memory)
    }
}

impl SecretCandidate {
    fn from_os_string(value: OsString) -> Self {
        // Consume the environment-owned allocation immediately. The candidate is
        // subsequently held only by a zeroizing byte owner, including invalid
        // Unicode and all validation/allocation failure paths.
        Self { bytes: value.into_encoded_bytes() }
    }

    fn into_secret(mut self, memory: &mut MemoryBudget) -> Result<Secret, ProviderError> {
        validate_secret_candidate(&mut self.bytes, memory)?;
        Ok(Secret { bytes: std::mem::take(&mut self.bytes) })
    }
}

fn validate_secret_candidate(
    bytes: &mut Vec<u8>,
    memory: &mut MemoryBudget,
) -> Result<(), ProviderError> {
    let valid = std::str::from_utf8(bytes).is_ok()
        && !bytes.is_empty()
        && bytes.len() <= MAX_SECRET_BYTES
        && !bytes.iter().any(|byte| *byte <= 0x20 || *byte == 0x7f);
    if !valid {
        bytes.zeroize();
        return Err(ProviderError::new(ProviderErrorCode::SecretInvalid));
    }
    if let Err(error) = memory.grow(bytes.capacity()) {
        bytes.zeroize();
        return Err(error);
    }
    Ok(())
}

struct MemoryBudget {
    reservation: ResourceReservation,
    held: usize,
}

impl MemoryBudget {
    fn new(context: &ExecutionContext) -> Result<Self, ProviderError> {
        context
            .reserve_memory(0)
            .map(|reservation| Self { reservation, held: 0 })
            .map_err(map_context_error)
    }

    fn grow(&mut self, bytes: usize) -> Result<(), ProviderError> {
        self.reservation
            .grow(
                u64::try_from(bytes)
                    .map_err(|_| ProviderError::new(ProviderErrorCode::ResourceLimit))?,
            )
            .map_err(map_context_error)?;
        self.held = self
            .held
            .checked_add(bytes)
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::ResourceLimit))?;
        Ok(())
    }

    fn shrink(&mut self, bytes: usize) -> Result<(), ProviderError> {
        let held = self
            .held
            .checked_sub(bytes)
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::ResourceLimit))?;
        self.reservation
            .shrink(
                u64::try_from(bytes)
                    .map_err(|_| ProviderError::new(ProviderErrorCode::ResourceLimit))?,
            )
            .map_err(map_context_error)?;
        self.held = held;
        Ok(())
    }

    fn checkpoint(&self) -> usize {
        self.held
    }

    fn restore(&mut self, checkpoint: usize) -> Result<(), ProviderError> {
        let release = self
            .held
            .checked_sub(checkpoint)
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::ResourceLimit))?;
        self.shrink(release)
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl Drop for SecretCandidate {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

trait Resolver: Send + Sync {
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        (host, port)
            .to_socket_addrs()
            .map(|addresses| addresses.take(MAX_DNS_ADDRESSES + 1).collect())
    }
}

struct DnsJob {
    resolver: Arc<dyn Resolver>,
    host: String,
    port: u16,
    result: SyncSender<io::Result<Vec<SocketAddr>>>,
}

struct DnsPool {
    workers: Vec<SyncSender<DnsJob>>,
    next: AtomicUsize,
}

impl DnsPool {
    fn start() -> Option<Self> {
        let mut workers = Vec::with_capacity(DNS_WORKERS);
        for index in 0..DNS_WORKERS {
            let (sender, receiver) = std::sync::mpsc::sync_channel::<DnsJob>(DNS_QUEUE_PER_WORKER);
            if std::thread::Builder::new()
                .name(format!("into-md-provider-dns-{index}"))
                .spawn(move || {
                    while let Ok(job) = receiver.recv() {
                        let result = job.resolver.resolve(&job.host, job.port);
                        let _ = job.result.send(result);
                    }
                })
                .is_ok()
            {
                workers.push(sender);
            }
        }
        (!workers.is_empty()).then(|| Self { workers, next: AtomicUsize::new(0) })
    }

    fn submit(&self, mut job: DnsJob) -> Result<(), ProviderError> {
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        for offset in 0..self.workers.len() {
            let index = (start + offset) % self.workers.len();
            match self.workers[index].try_send(job) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(returned) | TrySendError::Disconnected(returned)) => {
                    job = returned;
                }
            }
        }
        Err(ProviderError::new(ProviderErrorCode::Dns))
    }
}

static DNS_POOL: OnceLock<Option<DnsPool>> = OnceLock::new();

/// Successful minimal connectivity/capability result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTestResult {
    /// Stable output schema.
    pub schema_version: u32,
    /// The configured model was listed by the endpoint.
    pub configured_model_available: bool,
    /// Capabilities proved by the server and retained by the configured allowlist.
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
    retry_jitter: RetryJitter,
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
        Self {
            config,
            policy,
            resolver: Arc::new(SystemResolver),
            retry_jitter: process_retry_jitter,
        }
    }

    /// Execute a minimal `GET /models` test. It sends no document or user content.
    ///
    /// # Errors
    ///
    /// Returns a stable [`ProviderError`] for policy, transport, HTTP, or schema failures.
    pub fn test(&self, context: &ExecutionContext) -> Result<ProviderTestResult, ProviderError> {
        let mut memory = MemoryBudget::new(context)?;
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
        let addresses = resolve_with_memory(
            self.resolver.clone(),
            host.clone(),
            port,
            context,
            deadline,
            &mut memory,
        )?;
        if addresses.is_empty() {
            return Err(ProviderError::new(ProviderErrorCode::Dns));
        }
        for address in &addresses {
            self.check_address(address.ip())?;
        }
        // Secret lookup is deliberately after URL, host, DNS, and address authorization.
        let secret =
            Secret::from_environment(&self.config.api_key_environment_variable, &mut memory)?;
        let mut retry = 0_u8;
        loop {
            check_operation(context, deadline)?;
            let response = Self::request_models(
                &endpoint,
                &host,
                &addresses,
                &secret,
                context,
                deadline,
                &mut memory,
            )?;
            if retry < MAX_RETRIES && matches!(response.status, 429 | 500..=599) {
                retry += 1;
                let delay = retry_delay(response.retry_after, retry, self.retry_jitter);
                memory.shrink(response.body.capacity())?;
                checked_sleep(delay.min(MAX_RETRY_AFTER), context, deadline)?;
                continue;
            }
            return parse_models_response(response, &self.config, context, deadline, &mut memory);
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
        let mut memory = MemoryBudget::new(context)?;
        validate_generation_request(&request, &self.config)?;
        let deadline = Instant::now()
            .checked_add(self.config.timeout)
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
        check_operation(context, deadline)?;
        if !self.policy.allow_network {
            return Err(ProviderError::new(ProviderErrorCode::NetworkDenied));
        }
        let estimated_body = generation_body_bound(&request, self.config.model.len())?;
        let encoding_budget = estimated_body.saturating_mul(3);
        memory.grow(encoding_budget)?;
        let (path, body) = encode_generation_request(&request, &self.config.model)?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err(ProviderError::new(ProviderErrorCode::ResourceLimit));
        }
        if body.capacity() > encoding_budget {
            memory.grow(body.capacity() - encoding_budget)?;
        } else {
            memory.shrink(encoding_budget - body.capacity())?;
        }
        let mut endpoint = self.config.base_url.clone();
        endpoint.set_path(&format!("{}{}", endpoint.path().trim_end_matches('/'), path));
        let host = canonical_host(&endpoint)?;
        self.check_host(&host)?;
        let port = endpoint
            .port_or_known_default()
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
        let addresses = resolve_with_memory(
            self.resolver.clone(),
            host.clone(),
            port,
            context,
            deadline,
            &mut memory,
        )?;
        if addresses.is_empty() {
            return Err(ProviderError::new(ProviderErrorCode::Dns));
        }
        for address in &addresses {
            self.check_address(address.ip())?;
        }
        let secret =
            Secret::from_environment(&self.config.api_key_environment_variable, &mut memory)?;
        let spec = RequestSpec {
            method: "POST",
            body: Some(&body),
            idempotency_key: request.idempotency_key,
        };
        let mut retry = 0_u8;
        loop {
            let response = Self::request(
                &endpoint,
                &host,
                &addresses,
                &secret,
                context,
                deadline,
                spec,
                &mut memory,
            )?;
            if request.idempotency_key.is_some()
                && retry < MAX_RETRIES
                && matches!(response.status, 429 | 500..=599)
            {
                retry += 1;
                memory.shrink(response.body.capacity())?;
                checked_sleep(
                    retry_delay(response.retry_after, retry, self.retry_jitter),
                    context,
                    deadline,
                )?;
                continue;
            }
            let mut result = parse_generation_response(
                response,
                request.endpoint,
                context,
                deadline,
                &mut memory,
            )?;
            if contains_secret(
                result.text.as_bytes(),
                &secret.bytes,
                context,
                deadline,
                &mut memory,
            )? {
                result.text.zeroize();
                return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
            }
            return Ok(result);
        }
    }

    fn request_models(
        endpoint: &Url,
        host: &str,
        addresses: &[SocketAddr],
        secret: &Secret,
        context: &ExecutionContext,
        deadline: Instant,
        memory: &mut MemoryBudget,
    ) -> Result<HttpResponse, ProviderError> {
        Self::request(
            endpoint,
            host,
            addresses,
            secret,
            context,
            deadline,
            RequestSpec { method: "GET", body: None, idempotency_key: None },
            memory,
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
        memory: &mut MemoryBudget,
    ) -> Result<HttpResponse, ProviderError> {
        let mut last = ProviderError::new(ProviderErrorCode::Connect);
        for (index, address) in addresses.iter().enumerate() {
            check_operation(context, deadline)?;
            let remaining = effective_remaining(context, deadline)?;
            let addresses_left = u32::try_from(addresses.len() - index).unwrap_or(u32::MAX);
            let address_deadline = Instant::now()
                .checked_add(remaining / addresses_left.max(1))
                .unwrap_or(deadline)
                .min(deadline);
            let attempt_checkpoint = memory.checkpoint();
            let attempt = Self::request_one(
                endpoint,
                host,
                *address,
                secret,
                context,
                deadline,
                address_deadline,
                spec,
                memory,
            );
            match attempt {
                Ok(response) => return Ok(response),
                Err(failure)
                    if !failure.progressed()
                        && failure.error.code() == ProviderErrorCode::Connect
                        && index + 1 < addresses.len() =>
                {
                    memory.restore(attempt_checkpoint)?;
                    last = failure.error;
                }
                Err(failure) => {
                    memory.restore(attempt_checkpoint)?;
                    return Err(failure.error);
                }
            }
        }
        Err(last)
    }

    #[allow(clippy::too_many_arguments)]
    fn request_one(
        endpoint: &Url,
        host: &str,
        address: SocketAddr,
        secret: &Secret,
        context: &ExecutionContext,
        deadline: Instant,
        address_deadline: Instant,
        spec: RequestSpec<'_>,
        memory: &mut MemoryBudget,
    ) -> Result<HttpResponse, AttemptFailure> {
        let stream = connect_checked(address, context, address_deadline)
            .map_err(AttemptFailure::before_request)?;
        let stream: Box<dyn ReadWrite> = if endpoint.scheme() == "https" {
            Box::new(
                tls_handshake(stream, host, context, address_deadline)
                    .map_err(AttemptFailure::before_request)?,
            )
        } else {
            Box::new(stream)
        };
        let mut stream = TrackedIo::new(stream);
        let host_header = host_header(endpoint, host)?;
        let target = request_target(endpoint)?;
        let request_capacity = 512_usize
            .checked_add(target.len())
            .and_then(|size| size.checked_add(host_header.len()))
            .and_then(|size| size.checked_add(secret.bytes.len()))
            .and_then(|size| size.checked_add(spec.idempotency_key.map_or(0, str::len)))
            .and_then(|size| size.checked_add(spec.body.map_or(0, <[u8]>::len)))
            .filter(|size| *size <= MAX_REQUEST_BYTES)
            .ok_or_else(|| ProviderError::new(ProviderErrorCode::ResourceLimit))?;
        memory.grow(request_capacity).map_err(AttemptFailure::before_request)?;
        let mut request = Vec::with_capacity(request_capacity);
        if request.capacity() > request_capacity {
            memory
                .grow(request.capacity() - request_capacity)
                .map_err(AttemptFailure::before_request)?;
        }
        let allocated_request = request.capacity();
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
            return Err(ProviderError::new(ProviderErrorCode::ResourceLimit).into());
        }
        if request.capacity() > allocated_request
            && let Err(error) = memory.grow(request.capacity() - allocated_request)
        {
            request.zeroize();
            return Err(AttemptFailure::before_request(error));
        }
        let allocated_request = request.capacity();
        let result = write_all_checked(&mut stream, &request, context, deadline)
            .and_then(|()| read_response(&mut stream, context, deadline, memory));
        request.zeroize();
        let release = memory.shrink(allocated_request);
        if let Err(error) = release {
            return Err(AttemptFailure {
                error,
                request_bytes: stream.written,
                response_bytes: stream.read,
            });
        }
        result.map_err(|error| AttemptFailure {
            error,
            request_bytes: stream.written,
            response_bytes: stream.read,
        })
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
    let pool = DNS_POOL
        .get_or_init(DnsPool::start)
        .as_ref()
        .ok_or_else(|| ProviderError::new(ProviderErrorCode::Dns))?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    pool.submit(DnsJob { resolver, host, port, result: sender })?;
    loop {
        check_operation(context, deadline)?;
        match receiver.recv_timeout(blocking_slice(context, deadline)?) {
            Ok(Ok(addresses)) => return validate_dns_addresses(addresses, port),
            Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(ProviderError::new(ProviderErrorCode::Dns));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn resolve_with_memory(
    resolver: Arc<dyn Resolver>,
    host: String,
    port: u16,
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut MemoryBudget,
) -> Result<Vec<SocketAddr>, ProviderError> {
    let reserved = MAX_DNS_ADDRESSES
        .checked_mul(std::mem::size_of::<SocketAddr>())
        .ok_or_else(|| ProviderError::new(ProviderErrorCode::ResourceLimit))?;
    memory.grow(reserved)?;
    let addresses = match resolve_checked(resolver, host, port, context, deadline) {
        Ok(addresses) => addresses,
        Err(error) => {
            memory.shrink(reserved)?;
            return Err(error);
        }
    };
    let actual = addresses
        .capacity()
        .checked_mul(std::mem::size_of::<SocketAddr>())
        .ok_or_else(|| ProviderError::new(ProviderErrorCode::ResourceLimit))?;
    memory.shrink(reserved.saturating_sub(actual))?;
    Ok(addresses)
}

fn validate_dns_addresses(
    mut addresses: Vec<SocketAddr>,
    port: u16,
) -> Result<Vec<SocketAddr>, ProviderError> {
    if addresses.is_empty()
        || addresses.len() > MAX_DNS_ADDRESSES
        || addresses.capacity() > MAX_DNS_ADDRESSES
        || addresses.iter().any(|address| address.port() != port)
    {
        return Err(ProviderError::new(ProviderErrorCode::Dns));
    }
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        Err(ProviderError::new(ProviderErrorCode::Dns))
    } else {
        Ok(addresses)
    }
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

struct TrackedIo {
    inner: Box<dyn ReadWrite>,
    written: usize,
    read: usize,
}

impl TrackedIo {
    fn new(inner: Box<dyn ReadWrite>) -> Self {
        Self { inner, written: 0, read: 0 }
    }
}

impl Read for TrackedIo {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.read = self.read.saturating_add(read);
        Ok(read)
    }
}

impl Write for TrackedIo {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.written = self.written.saturating_add(written);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct AttemptFailure {
    error: ProviderError,
    request_bytes: usize,
    response_bytes: usize,
}

impl AttemptFailure {
    fn before_request(error: ProviderError) -> Self {
        Self { error, request_bytes: 0, response_bytes: 0 }
    }

    fn progressed(&self) -> bool {
        self.request_bytes != 0 || self.response_bytes != 0
    }
}

impl From<ProviderError> for AttemptFailure {
    fn from(error: ProviderError) -> Self {
        Self::before_request(error)
    }
}

#[derive(Clone, Copy)]
struct RequestSpec<'a> {
    method: &'static str,
    body: Option<&'a [u8]>,
    idempotency_key: Option<&'a str>,
}

fn connect_checked(
    address: SocketAddr,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<TcpStream, ProviderError> {
    let socket = Socket::new(Domain::for_address(address), Type::STREAM, Some(Protocol::TCP))
        .map_err(|_| ProviderError::new(ProviderErrorCode::Connect))?;
    socket.set_nonblocking(true).map_err(|_| ProviderError::new(ProviderErrorCode::Connect))?;
    match socket.connect(&SockAddr::from(address)) {
        Ok(()) => {}
        Err(error) if connect_is_pending(&error) => loop {
            check_operation(context, deadline)?;
            if socket
                .take_error()
                .map_err(|_| ProviderError::new(ProviderErrorCode::Connect))?
                .is_some()
            {
                return Err(ProviderError::new(ProviderErrorCode::Connect));
            }
            if socket.peer_addr().is_ok() {
                break;
            }
            std::thread::sleep(blocking_slice(context, deadline)?);
        },
        Err(_) => return Err(ProviderError::new(ProviderErrorCode::Connect)),
    }
    check_operation(context, deadline)?;
    let stream = TcpStream::from(socket);
    stream.set_nodelay(true).map_err(|_| ProviderError::new(ProviderErrorCode::Connect))?;
    Ok(stream)
}

fn connect_is_pending(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
        || matches!(error.raw_os_error(), Some(36 | 115 | 10035))
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots =
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect::<rustls::RootCertStore>();
            Arc::new(
                rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth(),
            )
        })
        .clone()
}

fn tls_handshake(
    mut stream: TcpStream,
    host: &str,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>, ProviderError> {
    let server_name = ServerName::try_from(host.to_owned())
        .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidConfiguration))?;
    let mut connection = rustls::ClientConnection::new(tls_config(), server_name)
        .map_err(|_| ProviderError::new(ProviderErrorCode::Tls))?;
    while connection.is_handshaking() {
        check_operation(context, deadline)?;
        match connection.complete_io(&mut stream) {
            Ok(_) => {}
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock) => {
                std::thread::sleep(blocking_slice(context, deadline)?);
            }
            Err(_) => return Err(ProviderError::new(ProviderErrorCode::Tls)),
        }
    }
    Ok(rustls::StreamOwned::new(connection, stream))
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

#[allow(clippy::too_many_lines)]
fn encode_generation_request(
    request: &GenerationRequest<'_>,
    model: &str,
) -> Result<(&'static str, Vec<u8>), ProviderError> {
    #[derive(Serialize)]
    struct ChatRequest<'a> {
        model: &'a str,
        messages: [ChatMessage<'a>; 1],
        max_tokens: u32,
    }
    #[derive(Serialize)]
    struct ChatMessage<'a> {
        role: &'static str,
        content: ChatContent<'a>,
    }
    #[derive(Serialize)]
    #[serde(untagged)]
    enum ChatContent<'a> {
        Text(&'a str),
        Parts([ChatPart<'a>; 2]),
    }
    #[derive(Serialize)]
    #[serde(tag = "type")]
    enum ChatPart<'a> {
        #[serde(rename = "text")]
        Text { text: &'a str },
        #[serde(rename = "image_url")]
        ImageUrl { image_url: ChatImageUrl<'a> },
    }
    #[derive(Serialize)]
    struct ChatImageUrl<'a> {
        url: &'a str,
    }
    #[derive(Serialize)]
    struct ResponsesRequest<'a> {
        model: &'a str,
        input: [ResponsesMessage<'a>; 1],
        max_output_tokens: u32,
    }
    #[derive(Serialize)]
    struct ResponsesMessage<'a> {
        role: &'static str,
        content: ResponsesContent<'a>,
    }
    #[derive(Serialize)]
    #[serde(untagged)]
    enum ResponsesContent<'a> {
        Text([ResponsesText<'a>; 1]),
        Image([ResponsesPart<'a>; 2]),
    }
    #[derive(Serialize)]
    struct ResponsesText<'a> {
        r#type: &'static str,
        text: &'a str,
    }
    #[derive(Serialize)]
    #[serde(tag = "type")]
    enum ResponsesPart<'a> {
        #[serde(rename = "input_text")]
        Text { text: &'a str },
        #[serde(rename = "input_image")]
        Image { image_url: &'a str },
    }

    let image_url = match request.input {
        GenerationInput::Image { bytes, media_type, .. } => Some(format!(
            "data:{media_type};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )),
        GenerationInput::Text(_) => None,
    };
    let body = match request.endpoint {
        GenerationEndpoint::ChatCompletions => {
            let content = match (request.input, image_url.as_deref()) {
                (GenerationInput::Text(text), None) => ChatContent::Text(text),
                (GenerationInput::Image { prompt, .. }, Some(url)) => ChatContent::Parts([
                    ChatPart::Text { text: prompt },
                    ChatPart::ImageUrl { image_url: ChatImageUrl { url } },
                ]),
                _ => return Err(ProviderError::new(ProviderErrorCode::InvalidConfiguration)),
            };
            serde_json::to_vec(&ChatRequest {
                model,
                messages: [ChatMessage { role: "user", content }],
                max_tokens: request.max_output_tokens,
            })
            .map(|body| ("/chat/completions", body))
        }
        GenerationEndpoint::Responses => {
            let content = match (request.input, image_url.as_deref()) {
                (GenerationInput::Text(text), None) => {
                    ResponsesContent::Text([ResponsesText { r#type: "input_text", text }])
                }
                (GenerationInput::Image { prompt, .. }, Some(image_url)) => {
                    ResponsesContent::Image([
                        ResponsesPart::Text { text: prompt },
                        ResponsesPart::Image { image_url },
                    ])
                }
                _ => return Err(ProviderError::new(ProviderErrorCode::InvalidConfiguration)),
            };
            serde_json::to_vec(&ResponsesRequest {
                model,
                input: [ResponsesMessage { role: "user", content }],
                max_output_tokens: request.max_output_tokens,
            })
            .map(|body| ("/responses", body))
        }
    };
    body.map_err(|_| ProviderError::new(ProviderErrorCode::InvalidConfiguration))
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
                || (a == 192 && b == 88 && c == 99)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            if ip.to_ipv4_mapped().is_some() {
                return false;
            }
            let segments = ip.segments();
            (segments[0] & 0xe000) == 0x2000
                // IANA special-purpose aggregate (Teredo, ORCHID, benchmarking,
                // documentation, and protocol assignments).
                && !(segments[0] == 0x2001 && segments[1] <= 0x01ff)
                && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
                // 6to4 embeds an IPv4 address and is not accepted as global-only.
                && segments[0] != 0x2002
                // Documentation and segment-routing special-purpose blocks.
                && !(segments[0] == 0x3fff && (segments[1] & 0xfff0) == 0)
        }
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    retry_after: Option<Duration>,
    body: Vec<u8>,
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
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                std::thread::sleep(blocking_slice(context, deadline)?);
            }
            Err(error) => return Err(map_stream_error(&error)),
        }
    }
    stream.flush().map_err(|error| map_stream_error(&error))
}

#[allow(clippy::too_many_lines)]
fn read_response(
    stream: &mut dyn ReadWrite,
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut MemoryBudget,
) -> Result<HttpResponse, ProviderError> {
    let header = read_until(stream, b"\r\n\r\n", MAX_HEADER_BYTES, context, deadline, memory)?;
    let text = std::str::from_utf8(&header)
        .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    let mut lines = text[..text.len() - 4].split("\r\n");
    let status_line =
        lines.next().ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    let status_bytes = status_line.as_bytes();
    if status_bytes.len() < 13
        || &status_bytes[..9] != b"HTTP/1.1 "
        || !status_bytes[9..12].iter().all(u8::is_ascii_digit)
        || status_bytes[12] != b' '
        || !valid_field_value(&status_bytes[13..])
    {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    let status = std::str::from_utf8(&status_bytes[9..12])
        .ok()
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
        if name.is_empty()
            || name.trim() != name
            || !name.bytes().all(is_tchar)
            || !valid_field_value(value.as_bytes())
        {
            return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
        }
        let name = name.to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => {
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
                }
                let parsed = value
                    .parse::<usize>()
                    .map_err(|_| ProviderError::new(ProviderErrorCode::ResponseTooLarge))?;
                if parsed > MAX_RESPONSE_BYTES {
                    return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
                }
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
                retry_after = Some(parse_retry_after(value)?);
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
    if saw_transfer_encoding && content_length.is_some() {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    let header_capacity = header.capacity();
    drop(header);
    memory.shrink(header_capacity)?;
    let compressed = if chunked {
        read_chunked(stream, context, deadline, memory)?
    } else if let Some(length) = content_length {
        read_exact_bounded(stream, length, context, deadline, memory)?
    } else {
        read_to_eof_bounded(stream, context, deadline, memory)?
    };
    let body = if gzip {
        let body = decompress_gzip(&compressed, context, deadline, memory)?;
        let compressed_capacity = compressed.capacity();
        drop(compressed);
        memory.shrink(compressed_capacity)?;
        body
    } else {
        compressed
    };
    let error_status = matches!(status, 401 | 403 | 404 | 408 | 409 | 429 | 500..=599);
    let body_or_framing =
        chunked || content_length.is_some() || saw_content_encoding || !body.is_empty();
    if (200..=299).contains(&status) && body_or_framing && !json {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    if error_status && body_or_framing && !json {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    if matches!(status, 204 | 304)
        && (chunked || content_length.is_some() || saw_content_encoding || !body.is_empty())
    {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    Ok(HttpResponse { status, retry_after, body })
}

fn valid_json_content_type(value: &str) -> bool {
    let mut parts = value.split(';');
    let media = parts.next().unwrap_or("").trim();
    if !(media.eq_ignore_ascii_case("application/json")
        || media.to_ascii_lowercase().ends_with("+json"))
    {
        return false;
    }
    let mut saw_charset = false;
    parts.all(|part| {
        let part = part.trim();
        let charset = part.eq_ignore_ascii_case("charset=utf-8")
            || part.eq_ignore_ascii_case("charset=\"utf-8\"");
        if part.is_empty() || !charset || saw_charset {
            return false;
        }
        saw_charset = true;
        true
    })
}

fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn valid_field_value(value: &[u8]) -> bool {
    value.iter().all(|byte| *byte == b'\t' || *byte >= 0x20 && *byte != 0x7f)
}

fn parse_retry_after(value: &str) -> Result<Duration, ProviderError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    let delay = value
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .filter(|delay| *delay <= MAX_RETRY_AFTER)
        .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    Ok(delay)
}

fn retry_delay(retry_after: Option<Duration>, attempt: u8, jitter: RetryJitter) -> Duration {
    retry_after.unwrap_or_else(|| {
        let exponential = Duration::from_millis(
            100_u64.saturating_mul(1_u64.checked_shl(u32::from(attempt)).unwrap_or(u64::MAX)),
        );
        jitter(exponential, attempt).min(MAX_RETRY_AFTER)
    })
}

fn process_retry_jitter(base: Duration, attempt: u8) -> Duration {
    let ceiling = u64::try_from(base.as_millis() / 4).unwrap_or(u64::MAX);
    if ceiling == 0 {
        return base;
    }
    let sequence =
        u64::try_from(RETRY_JITTER_SEQUENCE.fetch_add(1, Ordering::Relaxed)).unwrap_or(u64::MAX);
    let mixed = sequence
        .wrapping_add(u64::from(std::process::id()))
        .wrapping_add(u64::from(attempt))
        .wrapping_mul(0x9e37_79b9_7f4a_7c15);
    base.saturating_add(Duration::from_millis(mixed % ceiling.saturating_add(1)))
}

fn read_until(
    stream: &mut dyn ReadWrite,
    delimiter: &[u8],
    limit: usize,
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut MemoryBudget,
) -> Result<Vec<u8>, ProviderError> {
    let mut output = Vec::new();
    while output.len() < limit {
        let byte = read_one(stream, context, deadline)?;
        push_byte(&mut output, byte, memory)?;
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
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                std::thread::sleep(blocking_slice(context, deadline)?);
            }
            Err(error) => return Err(map_stream_error(&error)),
        }
    }
}

fn read_exact_bounded(
    stream: &mut dyn ReadWrite,
    length: usize,
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut MemoryBudget,
) -> Result<Vec<u8>, ProviderError> {
    if length > MAX_RESPONSE_BYTES {
        return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
    }
    memory.grow(length)?;
    let mut output = vec![0; length];
    if output.capacity() > length {
        memory.grow(output.capacity() - length)?;
    }
    let mut read = 0;
    while read < length {
        check_operation(context, deadline)?;
        match stream.read(&mut output[read..]) {
            Ok(0) => return Err(ProviderError::new(ProviderErrorCode::InvalidResponse)),
            Ok(count) => read += count,
            Err(error)
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                std::thread::sleep(blocking_slice(context, deadline)?);
            }
            Err(error) => return Err(map_stream_error(&error)),
        }
    }
    Ok(output)
}

fn read_to_eof_bounded(
    stream: &mut dyn ReadWrite,
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut MemoryBudget,
) -> Result<Vec<u8>, ProviderError> {
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        check_operation(context, deadline)?;
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(output),
            Ok(count) if output.len().saturating_add(count) <= MAX_RESPONSE_BYTES => {
                extend_bytes(&mut output, &buffer[..count], memory)?;
            }
            Ok(_) => return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge)),
            Err(error)
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                std::thread::sleep(blocking_slice(context, deadline)?);
            }
            Err(error) => return Err(map_stream_error(&error)),
        }
    }
}

fn read_chunked(
    stream: &mut dyn ReadWrite,
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut MemoryBudget,
) -> Result<Vec<u8>, ProviderError> {
    let mut output = Vec::new();
    let mut chunks = 0_usize;
    loop {
        let line = read_until(stream, b"\r\n", 128, context, deadline, memory)?;
        let text = std::str::from_utf8(&line[..line.len() - 2])
            .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
        if text.contains(';')
            || text.is_empty()
            || text.len() > 16
            || !text.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
        }
        let size_text = text;
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
        let line_capacity = line.capacity();
        drop(line);
        memory.shrink(line_capacity)?;
        if size == 0 {
            let end = read_until(stream, b"\r\n", MAX_HEADER_BYTES, context, deadline, memory)?;
            if end != b"\r\n" {
                return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
            }
            let end_capacity = end.capacity();
            drop(end);
            memory.shrink(end_capacity)?;
            return Ok(output);
        }
        chunks += 1;
        if chunks > MAX_CHUNKS {
            return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
        }
        if output.len().saturating_add(size) > MAX_RESPONSE_BYTES {
            return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
        }
        let chunk = read_exact_bounded(stream, size, context, deadline, memory)?;
        extend_bytes(&mut output, &chunk, memory)?;
        let chunk_capacity = chunk.capacity();
        drop(chunk);
        memory.shrink(chunk_capacity)?;
        let ending = read_exact_bounded(stream, 2, context, deadline, memory)?;
        if ending != b"\r\n" {
            return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
        }
        let ending_capacity = ending.capacity();
        drop(ending);
        memory.shrink(ending_capacity)?;
    }
}

fn reserve_vec(
    output: &mut Vec<u8>,
    additional: usize,
    memory: &mut MemoryBudget,
) -> Result<(), ProviderError> {
    let required = output
        .len()
        .checked_add(additional)
        .ok_or_else(|| ProviderError::new(ProviderErrorCode::ResourceLimit))?;
    if required <= output.capacity() {
        return Ok(());
    }
    let old = output.capacity();
    let target = old.saturating_mul(2).max(required);
    memory.grow(target - old)?;
    output.reserve_exact(target - output.len());
    if output.capacity() > target {
        memory.grow(output.capacity() - target)?;
    } else if output.capacity() < target {
        memory.shrink(target - output.capacity())?;
    }
    Ok(())
}

fn push_byte(
    output: &mut Vec<u8>,
    byte: u8,
    memory: &mut MemoryBudget,
) -> Result<(), ProviderError> {
    reserve_vec(output, 1, memory)?;
    output.push(byte);
    Ok(())
}

fn extend_bytes(
    output: &mut Vec<u8>,
    bytes: &[u8],
    memory: &mut MemoryBudget,
) -> Result<(), ProviderError> {
    reserve_vec(output, bytes.len(), memory)?;
    output.extend_from_slice(bytes);
    Ok(())
}

fn decompress_gzip(
    input: &[u8],
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut MemoryBudget,
) -> Result<Vec<u8>, ProviderError> {
    let mut decoder = GzDecoder::new(Cursor::new(input));
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        check_operation(context, deadline)?;
        let count = decoder
            .read(&mut chunk)
            .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
        if count == 0 {
            break;
        }
        if output.len().saturating_add(count) > MAX_DECOMPRESSED_BYTES {
            return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
        }
        extend_bytes(&mut output, &chunk[..count], memory)?;
    }
    Ok(output)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsListWire {
    object: String,
    data: Vec<ModelWire>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelWire {
    id: String,
    object: String,
    created: u64,
    owned_by: String,
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
fn parse_models_response(
    response: HttpResponse,
    config: &ProviderConfig,
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut MemoryBudget,
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
    let parsed: ParsedJson<ModelsListWire> =
        parse_json_bounded(&response.body, context, deadline, memory)?;
    let mut list = parsed.value;
    if list.object != "list" {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    if list.has_more {
        return Err(ProviderError::new(ProviderErrorCode::Incomplete));
    }
    if list.data.len() > MAX_MODELS {
        return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
    }
    for model in &list.data {
        if model.object != "model"
            || model.id.is_empty()
            || model.id.len() > MAX_MODEL_ID_BYTES
            || model.id.chars().any(char::is_control)
            || model.owned_by.is_empty()
            || model.owned_by.len() > MAX_MODEL_ID_BYTES
            || model.owned_by.chars().any(char::is_control)
        {
            return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
        }
        let _ = model.created;
    }
    list.data.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    if list.data.windows(2).any(|models| models[0].id == models[1].id) {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    let configured_model_available =
        list.data.binary_search_by(|model| model.id.as_str().cmp(config.model.as_str())).is_ok();
    let model_count = list.data.len();
    memory.shrink(parsed.reserved.saturating_add(response.body.capacity()))?;
    Ok(ProviderTestResult {
        schema_version: 1,
        configured_model_available,
        // `/models` proves model presence only. It does not prove multimodal or
        // repair capabilities, so the negotiated set remains empty.
        capabilities: Vec::new(),
        model_count,
    })
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
fn parse_generation_response(
    response: HttpResponse,
    endpoint: GenerationEndpoint,
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut MemoryBudget,
) -> Result<GenerationResult, ProviderError> {
    ensure_success_status(response.status)?;
    let text = match endpoint {
        GenerationEndpoint::ChatCompletions => {
            let parsed: ParsedJson<ChatResponseWire> =
                parse_json_bounded(&response.body, context, deadline, memory)?;
            let text = parse_chat_text(parsed.value)?;
            memory.grow(text.capacity())?;
            memory.shrink(parsed.reserved)?;
            text
        }
        GenerationEndpoint::Responses => {
            let parsed: ParsedJson<ResponsesResponseWire> =
                parse_json_bounded(&response.body, context, deadline, memory)?;
            let text = parse_responses_text(parsed.value, memory)?;
            memory.shrink(parsed.reserved)?;
            text
        }
    };
    memory.shrink(response.body.capacity())?;
    Ok(GenerationResult { schema_version: 1, text })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatResponseWire {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Option<Vec<ChatChoiceWire>>,
    #[serde(default)]
    error: Option<ApiErrorWire>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
    #[serde(default)]
    service_tier: Option<String>,
    #[serde(default)]
    system_fingerprint: Option<String>,
    #[serde(default)]
    moderation: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatChoiceWire {
    index: usize,
    message: ChatMessageWire,
    finish_reason: String,
    #[serde(default)]
    logprobs: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatMessageWire {
    role: String,
    content: Option<String>,
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    annotations: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    audio: Option<serde_json::Value>,
    #[serde(default)]
    tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    function_call: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiErrorWire {
    message: String,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    param: Option<String>,
}

impl ApiErrorWire {
    fn is_well_formed(&self) -> bool {
        !self.message.is_empty()
            && self.message.len() <= MAX_JSON_STRING_BYTES
            && self.r#type.as_ref().is_none_or(|value| !value.is_empty())
            && self.code.as_ref().is_none_or(|value| !value.is_empty())
            && self.param.as_ref().is_none_or(|value| !value.is_empty())
    }
}

fn parse_chat_text(value: ChatResponseWire) -> Result<String, ProviderError> {
    if value.object != "chat.completion" || value.id.is_empty() || value.model.is_empty() {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    if let Some(error) = value.error {
        let _ = error.is_well_formed();
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    let _ = (
        value.created,
        value.usage,
        value.service_tier,
        value.system_fingerprint,
        value.moderation,
    );
    let mut choices =
        value.choices.ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    if choices.len() != 1 {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    let choice =
        choices.pop().ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    if choice.index != 0 {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    match choice.finish_reason.as_str() {
        "stop" => {}
        "length" | "content_filter" => {
            return Err(ProviderError::new(ProviderErrorCode::Incomplete));
        }
        _ => return Err(ProviderError::new(ProviderErrorCode::InvalidResponse)),
    }
    let _ = choice.logprobs;
    let message = choice.message;
    if message.role != "assistant"
        || message.refusal.is_some()
        || message.audio.is_some()
        || message.tool_calls.is_some()
        || message.function_call.is_some()
    {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    let _ = message.annotations;
    let content =
        message.content.ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
    if content.is_empty() || content.len() > MAX_JSON_STRING_BYTES {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    Ok(content)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponsesResponseWire {
    id: String,
    object: String,
    created_at: u64,
    status: String,
    #[serde(default)]
    completed_at: Option<u64>,
    #[serde(default)]
    error: Option<ApiErrorWire>,
    #[serde(default)]
    incomplete_details: Option<ResponsesIncompleteDetailsWire>,
    #[serde(default)]
    input: Option<serde_json::Value>,
    model: String,
    output: Vec<ResponsesOutputWire>,
    #[serde(default)]
    background: Option<bool>,
    #[serde(default)]
    conversation: Option<serde_json::Value>,
    #[serde(default)]
    instructions: Option<serde_json::Value>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    max_tool_calls: Option<u64>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    #[serde(default)]
    parallel_tool_calls: Option<bool>,
    #[serde(default)]
    previous_response_id: Option<String>,
    #[serde(default)]
    prompt: Option<serde_json::Value>,
    #[serde(default)]
    prompt_cache_key: Option<String>,
    #[serde(default)]
    reasoning: Option<serde_json::Value>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    safety_identifier: Option<String>,
    #[serde(default)]
    service_tier: Option<String>,
    #[serde(default)]
    store: Option<bool>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    text: Option<serde_json::Value>,
    #[serde(default)]
    tool_choice: Option<serde_json::Value>,
    #[serde(default)]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    top_logprobs: Option<u64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    truncation: Option<String>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
    #[serde(default)]
    user: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponsesIncompleteDetailsWire {
    reason: String,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ResponsesOutputWire {
    #[serde(rename = "message")]
    Message(ResponsesMessageWire),
    #[serde(rename = "function_call")]
    FunctionCall(ResponsesFunctionCallWire),
    #[serde(rename = "reasoning")]
    Reasoning(ResponsesReasoningWire),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponsesMessageWire {
    id: String,
    status: String,
    role: String,
    content: Vec<ResponsesOutputPartWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponsesFunctionCallWire {
    id: String,
    call_id: String,
    name: String,
    arguments: String,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponsesReasoningWire {
    id: String,
    summary: Vec<ResponsesReasoningSummaryWire>,
    #[serde(default)]
    encrypted_content: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponsesReasoningSummaryWire {
    r#type: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ResponsesOutputPartWire {
    #[serde(rename = "output_text")]
    OutputText(ResponsesOutputTextWire),
    #[serde(rename = "refusal")]
    Refusal(ResponsesRefusalWire),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponsesOutputTextWire {
    text: String,
    #[serde(default)]
    annotations: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    logprobs: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponsesRefusalWire {
    refusal: String,
}

fn parse_responses_text(
    value: ResponsesResponseWire,
    memory: &mut MemoryBudget,
) -> Result<String, ProviderError> {
    if value.object != "response" || value.id.is_empty() || value.model.is_empty() {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    if let Some(error) = value.error {
        let _ = error.is_well_formed();
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    match value.status.as_str() {
        "completed" if value.incomplete_details.is_none() => {}
        "incomplete" => {
            let details = value
                .incomplete_details
                .ok_or_else(|| ProviderError::new(ProviderErrorCode::InvalidResponse))?;
            if details.reason.is_empty()
                || details.reason.len() > MAX_JSON_STRING_BYTES
                || details.reason.chars().any(char::is_control)
            {
                return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
            }
            return Err(ProviderError::new(ProviderErrorCode::Incomplete));
        }
        _ => return Err(ProviderError::new(ProviderErrorCode::InvalidResponse)),
    }
    if let Some(truncation) = value.truncation.as_deref()
        && !matches!(truncation, "auto" | "disabled")
    {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    consume_responses_metadata(&value);
    if value.output.is_empty() || value.output.len() > 64 {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    let mut result_bytes = 0_usize;
    for item in &value.output {
        let item = match item {
            ResponsesOutputWire::Message(item) => item,
            ResponsesOutputWire::FunctionCall(call) => {
                let _ = (&call.id, &call.call_id, &call.name, &call.arguments, &call.status);
                return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
            }
            ResponsesOutputWire::Reasoning(reasoning) => {
                if reasoning.id.is_empty()
                    || reasoning.status.as_deref().is_some_and(|status| status != "completed")
                    || reasoning.summary.len() > 64
                    || reasoning.summary.iter().any(|part| {
                        part.r#type != "summary_text"
                            || part.text.is_empty()
                            || part.text.len() > MAX_JSON_STRING_BYTES
                    })
                    || reasoning
                        .encrypted_content
                        .as_ref()
                        .is_some_and(|content| content.len() > MAX_JSON_STRING_BYTES)
                {
                    return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
                }
                continue;
            }
        };
        if item.id.is_empty()
            || item.status != "completed"
            || item.role != "assistant"
            || item.content.is_empty()
            || item.content.len() > 64
        {
            return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
        }
        for part in &item.content {
            let part = match part {
                ResponsesOutputPartWire::OutputText(part) => part,
                ResponsesOutputPartWire::Refusal(refusal) => {
                    let _ = &refusal.refusal;
                    return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
                }
            };
            let _ = (&part.annotations, &part.logprobs);
            result_bytes = result_bytes.saturating_add(part.text.len());
            if result_bytes > MAX_JSON_STRING_BYTES {
                return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
            }
        }
    }
    if result_bytes == 0 {
        return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
    }
    memory.grow(result_bytes)?;
    let mut result = String::with_capacity(result_bytes);
    if result.capacity() > result_bytes {
        memory.grow(result.capacity() - result_bytes)?;
    }
    for item in value.output {
        if let ResponsesOutputWire::Message(item) = item {
            for part in item.content {
                if let ResponsesOutputPartWire::OutputText(part) = part {
                    result.push_str(&part.text);
                }
            }
        }
    }
    Ok(result)
}

fn consume_responses_metadata(value: &ResponsesResponseWire) {
    let _ = (
        value.created_at,
        &value.completed_at,
        &value.input,
        &value.background,
        &value.conversation,
        &value.instructions,
        &value.max_output_tokens,
        &value.max_tool_calls,
        &value.metadata,
        &value.parallel_tool_calls,
        &value.previous_response_id,
        &value.prompt,
        &value.prompt_cache_key,
        &value.reasoning,
        &value.reasoning_effort,
        &value.safety_identifier,
        &value.service_tier,
        &value.store,
        &value.temperature,
        &value.text,
        &value.tool_choice,
        &value.tools,
        &value.top_logprobs,
        &value.top_p,
        &value.usage,
        &value.user,
    );
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

fn contains_secret(
    haystack: &[u8],
    needle: &[u8],
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut MemoryBudget,
) -> Result<bool, ProviderError> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Ok(false);
    }
    let prefix_bytes = needle
        .len()
        .checked_mul(std::mem::size_of::<usize>())
        .ok_or_else(|| ProviderError::new(ProviderErrorCode::ResourceLimit))?;
    memory.grow(prefix_bytes)?;
    let mut prefix = vec![0_usize; needle.len()];
    if prefix.capacity() > needle.len() {
        memory.grow((prefix.capacity() - needle.len()) * std::mem::size_of::<usize>())?;
    }
    let prefix_capacity = prefix.capacity() * std::mem::size_of::<usize>();
    let mut matched = 0_usize;
    for index in 1..needle.len() {
        if index.is_multiple_of(4_096) {
            check_operation(context, deadline)?;
        }
        while matched != 0 && needle[index] != needle[matched] {
            matched = prefix[matched - 1];
        }
        if needle[index] == needle[matched] {
            matched += 1;
            prefix[index] = matched;
        }
    }
    matched = 0;
    let mut found = false;
    for (index, byte) in haystack.iter().enumerate() {
        if index.is_multiple_of(4_096) {
            check_operation(context, deadline)?;
        }
        while matched != 0 && *byte != needle[matched] {
            matched = prefix[matched - 1];
        }
        if *byte == needle[matched] {
            matched += 1;
            if matched == needle.len() {
                found = true;
                break;
            }
        }
    }
    drop(prefix);
    memory.shrink(prefix_capacity)?;
    Ok(found)
}

#[derive(Debug)]
struct ParsedJson<T> {
    value: T,
    reserved: usize,
}

fn parse_json_bounded<T: DeserializeOwned>(
    bytes: &[u8],
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut MemoryBudget,
) -> Result<ParsedJson<T>, ProviderError> {
    let values = preflight_json(bytes, context, deadline)?;
    let dom_bound = values
        .checked_mul(std::mem::size_of::<serde_json::Value>().saturating_mul(2))
        .and_then(|bound| bound.checked_add(bytes.len().saturating_mul(2)))
        .ok_or_else(|| ProviderError::new(ProviderErrorCode::ResourceLimit))?;
    memory.grow(dom_bound)?;
    serde_json::from_slice(bytes)
        .map(|value| ParsedJson { value, reserved: dom_bound })
        .map_err(|_| ProviderError::new(ProviderErrorCode::InvalidResponse))
}

fn preflight_json(
    bytes: &[u8],
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<usize, ProviderError> {
    let mut index = 0_usize;
    let mut depth = 0_usize;
    let mut values = 0_usize;
    while index < bytes.len() {
        if index.is_multiple_of(4_096) {
            check_operation(context, deadline)?;
        }
        match bytes[index] {
            b'{' | b'[' => {
                depth += 1;
                values += 1;
                if depth > MAX_JSON_DEPTH || values > MAX_JSON_VALUES {
                    return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
                }
                index += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
                }
                depth -= 1;
                index += 1;
            }
            b'"' => {
                values += 1;
                index += 1;
                let start = index;
                let mut escaped = false;
                while index < bytes.len() {
                    if index.is_multiple_of(4_096) {
                        check_operation(context, deadline)?;
                    }
                    let byte = bytes[index];
                    if !escaped && byte == b'"' {
                        break;
                    }
                    escaped = !escaped && byte == b'\\';
                    if byte != b'\\' {
                        escaped = false;
                    }
                    index += 1;
                    if index.saturating_sub(start) > MAX_JSON_STRING_BYTES {
                        return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
                    }
                }
                if index == bytes.len() || values > MAX_JSON_VALUES {
                    return Err(ProviderError::new(ProviderErrorCode::InvalidResponse));
                }
                index += 1;
            }
            b'-' | b'0'..=b'9' | b't' | b'f' | b'n' => {
                values += 1;
                if values > MAX_JSON_VALUES {
                    return Err(ProviderError::new(ProviderErrorCode::ResponseTooLarge));
                }
                while index < bytes.len()
                    && !matches!(bytes[index], b' ' | b'\t' | b'\r' | b'\n' | b',' | b'}' | b']')
                {
                    index += 1;
                }
            }
            _ => index += 1,
        }
    }
    if depth == 0 {
        Ok(values)
    } else {
        Err(ProviderError::new(ProviderErrorCode::InvalidResponse))
    }
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

fn effective_remaining(
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<Duration, ProviderError> {
    check_operation(context, deadline)?;
    let local = deadline.saturating_duration_since(Instant::now());
    Ok(context.remaining_time().map_or(local, |request| request.min(local)))
}

fn blocking_slice(
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<Duration, ProviderError> {
    check_operation(context, deadline)?;
    let local = deadline.saturating_duration_since(Instant::now());
    let request = context.remaining_time().unwrap_or(IO_POLL);
    let slice = IO_POLL.min(local).min(request);
    if slice.is_zero() { Err(ProviderError::new(ProviderErrorCode::Timeout)) } else { Ok(slice) }
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

fn map_stream_error(error: &io::Error) -> ProviderError {
    if error.kind() == io::ErrorKind::InvalidData {
        ProviderError::new(ProviderErrorCode::Tls)
    } else {
        ProviderError::new(ProviderErrorCode::Connect)
    }
}

#[allow(clippy::needless_pass_by_value)]
#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{CancellationToken, ExecutionOptions, ResourceLimits};
    use std::net::TcpListener;
    use std::sync::{Barrier, Condvar, Mutex};

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn memory_budget(context: &ExecutionContext) -> MemoryBudget {
        MemoryBudget::new(context).unwrap()
    }

    fn parse_raw_response(raw: &[u8]) -> Result<HttpResponse, ProviderError> {
        let context = context();
        let mut memory = memory_budget(&context);
        read_response(
            &mut Cursor::new(raw.to_vec()),
            &context,
            Instant::now() + Duration::from_secs(1),
            &mut memory,
        )
    }

    fn parse_generation_fixture(
        body: &[u8],
        endpoint: GenerationEndpoint,
    ) -> Result<GenerationResult, ProviderError> {
        let context = context();
        let mut memory = memory_budget(&context);
        let body = body.to_vec();
        memory.grow(body.capacity()).unwrap();
        parse_generation_response(
            HttpResponse { status: 200, retry_after: None, body },
            endpoint,
            &context,
            Instant::now() + Duration::from_secs(1),
            &mut memory,
        )
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
        assert_eq!(
            ProviderConfig::parse(
                "https://example.com/v1",
                "m",
                &format!("A{}", "B".repeat(MAX_ENVIRONMENT_NAME_BYTES)),
                Duration::from_secs(1),
                [],
            )
            .unwrap_err()
            .code(),
            ProviderErrorCode::InvalidConfiguration
        );
        assert_eq!(
            ProviderConfig::parse(
                &format!("https://example.com/{}", "x".repeat(MAX_BASE_URL_BYTES)),
                "m",
                "API_KEY",
                Duration::from_secs(1),
                [],
            )
            .unwrap_err()
            .code(),
            ProviderErrorCode::InvalidConfiguration
        );
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
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2001::1",
            "2001:20::1",
            "2001:2::1",
            "2002:0808:0808::1",
            "3fff::1",
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
        assert!(!valid_json_content_type("application/json;"));
        assert!(!valid_json_content_type("application/json; charset=utf-8; charset=utf-8"));
        assert!(valid_json_content_type("application/json; charset=utf-8"));
        let mut nested = "null".to_owned();
        for _ in 0..=MAX_JSON_DEPTH {
            nested = format!("[{nested}]");
        }
        let context = context();
        let mut memory = memory_budget(&context);
        assert_eq!(
            parse_json_bounded::<serde_json::Value>(
                nested.as_bytes(),
                &context,
                Instant::now() + Duration::from_secs(1),
                &mut memory,
            )
            .unwrap_err()
            .code(),
            ProviderErrorCode::ResponseTooLarge
        );
    }

    #[test]
    fn chat_and_responses_use_distinct_exact_wire_contracts() {
        let chat = GenerationRequest {
            endpoint: GenerationEndpoint::ChatCompletions,
            capability: "image-description",
            input: GenerationInput::Image {
                bytes: b"abc",
                media_type: "image/png",
                prompt: "describe",
            },
            max_output_tokens: 42,
            idempotency_key: None,
        };
        let (_, chat_body) = encode_generation_request(&chat, "model").unwrap();
        assert_eq!(
            std::str::from_utf8(&chat_body).unwrap(),
            r#"{"model":"model","messages":[{"role":"user","content":[{"type":"text","text":"describe"},{"type":"image_url","image_url":{"url":"data:image/png;base64,YWJj"}}]}],"max_tokens":42}"#
        );

        let responses = GenerationRequest { endpoint: GenerationEndpoint::Responses, ..chat };
        let (_, responses_body) = encode_generation_request(&responses, "model").unwrap();
        assert_eq!(
            std::str::from_utf8(&responses_body).unwrap(),
            r#"{"model":"model","input":[{"role":"user","content":[{"type":"input_text","text":"describe"},{"type":"input_image","image_url":"data:image/png;base64,YWJj"}]}],"max_output_tokens":42}"#
        );
    }

    #[test]
    fn chat_and_responses_parsers_enforce_real_independent_wire_contracts() {
        let chat = br#"{"id":"chatcmpl-test","object":"chat.completion","created":1720000000,"model":"configured","choices":[{"index":0,"message":{"role":"assistant","content":"trusted","refusal":null,"annotations":[]},"logprobs":null,"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2},"service_tier":"default","system_fingerprint":null}"#;
        assert_eq!(
            parse_generation_fixture(chat, GenerationEndpoint::ChatCompletions).unwrap().text,
            "trusted"
        );

        let responses = br#"{"id":"resp_test","object":"response","created_at":1720000000,"status":"completed","completed_at":1720000001,"error":null,"incomplete_details":null,"input":[],"model":"configured","output":[{"id":"rs_test","type":"reasoning","summary":[],"status":"completed"},{"id":"msg_test","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"trusted","annotations":[],"logprobs":[]}]}],"background":false,"instructions":null,"max_output_tokens":16,"metadata":{},"parallel_tool_calls":false,"previous_response_id":null,"reasoning":{},"reasoning_effort":null,"service_tier":"default","store":false,"temperature":1.0,"text":{"format":{"type":"text"}},"tool_choice":"auto","tools":[],"top_p":1.0,"truncation":"disabled","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2},"user":null}"#;
        assert_eq!(
            parse_generation_fixture(responses, GenerationEndpoint::Responses).unwrap().text,
            "trusted"
        );

        let invalid_cases: &[(&[u8], GenerationEndpoint, ProviderErrorCode)] = &[
            (
                br#"{"id":"chatcmpl-test","object":"chat.completion","created":1,"model":"configured","choices":[{"index":0,"message":{"role":"assistant","content":"must-not-win"},"finish_reason":"stop"}],"error":{"message":"failed","type":"server_error","code":"failed","param":null}}"#,
                GenerationEndpoint::ChatCompletions,
                ProviderErrorCode::InvalidResponse,
            ),
            (
                br#"{"id":"chatcmpl-test","object":"chat.completion","created":1,"model":"configured","choices":[{"index":0,"message":{"role":"assistant","content":"truncated"},"finish_reason":"length"}]}"#,
                GenerationEndpoint::ChatCompletions,
                ProviderErrorCode::Incomplete,
            ),
            (
                br#"{"id":"chatcmpl-test","object":"chat.completion","created":1,"model":"configured","choices":[{"index":1,"message":{"role":"assistant","content":"wrong index"},"finish_reason":"stop"}]}"#,
                GenerationEndpoint::ChatCompletions,
                ProviderErrorCode::InvalidResponse,
            ),
            (
                br#"{"id":"chatcmpl-test","object":"chat.completion","created":1,"model":"configured","choices":[{"index":0,"message":{"role":"user","content":"wrong role"},"finish_reason":"stop"}]}"#,
                GenerationEndpoint::ChatCompletions,
                ProviderErrorCode::InvalidResponse,
            ),
            (
                br#"{"id":"chatcmpl-test","object":"chat.completion","created":1,"model":"configured","choices":[{"index":0,"message":{"role":"assistant","content":"tool instead"},"finish_reason":"tool_calls"}]}"#,
                GenerationEndpoint::ChatCompletions,
                ProviderErrorCode::InvalidResponse,
            ),
            (
                br#"{"id":"resp_test","object":"response","created_at":1,"status":"incomplete","error":null,"incomplete_details":{"reason":"max_output_tokens"},"model":"configured","output":[]}"#,
                GenerationEndpoint::Responses,
                ProviderErrorCode::Incomplete,
            ),
            (
                br#"{"id":"resp_test","object":"response","created_at":1,"status":"incomplete","error":null,"incomplete_details":null,"model":"configured","output":[]}"#,
                GenerationEndpoint::Responses,
                ProviderErrorCode::InvalidResponse,
            ),
            (
                br#"{"id":"resp_test","object":"response","created_at":1,"status":"completed","error":{"message":"failed","type":"server_error","code":"failed","param":null},"incomplete_details":null,"model":"configured","output":[{"id":"msg_test","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"must-not-win","annotations":[]}]}]}"#,
                GenerationEndpoint::Responses,
                ProviderErrorCode::InvalidResponse,
            ),
            (
                br#"{"id":"resp_test","object":"response","created_at":1,"status":"completed","error":null,"incomplete_details":null,"model":"configured","output":[{"id":"msg_test","type":"message","status":"completed","role":"assistant","content":[{"type":"refusal","refusal":"denied"}]}]}"#,
                GenerationEndpoint::Responses,
                ProviderErrorCode::InvalidResponse,
            ),
            (
                br#"{"id":"resp_test","object":"response","created_at":1,"status":"completed","error":null,"incomplete_details":null,"model":"configured","output":[{"id":"msg_test","type":"message","status":"incomplete","role":"assistant","content":[{"type":"output_text","text":"partial","annotations":[]}]}]}"#,
                GenerationEndpoint::Responses,
                ProviderErrorCode::InvalidResponse,
            ),
            (
                br#"{"id":"resp_test","object":"response","created_at":1,"status":"completed","error":null,"incomplete_details":null,"model":"configured","output":[{"id":"unknown","type":"provider_magic","status":"completed"}]}"#,
                GenerationEndpoint::Responses,
                ProviderErrorCode::InvalidResponse,
            ),
            (
                br#"{"id":"resp_test","object":"response","created_at":1,"status":"completed","error":null,"incomplete_details":null,"model":"configured","output":[{"id":"call_test","type":"function_call","call_id":"call_1","name":"tool","arguments":"{}","status":"completed"}]}"#,
                GenerationEndpoint::Responses,
                ProviderErrorCode::InvalidResponse,
            ),
            (
                br#"{"id":"resp_test","object":"response","created_at":1,"status":"completed","error":null,"incomplete_details":null,"model":"configured","output":[{"id":"msg_test","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"x","annotations":[]}]}],"truncation":"provider_magic"}"#,
                GenerationEndpoint::Responses,
                ProviderErrorCode::InvalidResponse,
            ),
            (
                br#"{"id":"resp_test","object":"response","created_at":1,"status":"completed","error":null,"incomplete_details":null,"model":"configured","output":[],"output_text":"sdk convenience must not be wire"}"#,
                GenerationEndpoint::Responses,
                ProviderErrorCode::InvalidResponse,
            ),
        ];
        for (body, endpoint, expected) in invalid_cases {
            assert_eq!(
                parse_generation_fixture(body, *endpoint).unwrap_err().code(),
                *expected,
                "accepted invalid wire fixture: {}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn raw_http_parser_rejects_ambiguous_or_illegal_syntax() {
        let cases: &[&[u8]] = &[
            b"HTTP/1.1 20 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 2000 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\n: value\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\nBad Name: value\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\nX-Bad: \x01\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\nX-One: a\r\n folded\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n2;x=y\r\n{}\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\nX: y\r\n\r\n",
            b"HTTP/1.1 101 Switching Protocols\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 100 Continue\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 204 No Content\r\nContent-Type: text/plain\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.1 429 Slow Down\r\nRetry-After: tomorrow\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 429 Slow Down\r\nRetry-After: 3\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.1 401 Unauthorized\r\nContent-Type: text/html\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 429 Slow Down\r\nContent-Type: text/html\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
            b"HTTP/1.1 500 Server Error\r\nContent-Encoding: identity\r\n\r\n",
            b"HTTP/1.1 204 No Content\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
        ];
        for raw in cases {
            assert_eq!(
                parse_raw_response(raw).unwrap_err().code(),
                ProviderErrorCode::InvalidResponse,
                "accepted malformed fixture: {:?}",
                String::from_utf8_lossy(raw)
            );
        }
    }

    #[test]
    fn raw_http_parser_accepts_strict_chunked_json() {
        let response = parse_raw_response(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(response.body, b"{}");
        let no_body = parse_raw_response(b"HTTP/1.1 401 Unauthorized\r\n\r\n").unwrap();
        assert_eq!(no_body.status, 401);
        assert!(no_body.body.is_empty());
    }

    #[test]
    fn retry_backoff_has_bounded_injectable_jitter_and_exact_retry_after() {
        fn fixed_jitter(base: Duration, attempt: u8) -> Duration {
            base + Duration::from_millis(u64::from(attempt) * 7)
        }

        assert_eq!(retry_delay(None, 1, fixed_jitter), Duration::from_millis(207));
        assert_eq!(
            retry_delay(Some(Duration::from_millis(11)), 2, fixed_jitter),
            Duration::from_millis(11)
        );
        assert!(process_retry_jitter(Duration::from_millis(200), 1) <= Duration::from_millis(250));
    }

    #[test]
    fn provider_status_codes_have_stable_categories() {
        for (status, expected) in [
            (401, ProviderErrorCode::Unauthorized),
            (403, ProviderErrorCode::Forbidden),
            (404, ProviderErrorCode::NotFound),
            (408, ProviderErrorCode::Timeout),
            (409, ProviderErrorCode::Conflict),
            (429, ProviderErrorCode::RateLimited),
            (500, ProviderErrorCode::ServerError),
            (302, ProviderErrorCode::RedirectDenied),
        ] {
            assert_eq!(ensure_success_status(status).unwrap_err().code(), expected);
        }
    }

    struct StallingIo;

    impl Read for StallingIo {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    impl Write for StallingIo {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn slowloris_read_observes_request_cancellation() {
        let cancellation = CancellationToken::new();
        let context = ExecutionContext::new(
            ExecutionOptions {
                cancellation: cancellation.clone(),
                timeout: Some(Duration::from_secs(2)),
                progress_listener: None,
            },
            ResourceLimits::default(),
        );
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            cancellation.cancel();
        });
        let mut memory = memory_budget(&context);
        let error = read_response(
            &mut StallingIo,
            &context,
            Instant::now() + Duration::from_secs(2),
            &mut memory,
        )
        .unwrap_err();
        worker.join().unwrap();
        assert_eq!(error.code(), ProviderErrorCode::Cancelled);
    }

    #[test]
    fn gzip_bomb_and_excessive_chunk_framing_are_bounded() {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&vec![b'x'; MAX_DECOMPRESSED_BYTES + 1]).unwrap();
        let compressed = encoder.finish().unwrap();
        let context = context();
        let mut memory = memory_budget(&context);
        assert_eq!(
            decompress_gzip(
                &compressed,
                &context,
                Instant::now() + Duration::from_secs(2),
                &mut memory,
            )
            .unwrap_err()
            .code(),
            ProviderErrorCode::ResponseTooLarge
        );

        let mut raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n"
            .to_vec();
        for _ in 0..=MAX_CHUNKS {
            raw.extend_from_slice(b"1\r\nx\r\n");
        }
        raw.extend_from_slice(b"0\r\n\r\n");
        assert_eq!(
            parse_raw_response(&raw).unwrap_err().code(),
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
            let context = context();
            let mut memory = memory_budget(&context);
            let error = Secret::from_os_string(value, &mut memory).err().unwrap();
            assert_eq!(error.code(), ProviderErrorCode::SecretInvalid);
            assert_eq!(error.to_string(), "providerSecretInvalid");
        }
    }

    #[test]
    fn secret_candidate_buffer_is_zeroized_on_validation_and_memory_failures() {
        let canary = b"SECRET-CANDIDATE-CANARY";
        for mut candidate in [
            canary.iter().copied().chain(*b" ").collect::<Vec<_>>(),
            vec![b'x'; MAX_SECRET_BYTES + 1],
            vec![0xff, 0xfe],
        ] {
            let context = context();
            let mut memory = memory_budget(&context);
            assert!(validate_secret_candidate(&mut candidate, &mut memory).is_err());
            assert!(!candidate.windows(canary.len()).any(|window| window == canary));
            assert!(candidate.iter().all(|byte| *byte == 0));
        }

        let limited = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 0, ..ResourceLimits::default() },
        );
        let mut memory = memory_budget(&limited);
        let mut candidate = canary.to_vec();
        assert_eq!(
            validate_secret_candidate(&mut candidate, &mut memory).unwrap_err().code(),
            ProviderErrorCode::ResourceLimit
        );
        assert!(!candidate.windows(canary.len()).any(|window| window == canary));
        assert!(candidate.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn missing_secret_has_a_stable_non_echoing_error() {
        let context = context();
        let mut memory = memory_budget(&context);
        let error = Secret::from_environment(
            "INTO_MD_PROVIDER_TEST_DEFINITELY_MISSING_82C881",
            &mut memory,
        )
        .err()
        .expect("missing environment variable must fail");
        assert_eq!(error.code(), ProviderErrorCode::SecretMissing);
        assert_eq!(error.to_string(), "providerSecretMissing");
    }

    #[test]
    fn reflected_secret_detection_is_bounded_and_exact() {
        let context = context();
        let mut memory = memory_budget(&context);
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(
            contains_secret(
                b"prefix-PROVIDER_SECRET_CANARY-suffix",
                b"PROVIDER_SECRET_CANARY",
                &context,
                deadline,
                &mut memory,
            )
            .unwrap()
        );
        assert!(
            !contains_secret(
                b"prefix-PROVIDER_SECRET-suffix",
                b"PROVIDER_SECRET_CANARY",
                &context,
                deadline,
                &mut memory,
            )
            .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn secret_rejects_non_unicode_environment_bytes() {
        use std::os::unix::ffi::OsStringExt as _;
        let value = OsString::from_vec(vec![0xff, 0xfe]);
        let context = context();
        let mut memory = memory_budget(&context);
        assert_eq!(
            Secret::from_os_string(value, &mut memory).err().unwrap().code(),
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
    fn dns_results_are_bounded_deduplicated_and_port_exact() {
        assert_eq!(
            validate_dns_addresses(Vec::new(), 443).unwrap_err().code(),
            ProviderErrorCode::Dns
        );
        let mut overallocated = Vec::with_capacity(MAX_DNS_ADDRESSES + 1);
        overallocated.push("8.8.8.8:443".parse().unwrap());
        assert_eq!(
            validate_dns_addresses(overallocated, 443).unwrap_err().code(),
            ProviderErrorCode::Dns
        );
        assert_eq!(
            validate_dns_addresses(vec!["8.8.8.8:80".parse().unwrap()], 443).unwrap_err().code(),
            ProviderErrorCode::Dns
        );
        let result = validate_dns_addresses(
            vec!["8.8.8.8:443".parse().unwrap(), "8.8.8.8:443".parse().unwrap()],
            443,
        )
        .unwrap();
        assert_eq!(result, ["8.8.8.8:443".parse::<SocketAddr>().unwrap()]);
    }

    struct BlockingResolver {
        entered: Arc<Barrier>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Resolver for BlockingResolver {
        fn resolve(&self, _: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            self.entered.wait();
            let (lock, changed) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
            Ok(vec![SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port)])
        }
    }

    #[test]
    fn dns_pool_has_fixed_workers_and_bounded_queue_when_resolvers_hang() {
        let pool = DnsPool::start().unwrap();
        assert_eq!(pool.workers.len(), DNS_WORKERS);
        let entered = Arc::new(Barrier::new(DNS_WORKERS + 1));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let resolver: Arc<dyn Resolver> = Arc::new(BlockingResolver {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let job = || {
            let (result, _) = std::sync::mpsc::sync_channel(1);
            DnsJob {
                resolver: Arc::clone(&resolver),
                host: "blocked.test".into(),
                port: 443,
                result,
            }
        };
        for _ in 0..DNS_WORKERS {
            pool.submit(job()).unwrap();
        }
        entered.wait();
        for _ in 0..DNS_WORKERS * DNS_QUEUE_PER_WORKER {
            pool.submit(job()).unwrap();
        }
        assert_eq!(pool.submit(job()).unwrap_err().code(), ProviderErrorCode::Dns);
        let (lock, changed) = &*release;
        *lock.lock().unwrap() = true;
        changed.notify_all();
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
    fn models_pagination_is_explicitly_incomplete_and_does_not_claim_capabilities() {
        let config = ProviderConfig::parse(
            "https://provider.example/v1",
            "configured",
            "PATH",
            Duration::from_secs(1),
            ["image-description".into()],
        )
        .unwrap();
        let context = context();
        let mut memory = memory_budget(&context);
        let body = br#"{"object":"list","data":[{"id":"configured","object":"model","created":0,"owned_by":"test"}],"has_more":true}"#;
        assert_eq!(
            parse_models_response(
                HttpResponse { status: 200, retry_after: None, body: body.to_vec() },
                &config,
                &context,
                Instant::now() + Duration::from_secs(1),
                &mut memory,
            )
            .unwrap_err()
            .code(),
            ProviderErrorCode::Incomplete
        );
    }

    #[test]
    fn models_duplicate_id_is_an_invalid_response() {
        let config = ProviderConfig::parse(
            "https://provider.example/v1",
            "configured",
            "PATH",
            Duration::from_secs(1),
            [],
        )
        .unwrap();
        let context = context();
        let mut memory = memory_budget(&context);
        let body = br#"{"object":"list","data":[{"id":"configured","object":"model","created":0,"owned_by":"one"},{"id":"configured","object":"model","created":1,"owned_by":"two"}],"has_more":false}"#;
        assert_eq!(
            parse_models_response(
                HttpResponse { status: 200, retry_after: None, body: body.to_vec() },
                &config,
                &context,
                Instant::now() + Duration::from_secs(1),
                &mut memory,
            )
            .unwrap_err()
            .code(),
            ProviderErrorCode::InvalidResponse
        );
    }

    #[test]
    fn post_without_idempotency_key_is_not_replayed_after_write_progress() {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = first.local_addr().unwrap().port();
        let second = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
        let first_worker = std::thread::spawn(move || {
            let (mut stream, _) = first.accept().unwrap();
            let mut byte = [0_u8];
            stream.read_exact(&mut byte).unwrap();
        });
        let config = ProviderConfig::parse(
            &format!("http://provider.test:{port}/v1"),
            "configured",
            "PATH",
            Duration::from_secs(2),
            ["image-description".into()],
        )
        .unwrap();
        let mut client = OpenAiCompatibleClient::new(
            config,
            ProviderNetworkPolicy {
                allow_network: true,
                allow_private_network: true,
                allowed_hosts: vec!["provider.test".into()],
            },
        );
        client.resolver = Arc::new(FixedResolver(vec![
            SocketAddr::from(([127, 0, 0, 1], port)),
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
        ]));
        let request = GenerationRequest {
            endpoint: GenerationEndpoint::Responses,
            capability: "image-description",
            input: GenerationInput::Text("fixed test input"),
            max_output_tokens: 16,
            idempotency_key: None,
        };
        assert!(matches!(
            client.generate(request, &context()).unwrap_err().code(),
            ProviderErrorCode::Connect | ProviderErrorCode::InvalidResponse
        ));
        first_worker.join().unwrap();
        second.set_nonblocking(true).unwrap();
        assert!(matches!(second.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock));
    }

    #[test]
    fn idempotency_key_does_not_replay_completed_post_after_malformed_response() {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = first.local_addr().unwrap().port();
        let second = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
        let first_count = Arc::new(AtomicUsize::new(0));
        let first_worker_count = Arc::clone(&first_count);
        let first_worker = std::thread::spawn(move || {
            let (mut stream, _) = first.accept().unwrap();
            first_worker_count.fetch_add(1, Ordering::SeqCst);
            let mut request = Vec::new();
            let mut byte = [0_u8];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let header = std::str::from_utf8(&request).unwrap();
            let content_length = header
                .split("\r\n")
                .find_map(|line| line.strip_prefix("Content-Length: ")?.parse::<usize>().ok())
                .unwrap();
            let mut body = vec![0_u8; content_length];
            stream.read_exact(&mut body).unwrap();
            assert!(header.contains("Idempotency-Key: request-1\r\n"));
            assert!(!body.is_empty());
            stream.write_all(b"HTTP/1.1 malformed\r\n\r\n").unwrap();
        });
        let config = ProviderConfig::parse(
            &format!("http://provider.test:{port}/v1"),
            "configured",
            "PATH",
            Duration::from_secs(2),
            ["image-description".into()],
        )
        .unwrap();
        let mut client = OpenAiCompatibleClient::new(
            config,
            ProviderNetworkPolicy {
                allow_network: true,
                allow_private_network: true,
                allowed_hosts: vec!["provider.test".into()],
            },
        );
        client.resolver = Arc::new(FixedResolver(vec![
            SocketAddr::from(([127, 0, 0, 1], port)),
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
        ]));
        let request = GenerationRequest {
            endpoint: GenerationEndpoint::Responses,
            capability: "image-description",
            input: GenerationInput::Text("fixed test input"),
            max_output_tokens: 16,
            idempotency_key: Some("request-1"),
        };
        assert_eq!(
            client.generate(request, &context()).unwrap_err().code(),
            ProviderErrorCode::InvalidResponse
        );
        first_worker.join().unwrap();
        second.set_nonblocking(true).unwrap();
        let second_count = match second.accept() {
            Ok(_) => 1,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => 0,
            Err(error) => panic!("second listener failed: {error}"),
        };
        assert_eq!(first_count.load(Ordering::SeqCst), 1);
        assert_eq!(second_count, 0, "completed POST was replayed on the second IP");
    }

    #[test]
    fn get_is_not_replayed_on_another_ip_after_write_progress() {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = first.local_addr().unwrap().port();
        let second = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
        let first_worker = std::thread::spawn(move || {
            let (mut stream, _) = first.accept().unwrap();
            let mut byte = [0_u8];
            stream.read_exact(&mut byte).unwrap();
        });
        let config = ProviderConfig::parse(
            &format!("http://provider.test:{port}/v1"),
            "configured",
            "PATH",
            Duration::from_secs(2),
            [],
        )
        .unwrap();
        let mut client = OpenAiCompatibleClient::new(
            config,
            ProviderNetworkPolicy {
                allow_network: true,
                allow_private_network: true,
                allowed_hosts: vec!["provider.test".into()],
            },
        );
        client.resolver = Arc::new(FixedResolver(vec![
            SocketAddr::from(([127, 0, 0, 1], port)),
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
        ]));
        assert!(matches!(
            client.test(&context()).unwrap_err().code(),
            ProviderErrorCode::Connect | ProviderErrorCode::InvalidResponse
        ));
        first_worker.join().unwrap();
        second.set_nonblocking(true).unwrap();
        assert!(matches!(second.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock));
    }

    #[test]
    fn pre_request_tls_failure_is_not_treated_as_connect_fallback() {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = first.local_addr().unwrap().port();
        let second = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
        let first_worker = std::thread::spawn(move || {
            let (mut stream, _) = first.accept().unwrap();
            stream.write_all(b"not tls").unwrap();
        });
        let config = ProviderConfig::parse(
            &format!("https://provider.test:{port}/v1"),
            "configured",
            "PATH",
            Duration::from_secs(2),
            [],
        )
        .unwrap();
        let mut client = OpenAiCompatibleClient::new(
            config,
            ProviderNetworkPolicy {
                allow_network: true,
                allow_private_network: true,
                allowed_hosts: vec!["provider.test".into()],
            },
        );
        client.resolver = Arc::new(FixedResolver(vec![
            SocketAddr::from(([127, 0, 0, 1], port)),
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
        ]));
        assert_eq!(client.test(&context()).unwrap_err().code(), ProviderErrorCode::Tls);
        first_worker.join().unwrap();
        second.set_nonblocking(true).unwrap();
        assert!(matches!(second.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock));
    }

    #[test]
    fn malformed_tls_peer_is_classified_as_tls() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"not tls").unwrap();
        });
        let config = ProviderConfig::parse(
            &format!("https://provider.test:{}/v1", address.port()),
            "configured",
            "PATH",
            Duration::from_secs(2),
            [],
        )
        .unwrap();
        let mut client = OpenAiCompatibleClient::new(
            config,
            ProviderNetworkPolicy {
                allow_network: true,
                allow_private_network: true,
                allowed_hosts: vec!["provider.test".into()],
            },
        );
        client.resolver = Arc::new(FixedResolver(vec![address]));
        assert_eq!(client.test(&context()).unwrap_err().code(), ProviderErrorCode::Tls);
        worker.join().unwrap();
    }

    #[test]
    fn tls_handshake_stall_observes_total_context_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        let config = ProviderConfig::parse(
            &format!("https://provider.test:{}/v1", address.port()),
            "configured",
            "PATH",
            Duration::from_secs(2),
            [],
        )
        .unwrap();
        let mut client = OpenAiCompatibleClient::new(
            config,
            ProviderNetworkPolicy {
                allow_network: true,
                allow_private_network: true,
                allowed_hosts: vec!["provider.test".into()],
            },
        );
        client.resolver = Arc::new(FixedResolver(vec![address]));
        let context = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(Duration::from_millis(30)),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        assert_eq!(client.test(&context).unwrap_err().code(), ProviderErrorCode::Timeout);
        worker.join().unwrap();
    }

    #[test]
    fn idempotent_models_probe_retries_only_within_fixed_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            for attempt in 0..=MAX_RETRIES {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0_u8];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let (status, retry, body): (&str, &str, &[u8]) = if attempt < MAX_RETRIES {
                    ("500 Server Error", "Retry-After: 0\r\n", b"{}")
                } else {
                    (
                        "200 OK",
                        "",
                        br#"{"object":"list","data":[{"id":"configured","object":"model","created":0,"owned_by":"test"}],"has_more":false}"#,
                    )
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{retry}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });
        let config = ProviderConfig::parse(
            &format!("http://{address}/v1"),
            "configured",
            "PATH",
            Duration::from_secs(2),
            [],
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
        assert!(client.test(&context()).unwrap().configured_model_available);
        worker.join().unwrap();
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
            let body = br#"{"object":"list","data":[{"id":"configured","object":"model","created":0,"owned_by":"test"}],"has_more":false}"#;
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
        assert!(result.capabilities.is_empty());
        worker.join().unwrap();
    }
}

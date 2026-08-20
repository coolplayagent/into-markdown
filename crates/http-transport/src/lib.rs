//! Audited, policy-constrained HTTP transport shared by remote sources and providers.
//!
//! The transport never reads ambient environment such as proxy or certificate
//! variables; explicit CONNECT proxy routing is injected by callers as a
//! `ConnectionFactory`. It resolves through a bounded worker pool, connects to
//! an exact checked address, and retains the original host exclusively for
//! HTTP `Host` and TLS SNI.

#![forbid(unsafe_code)]

use flate2::read::GzDecoder;
use into_markdown_core::{ExecutionContext, ResourceReservation};
use rustls::pki_types::ServerName;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use unicode_normalization::UnicodeNormalization;
use url::{Host, Url};

const DNS_WORKERS: usize = 4;
const DNS_QUEUE_PER_WORKER: usize = 8;
const MAX_DNS_ADDRESSES: usize = 16;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HEADER_COUNT: usize = 128;
const IO_CHUNK_BYTES: usize = 8 * 1024;
const MAX_FILENAME_BYTES: usize = 255;
const MAX_URL_BYTES: usize = 8 * 1024;
const MAX_ALLOWED_HOSTS: usize = 128;
const MAX_HOST_BYTES: usize = 253;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_mins(2);
const IO_POLL_SLICE: Duration = Duration::from_millis(25);

/// Per-request deadline: the fixed base timeout plus one millisecond per
/// authorized KiB of wire budget, so bounded large downloads keep a finite,
/// size-proportional transfer allowance instead of the base timeout alone.
fn transfer_deadline(now: Instant, limits: FetchLimits) -> Option<Instant> {
    let kibibytes = limits.max_wire_bytes.checked_div(1024)?;
    let base_ms = u64::try_from(DEFAULT_REQUEST_TIMEOUT.as_millis()).ok()?;
    let total_ms = base_ms.checked_add(kibibytes)?;
    now.checked_add(Duration::from_millis(total_ms))
}

mod body;
mod connect;
mod dns;
mod error;
mod http1;
mod policy;
mod proxy;
mod tls;

use body::{ChunkChain, finalize_body, read_response};
use connect::{
    DirectConnectionFactory, blocking_slice, check_context, check_operation, read_checked,
    write_all_checked,
};
pub use dns::SystemDnsResolver;
use dns::resolve_checked;
use error::map_context_error;
pub use error::{TransportError, TransportErrorKind};
use http1::{parse_head, read_head};
pub use policy::is_public_ip;
use policy::{
    canonical_host, canonical_redirect, canonical_url, encode_get_request, find_bytes,
    invalid_header_value_byte, is_localhost_name, is_token, normalize_allowlist,
    parse_content_disposition, parse_content_type, redacted_url,
};
pub use proxy::{NoProxyList, ProxyConfig, ProxyConfigError, RoutedConnectionFactory};
use tls::tls_handshake;

/// Per-request network authorization. Empty `allowed_hosts` means any public host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPolicy {
    /// Master authorization; false guarantees zero DNS and connector calls.
    pub allow_network: bool,
    /// Separate authorization for all non-global addresses.
    pub allow_private_network: bool,
    /// Exact normalized DNS names or IP literals allowed for every redirect hop.
    pub allowed_hosts: Vec<String>,
    /// Maximum number of redirect responses followed.
    pub max_redirects: u8,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            allow_network: false,
            allow_private_network: false,
            allowed_hosts: Vec::new(),
            max_redirects: 3,
        }
    }
}

/// Transport and decoded-source limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchLimits {
    /// Maximum compressed or identity transfer-body bytes.
    pub max_wire_bytes: u64,
    /// Maximum final decoded source bytes.
    pub max_decoded_bytes: u64,
}

/// One redacted redirect provenance entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectHop {
    /// Source URL without user information, query, or fragment.
    pub from: String,
    /// Destination URL without user information, query, or fragment.
    pub to: String,
    /// Redirect status code.
    pub status: u16,
}

/// A complete bounded response whose immutable bytes remain charged to the request.
pub struct FetchedResource {
    bytes: Arc<[u8]>,
    reservation: ResourceReservation,
    /// Canonical final URL, redacted before leaving the transport boundary.
    pub final_url: String,
    /// Strict lower-case media type without parameters.
    pub media_type: Option<String>,
    /// Portable NFC filename from a validated Content-Disposition field.
    pub filename: Option<String>,
    /// Ordered, redacted redirect provenance.
    pub redirects: Vec<RedirectHop>,
}

/// Owned response parts returned by [`FetchedResource::into_parts`].
pub type FetchedParts =
    (Arc<[u8]>, ResourceReservation, String, Option<String>, Option<String>, Vec<RedirectHop>);

impl FetchedResource {
    /// Borrow the immutable decoded bytes.
    #[must_use]
    pub fn bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }

    /// Split the response into metadata, bytes, and the exact source-memory lease.
    #[must_use]
    pub fn into_parts(self) -> FetchedParts {
        (
            self.bytes,
            self.reservation,
            self.final_url,
            self.media_type,
            self.filename,
            self.redirects,
        )
    }
}

/// Injectable DNS boundary used by deterministic policy and rebinding tests.
pub trait DnsResolver: Send + Sync {
    /// Resolve at most a bounded set of addresses for one canonical host and exact port.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the injected resolver cannot produce an answer.
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

/// Read/write connection returned by an exact-address connector.
pub trait Connection: Read + Write + Send {}
impl<T: Read + Write + Send> Connection for T {}

/// Injectable exact-IP connector. Production code never supplies a hostname as a route target.
pub trait ConnectionFactory: Send + Sync {
    /// Connect to `address`; use `host` only for TLS SNI and certificate authentication.
    ///
    /// # Errors
    ///
    /// Returns a categorized transport error for connect, TLS, timeout, or cancellation failure.
    fn connect(
        &self,
        scheme: &str,
        host: &str,
        address: SocketAddr,
        context: &ExecutionContext,
        deadline: Instant,
    ) -> Result<Box<dyn Connection>, TransportError>;
}

/// Client for direct, proxy-free HTTP(S) requests over checked IP addresses.
pub struct HttpClient {
    resolver: Arc<dyn DnsResolver>,
    connector: Arc<dyn ConnectionFactory>,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("HttpClient").finish_non_exhaustive()
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self {
            resolver: Arc::new(SystemDnsResolver),
            connector: Arc::new(DirectConnectionFactory::default()),
        }
    }
}

impl HttpClient {
    /// Construct a direct client with an injected bounded DNS resolver.
    #[must_use]
    pub fn with_resolver(resolver: Arc<dyn DnsResolver>) -> Self {
        Self { resolver, connector: Arc::new(DirectConnectionFactory::default()) }
    }

    /// Construct a direct client with an injected DNS resolver and explicit
    /// TLS verification mode.
    #[must_use]
    pub fn with_insecure(resolver: Arc<dyn DnsResolver>, insecure: bool) -> Self {
        Self { resolver, connector: Arc::new(DirectConnectionFactory { insecure }) }
    }

    /// Construct an injected client without performing I/O.
    #[must_use]
    pub fn with_components(
        resolver: Arc<dyn DnsResolver>,
        connector: Arc<dyn ConnectionFactory>,
    ) -> Self {
        Self { resolver, connector }
    }

    /// Resolve and authorize one URL without sending request bytes.
    ///
    /// The returned hostname is canonical and the addresses retain the exact
    /// requested port. Callers must pass one of those addresses to
    /// [`Self::connect_address`] rather than resolving the hostname again.
    ///
    /// # Errors
    ///
    /// Returns a policy, DNS, timeout, cancellation, or resource error.
    pub fn authorized_addresses(
        &self,
        url: &Url,
        policy: &NetworkPolicy,
        context: &ExecutionContext,
        deadline: Instant,
    ) -> Result<(String, Vec<SocketAddr>), TransportError> {
        check_operation(context, deadline)?;
        if !policy.allow_network {
            return Err(TransportError::new(TransportErrorKind::NetworkDenied));
        }
        let allowed_hosts = normalize_allowlist(&policy.allowed_hosts)?;
        let host = canonical_host(url)?;
        if !allowed_hosts.is_empty() && !allowed_hosts.contains(&host) {
            return Err(TransportError::new(TransportErrorKind::HostDenied));
        }
        if is_localhost_name(&host) && !policy.allow_private_network {
            return Err(TransportError::new(TransportErrorKind::PrivateNetworkDenied));
        }
        let port = url
            .port_or_known_default()
            .filter(|port| *port != 0)
            .ok_or_else(|| TransportError::new(TransportErrorKind::InvalidMessage))?;
        let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
            vec![SocketAddr::new(ip, port)]
        } else {
            resolve_checked(Arc::clone(&self.resolver), host.clone(), port, context, deadline)?
        };
        let all_public = addresses.iter().all(|address| is_public_ip(address.ip()));
        let all_private = addresses.iter().all(|address| !is_public_ip(address.ip()));
        if !all_public && !policy.allow_private_network {
            return Err(TransportError::new(TransportErrorKind::PrivateNetworkDenied));
        }
        if url.scheme() == "http" && (!policy.allow_private_network || !all_private) {
            return Err(TransportError::new(TransportErrorKind::PrivateNetworkDenied));
        }
        Ok((host, addresses))
    }

    /// Connect one exact address returned by [`Self::authorized_addresses`].
    ///
    /// # Errors
    ///
    /// Returns a categorized connect, TLS, timeout, or cancellation error.
    pub fn connect_address(
        &self,
        url: &Url,
        host: &str,
        address: SocketAddr,
        context: &ExecutionContext,
        deadline: Instant,
    ) -> Result<Box<dyn Connection>, TransportError> {
        self.connector.connect(url.scheme(), host, address, context, deadline)
    }

    /// Fetch one HTTP(S) source with explicit authorization and bounded decoding.
    ///
    /// # Errors
    ///
    /// Returns a policy, transport, protocol, timeout, cancellation, or resource error.
    pub fn get(
        &self,
        source: &str,
        policy: &NetworkPolicy,
        limits: FetchLimits,
        context: &ExecutionContext,
    ) -> Result<FetchedResource, TransportError> {
        check_context(context)?;
        if !policy.allow_network {
            return Err(TransportError::new(TransportErrorKind::NetworkDenied));
        }
        let mut current = canonical_url(source)?;
        let now = Instant::now();
        let local_deadline = transfer_deadline(now, limits)
            .ok_or_else(|| TransportError::new(TransportErrorKind::Timeout))?;
        let deadline = context
            .remaining_time()
            .and_then(|remaining| now.checked_add(remaining))
            .map_or(local_deadline, |request_deadline| request_deadline.min(local_deadline));
        let mut visited = BTreeSet::new();
        let mut redirects = Vec::new();
        loop {
            check_operation(context, deadline)?;
            let public_current = redacted_url(&current);
            // Retain the full canonical value only in request-local memory so distinct
            // signed URLs are not conflated. It is never returned or formatted.
            if !visited.insert(current.as_str().to_owned()) {
                return Err(TransportError::new(TransportErrorKind::Http));
            }
            let response = self.request_once(&current, policy, limits, context, deadline)?;
            if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                let location = response
                    .location
                    .as_deref()
                    .ok_or_else(|| TransportError::new(TransportErrorKind::Http))?;
                if redirects.len() >= usize::from(policy.max_redirects) {
                    return Err(TransportError::new(TransportErrorKind::Http));
                }
                let next = canonical_redirect(&current, location)?;
                let public_next = redacted_url(&next);
                if visited.contains(next.as_str()) {
                    return Err(TransportError::new(TransportErrorKind::Http));
                }
                redirects.push(RedirectHop {
                    from: public_current,
                    to: public_next,
                    status: response.status,
                });
                current = next;
                continue;
            }
            if response.status != 200 {
                return Err(TransportError::new(TransportErrorKind::Http));
            }
            let (bytes, reservation) =
                finalize_body(response.body, response.content_encoding, limits, context, deadline)?;
            return Ok(FetchedResource {
                bytes,
                reservation,
                final_url: redacted_url(&current),
                media_type: response.media_type,
                filename: response.filename,
                redirects,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn request_once(
        &self,
        url: &Url,
        policy: &NetworkPolicy,
        limits: FetchLimits,
        context: &ExecutionContext,
        deadline: Instant,
    ) -> Result<RawResponse, TransportError> {
        let route_capacity = MAX_DNS_ADDRESSES
            .checked_mul(std::mem::size_of::<SocketAddr>())
            .ok_or_else(|| TransportError::new(TransportErrorKind::ResourceLimit))?;
        let mut route_memory = context
            .reserve_memory(
                u64::try_from(route_capacity)
                    .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?,
            )
            .map_err(map_context_error)?;
        let (host, addresses) = self.authorized_addresses(url, policy, context, deadline)?;
        let address_capacity = addresses
            .capacity()
            .checked_mul(std::mem::size_of::<SocketAddr>())
            .ok_or_else(|| TransportError::new(TransportErrorKind::ResourceLimit))?;
        route_memory
            .shrink(
                u64::try_from(route_capacity - address_capacity)
                    .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?,
            )
            .map_err(map_context_error)?;
        let mut last = TransportError::new(TransportErrorKind::Connect);
        for (index, address) in addresses.iter().copied().enumerate() {
            check_operation(context, deadline)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            let left = u32::try_from(addresses.len() - index).unwrap_or(u32::MAX).max(1);
            let attempt_deadline =
                Instant::now().checked_add(remaining / left).unwrap_or(deadline).min(deadline);
            match self.connect_address(url, &host, address, context, attempt_deadline) {
                Ok(mut stream) => {
                    let (request, _request_memory) = encode_get_request(url, &host, context)?;
                    write_all_checked(&mut stream, request.as_bytes(), context, deadline)?;
                    return read_response(&mut stream, limits, context, deadline);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        TransportErrorKind::Connect | TransportErrorKind::Tls
                    ) && index + 1 < addresses.len() =>
                {
                    last = error;
                }
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }
}

struct RawResponse {
    status: u16,
    location: Option<String>,
    media_type: Option<String>,
    filename: Option<String>,
    content_encoding: ContentEncoding,
    body: WireBody,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContentEncoding {
    Identity,
    Gzip,
}

enum Framing {
    Length(usize),
    Chunked,
    Close,
}

struct ParsedHead {
    status: u16,
    framing: Framing,
    location: Option<String>,
    media_type: Option<String>,
    filename: Option<String>,
    content_encoding: ContentEncoding,
}

struct WireBody {
    chunks: ChunkChain,
    memory: ResourceReservation,
}

#[cfg(test)]
mod tests {
    mod protocol;
    mod proxy;
    mod resource;
    mod ssrf;
}

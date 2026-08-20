//! Explicit, caller-injected CONNECT proxy routing for HTTPS origins.
//!
//! The library never reads ambient environment. A caller parses and
//! authorizes a proxy description, then injects it as a
//! [`ConnectionFactory`](super::ConnectionFactory). TLS still terminates
//! end-to-end at the origin: the proxy only observes the `host:port` of a
//! destination that the policy layer has already independently authorized
//! and resolved. Plaintext HTTP origins never enter a tunnel; they keep the
//! direct, public-only route.

use super::{
    Connection, ConnectionFactory, DirectConnectionFactory, DnsResolver, ExecutionContext,
    IO_CHUNK_BYTES, Instant, Ipv6Addr, MAX_ALLOWED_HOSTS, MAX_HEADER_BYTES, MAX_HOST_BYTES,
    MAX_URL_BYTES, SocketAddr, TransportError, TransportErrorKind, canonical_host, check_operation,
    map_context_error, parse_head, read_head, resolve_checked, tls_handshake, write_all_checked,
};
use std::fmt::Write as _;
use std::sync::Arc;
use url::Url;

const MAX_CREDENTIAL_BYTES: usize = 256;

/// Stable failure while parsing one explicit proxy endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProxyConfigError {
    /// The value is empty.
    Empty,
    /// The value exceeds the bounded proxy URL length.
    TooLong,
    /// The value is not a valid absolute URL.
    InvalidUrl,
    /// Only `http://` proxy endpoints are supported.
    UnsupportedScheme,
    /// The proxy host is missing or invalid.
    InvalidHost,
    /// The proxy credentials contain characters outside the portable subset.
    InvalidCredentials,
    /// The proxy credentials exceed the bounded length.
    CredentialsTooLong,
}

impl std::fmt::Display for ProxyConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "proxy URL is empty",
            Self::TooLong => "proxy URL exceeds the bounded length",
            Self::InvalidUrl => "proxy URL is not a valid absolute http endpoint",
            Self::UnsupportedScheme => "only http:// proxy endpoints are supported",
            Self::InvalidHost => "proxy host is missing or invalid",
            Self::InvalidCredentials => {
                "proxy credentials contain characters outside the portable subset"
            }
            Self::CredentialsTooLong => "proxy credentials exceed the bounded length",
        })
    }
}

impl std::error::Error for ProxyConfigError {}

/// Parsed `http://[user:pass@]host[:port]` CONNECT proxy endpoint.
///
/// Credentials are retained only as a pre-encoded `Proxy-Authorization`
/// value and are never exposed through accessors or diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    host: String,
    port: u16,
    authorization: Option<String>,
}

impl ProxyConfig {
    /// Parse one bounded explicit proxy endpoint.
    ///
    /// The URL must be absolute, use the `http` scheme, carry no path,
    /// query, or fragment, and restrict credentials to the portable
    /// printable subset that needs no percent-decoding.
    ///
    /// # Errors
    ///
    /// Returns a stable [`ProxyConfigError`]; no value is partially retained.
    pub fn parse(value: &str) -> Result<Self, ProxyConfigError> {
        if value.is_empty() {
            return Err(ProxyConfigError::Empty);
        }
        if value.len() > MAX_URL_BYTES {
            return Err(ProxyConfigError::TooLong);
        }
        let url = Url::parse(value).map_err(|_| ProxyConfigError::InvalidUrl)?;
        if url.scheme() != "http" {
            return Err(ProxyConfigError::UnsupportedScheme);
        }
        if url.path() != "/" && !url.path().is_empty() {
            return Err(ProxyConfigError::InvalidUrl);
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ProxyConfigError::InvalidUrl);
        }
        let host = canonical_host(&url).map_err(|_| ProxyConfigError::InvalidHost)?;
        if host.is_empty() || host.len() > MAX_HOST_BYTES {
            return Err(ProxyConfigError::InvalidHost);
        }
        let port = url.port().unwrap_or(80);
        let authorization = Self::credentials(&url)?;
        Ok(Self { host, port, authorization })
    }

    fn credentials(url: &Url) -> Result<Option<String>, ProxyConfigError> {
        let username = url.username();
        let password = url.password().unwrap_or_default();
        if username.is_empty() && password.is_empty() {
            return Ok(None);
        }
        if username.is_empty() {
            return Err(ProxyConfigError::InvalidCredentials);
        }
        for value in [username, password] {
            if value.bytes().any(|byte| !portable_credential_byte(byte)) {
                return Err(ProxyConfigError::InvalidCredentials);
            }
        }
        let mut raw = String::with_capacity(username.len() + password.len() + 1);
        raw.push_str(username);
        raw.push(':');
        raw.push_str(password);
        if raw.len() > MAX_CREDENTIAL_BYTES {
            return Err(ProxyConfigError::CredentialsTooLong);
        }
        let mut encoded = String::with_capacity(raw.len().div_ceil(3) * 4 + 6);
        write!(encoded, "Basic ").map_err(|_| ProxyConfigError::CredentialsTooLong)?;
        base64_standard(raw.as_bytes(), &mut encoded);
        Ok(Some(encoded))
    }

    /// Canonical proxy host.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Proxy port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Redacted `host:port` form for diagnostics; credentials never appear.
    #[must_use]
    pub fn redacted_endpoint(&self) -> String {
        if self.host.parse::<Ipv6Addr>().is_ok() {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn portable_credential_byte(byte: u8) -> bool {
    (0x21..=0x7E).contains(&byte) && !matches!(byte, b'%' | b'@' | b':' | b'/' | b'?' | b'#')
}

pub(super) fn base64_standard(input: &[u8], output: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in input.chunks(3) {
        let bytes = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let triple = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        let mut index = triple >> 18 & 63;
        output.push(char::from(ALPHABET[usize::try_from(index).unwrap_or(0)]));
        index = triple >> 12 & 63;
        output.push(char::from(ALPHABET[usize::try_from(index).unwrap_or(0)]));
        if chunk.len() > 1 {
            index = triple >> 6 & 63;
            output.push(char::from(ALPHABET[usize::try_from(index).unwrap_or(0)]));
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            index = triple & 63;
            output.push(char::from(ALPHABET[usize::try_from(index).unwrap_or(0)]));
        } else {
            output.push('=');
        }
    }
}

/// Host patterns that bypass an injected proxy route.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NoProxyList {
    all: bool,
    entries: Vec<String>,
}

impl NoProxyList {
    /// Parse one comma-separated exclusion list; empty input matches nothing.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        let mut all = false;
        let mut entries = Vec::new();
        for raw in value.split(',') {
            let entry = raw.trim().to_ascii_lowercase();
            if entry.is_empty() {
                continue;
            }
            if entry == "*" {
                all = true;
                continue;
            }
            let entry = entry.strip_prefix('.').unwrap_or(&entry).to_owned();
            if !entry.is_empty()
                && entry.len() <= MAX_HOST_BYTES
                && entries.len() < MAX_ALLOWED_HOSTS
            {
                entries.push(entry);
            }
        }
        Self { all, entries }
    }

    /// Match one canonical lowercase origin host.
    #[must_use]
    pub fn matches(&self, host: &str) -> bool {
        if self.all {
            return true;
        }
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.entries.iter().any(|entry| {
            host == *entry || (host.len() > entry.len() + 1 && host.ends_with(&format!(".{entry}")))
        })
    }
}

/// Connection factory that routes policy-authorized HTTPS origins through a
/// CONNECT proxy and uses direct connections for every other target.
pub struct RoutedConnectionFactory {
    proxy: ProxyConfig,
    no_proxy: NoProxyList,
    resolver: Arc<dyn DnsResolver>,
    inner: Arc<dyn ConnectionFactory>,
    insecure: bool,
}

impl RoutedConnectionFactory {
    /// Construct with the direct factory as the proxy transport.
    #[must_use]
    pub fn new(
        proxy: ProxyConfig,
        no_proxy: NoProxyList,
        resolver: Arc<dyn DnsResolver>,
        insecure: bool,
    ) -> Self {
        Self {
            proxy,
            no_proxy,
            resolver,
            inner: Arc::new(DirectConnectionFactory { insecure }),
            insecure,
        }
    }

    /// Construct with an injected proxy transport for deterministic tests.
    #[must_use]
    pub fn with_inner(
        proxy: ProxyConfig,
        no_proxy: NoProxyList,
        resolver: Arc<dyn DnsResolver>,
        inner: Arc<dyn ConnectionFactory>,
        insecure: bool,
    ) -> Self {
        Self { proxy, no_proxy, resolver, inner, insecure }
    }

    pub(super) fn tunnel_stream(
        &self,
        host: &str,
        port: u16,
        context: &ExecutionContext,
        deadline: Instant,
    ) -> Result<Box<dyn Connection>, TransportError> {
        let addresses = resolve_checked(
            Arc::clone(&self.resolver),
            self.proxy.host.clone(),
            self.proxy.port(),
            context,
            deadline,
        )?;
        let mut last = TransportError::new(TransportErrorKind::Connect);
        for (index, address) in addresses.iter().copied().enumerate() {
            check_operation(context, deadline)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            let left = u32::try_from(addresses.len() - index).unwrap_or(u32::MAX).max(1);
            let attempt_deadline =
                Instant::now().checked_add(remaining / left).unwrap_or(deadline).min(deadline);
            match self.one_proxy_address(address, host, port, context, attempt_deadline, deadline) {
                Ok(stream) => return Ok(stream),
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

    fn one_proxy_address(
        &self,
        address: SocketAddr,
        host: &str,
        port: u16,
        context: &ExecutionContext,
        attempt_deadline: Instant,
        deadline: Instant,
    ) -> Result<Box<dyn Connection>, TransportError> {
        let mut stream =
            self.inner.connect("http", self.proxy.host(), address, context, attempt_deadline)?;
        establish_tunnel(
            stream.as_mut(),
            self.proxy.authorization.as_deref(),
            host,
            port,
            context,
            deadline,
        )?;
        Ok(stream)
    }
}

impl ConnectionFactory for RoutedConnectionFactory {
    fn connect(
        &self,
        scheme: &str,
        host: &str,
        address: SocketAddr,
        context: &ExecutionContext,
        deadline: Instant,
    ) -> Result<Box<dyn Connection>, TransportError> {
        if scheme != "https" || self.no_proxy.matches(host) {
            return self.inner.connect(scheme, host, address, context, deadline);
        }
        let stream = self.tunnel_stream(host, address.port(), context, deadline)?;
        tls_handshake(stream, host, context, deadline, self.insecure)
            .map(|stream| Box::new(stream) as _)
    }
}

pub(super) fn establish_tunnel(
    stream: &mut dyn Connection,
    authorization: Option<&str>,
    host: &str,
    port: u16,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<(), TransportError> {
    let ipv6 = host.parse::<Ipv6Addr>().is_ok();
    let target = if ipv6 { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    // "CONNECT <target> HTTP/1.1\r\nHost: <target>\r\n" plus the optional
    // "Proxy-Authorization: <value>\r\n" line and the final blank line, all
    // with exact byte counts.
    let authorization_bytes = authorization.map_or(0, |value| value.len().saturating_add(23));
    let required =
        29usize.saturating_add(target.len().saturating_mul(2)).saturating_add(authorization_bytes);
    if required > MAX_URL_BYTES + MAX_HOST_BYTES + MAX_CREDENTIAL_BYTES.div_ceil(3) * 4 + 64 {
        return Err(TransportError::new(TransportErrorKind::ResourceLimit));
    }
    let mut memory = context
        .reserve_memory(
            u64::try_from(required)
                .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?,
        )
        .map_err(map_context_error)?;
    let mut request = String::with_capacity(required);
    write!(request, "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n")
        .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
    if let Some(value) = authorization {
        write!(request, "Proxy-Authorization: {value}\r\n")
            .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
    }
    request.push_str("\r\n");
    if request.capacity() > required {
        memory
            .grow(
                u64::try_from(request.capacity() - required)
                    .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?,
            )
            .map_err(map_context_error)?;
    }
    if request.len() != required {
        return Err(TransportError::new(TransportErrorKind::ResourceLimit));
    }
    write_all_checked(stream, request.as_bytes(), context, deadline)?;
    let head_budget = u64::try_from(MAX_HEADER_BYTES + IO_CHUNK_BYTES)
        .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
    let mut head_memory = context.reserve_memory(head_budget).map_err(map_context_error)?;
    let (bytes, end) = read_head(stream, context, deadline, &mut head_memory)?;
    // A compliant proxy sends nothing after the response head until the
    // client speaks; early tunnel bytes indicate a smuggling attempt.
    if bytes.len() > end {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    let parsed = parse_head(&bytes[..end])?;
    if !(200..=299).contains(&parsed.status) {
        return Err(TransportError::new(TransportErrorKind::Connect));
    }
    Ok(())
}

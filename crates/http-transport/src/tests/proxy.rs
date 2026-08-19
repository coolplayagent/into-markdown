use crate::proxy::{base64_standard, establish_tunnel};
use crate::*;
use std::io;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

fn parse_ok(value: &str) -> ProxyConfig {
    ProxyConfig::parse(value).unwrap()
}

#[test]
fn proxy_urls_accept_only_bounded_http_endpoints() {
    assert_eq!(parse_ok("http://proxy.test").port(), 80);
    assert_eq!(parse_ok("http://proxy.test:8080").redacted_endpoint(), "proxy.test:8080");
    assert_eq!(parse_ok("HTTP://Proxy.Test:8080/").redacted_endpoint(), "proxy.test:8080");
    assert_eq!(parse_ok("http://[2001:db8::1]:8080").redacted_endpoint(), "[2001:db8::1]:8080");
    for value in [
        "",
        "socks5://proxy.test:1080",
        "https://proxy.test:443",
        "proxy.test:8080",
        "http://proxy.test/path",
        "http://proxy.test?query=1",
        "http://proxy.test#fragment",
        "http://user%40x:pass@proxy.test",
        "http://:pass@proxy.test",
        &format!("http://{}", "a".repeat(9_000)),
    ] {
        assert!(ProxyConfig::parse(value).is_err(), "{value}");
    }
}

#[test]
fn no_proxy_patterns_cover_wildcard_exact_and_domain_suffixes() {
    let list = NoProxyList::parse("*, HF.co, .mirror.test,, BAD ENTRY ");
    assert!(list.matches("anything.test"));
    let list = NoProxyList::parse("huggingface.co, .mirror.test");
    assert!(list.matches("huggingface.co"));
    assert!(list.matches("HUGGINGFACE.co."));
    assert!(list.matches("cdn.mirror.test"));
    assert!(!list.matches("notmirror.test"));
    assert!(!list.matches("mirror.test.evil.test"));
    assert!(!NoProxyList::default().matches("huggingface.co"));
}

#[test]
fn base64_encoding_matches_rfc4648_vectors() {
    for (input, expected) in
        [(&b""[..], ""), (b"M", "TQ=="), (b"Ma", "TWE="), (b"Man", "TWFu"), (b"Many", "TWFueQ==")]
    {
        let mut output = String::new();
        base64_standard(input, &mut output);
        assert_eq!(output, expected);
    }
}

struct RecordingConnection {
    response: Vec<u8>,
    read_offset: usize,
    request: Arc<Mutex<Vec<u8>>>,
}

impl Read for RecordingConnection {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let remaining = &self.response[self.read_offset..];
        let count = remaining.len().min(output.len());
        output[..count].copy_from_slice(&remaining[..count]);
        self.read_offset += count;
        Ok(count)
    }
}

impl Write for RecordingConnection {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.request.lock().unwrap().extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct RecordingConnector {
    response: Vec<u8>,
    request: Arc<Mutex<Vec<u8>>>,
    seen: Mutex<Vec<(String, String, SocketAddr)>>,
}

impl ConnectionFactory for RecordingConnector {
    fn connect(
        &self,
        scheme: &str,
        host: &str,
        address: SocketAddr,
        _: &ExecutionContext,
        _: Instant,
    ) -> Result<Box<dyn Connection>, TransportError> {
        self.seen.lock().unwrap().push((scheme.to_owned(), host.to_owned(), address));
        Ok(Box::new(RecordingConnection {
            response: self.response.clone(),
            read_offset: 0,
            request: Arc::clone(&self.request),
        }))
    }
}

fn context() -> ExecutionContext {
    ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits::default(),
    )
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

struct ProxyDns {
    addresses: Vec<SocketAddr>,
    hosts: Mutex<Vec<(String, u16)>>,
}

impl DnsResolver for ProxyDns {
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        self.hosts.lock().unwrap().push((host.to_owned(), port));
        Ok(self.addresses.iter().map(|address| SocketAddr::new(address.ip(), port)).collect())
    }
}

fn routed_factory(proxy: ProxyConfig, no_proxy: NoProxyList, response: &[u8]) -> TunnelRig {
    let request = Arc::new(Mutex::new(Vec::new()));
    let connector = Arc::new(RecordingConnector {
        response: response.to_vec(),
        request: Arc::clone(&request),
        seen: Mutex::new(Vec::new()),
    });
    let dns = Arc::new(ProxyDns {
        addresses: vec!["9.9.9.9:1080".parse().unwrap()],
        hosts: Mutex::new(Vec::new()),
    });
    let factory = RoutedConnectionFactory::with_inner(
        proxy,
        no_proxy,
        Arc::clone(&dns) as Arc<dyn DnsResolver>,
        Arc::clone(&connector) as Arc<dyn ConnectionFactory>,
    );
    TunnelRig { factory, request, connector, dns }
}

struct TunnelRig {
    factory: RoutedConnectionFactory,
    request: Arc<Mutex<Vec<u8>>>,
    connector: Arc<RecordingConnector>,
    dns: Arc<ProxyDns>,
}

#[test]
fn tunnel_request_is_exact_and_requires_a_success_response() {
    let rig = routed_factory(
        parse_ok("http://proxy.test:8080"),
        NoProxyList::default(),
        b"HTTP/1.1 200 Connection established\r\n\r\n",
    );
    let stream = rig
        .factory
        .tunnel_stream("huggingface.co", 443, &context(), deadline())
        .unwrap();
    drop(stream);
    assert_eq!(
        rig.request.lock().unwrap().as_slice(),
        b"CONNECT huggingface.co:443 HTTP/1.1\r\nHost: huggingface.co:443\r\n\r\n".as_slice()
    );
    let seen = rig.connector.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!((seen[0].0.as_str(), seen[0].1.as_str()), ("http", "proxy.test"));
    assert_eq!(seen[0].2, "9.9.9.9:8080".parse::<SocketAddr>().unwrap());
    assert_eq!(*rig.dns.hosts.lock().unwrap(), [("proxy.test".to_owned(), 8080)]);
}

#[test]
fn tunnel_credentials_are_sent_only_in_the_authorization_header() {
    let rig = routed_factory(
        parse_ok("http://user:secret@proxy.test:8080"),
        NoProxyList::default(),
        b"HTTP/1.1 200 OK\r\n\r\n",
    );
    rig.factory.tunnel_stream("origin.test", 8443, &context(), deadline()).unwrap();
    let request = rig.request.lock().unwrap();
    let text = String::from_utf8(request.clone()).unwrap();
    assert!(text.contains("CONNECT origin.test:8443 HTTP/1.1\r\n"));
    assert!(text.contains("Proxy-Authorization: Basic dXNlcjpzZWNyZXQ=\r\n"));
    assert!(!text.contains("secret"));
}

#[test]
fn tunnel_failures_are_stable_for_refusals_smuggling_and_bad_grammar() {
    for (response, kind) in [
        (&b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n"[..], TransportErrorKind::Connect),
        (b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n", TransportErrorKind::Connect),
        (b"HTTP/1.1 200 OK\r\n\r\nEARLY-TUNNEL-BYTES", TransportErrorKind::InvalidMessage),
        (b"garbage\r\n\r\n", TransportErrorKind::InvalidMessage),
        (b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n", TransportErrorKind::InvalidMessage),
    ] {
        let rig = routed_factory(
            parse_ok("http://proxy.test:8080"),
            NoProxyList::default(),
            response,
        );
        let Err(error) = rig.factory.tunnel_stream("origin.test", 443, &context(), deadline())
        else {
            panic!("{response:?} must fail");
        };
        assert_eq!(error.kind(), kind, "{response:?}");
    }
}

#[test]
fn routed_factory_bypasses_the_tunnel_for_excluded_and_plaintext_targets() {
    let rig = routed_factory(
        parse_ok("http://proxy.test:8080"),
        NoProxyList::parse("origin.test"),
        b"HTTP/1.1 200 OK\r\n\r\n",
    );
    let origin: SocketAddr = "8.8.8.8:443".parse().unwrap();
    drop(rig.factory.connect("https", "origin.test", origin, &context(), deadline()).unwrap());
    drop(rig.factory.connect("http", "plain.test", origin, &context(), deadline()).unwrap());
    let seen = rig.connector.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!((seen[0].0.as_str(), seen[0].2), ("https", origin));
    assert_eq!((seen[1].0.as_str(), seen[1].1.as_str()), ("http", "plain.test"));
    assert!(rig.dns.hosts.lock().unwrap().is_empty(), "no proxy resolution may happen on bypass");
}

#[test]
fn tunnel_surfaces_cancellation_and_deadline_checks() {
    let mut connection = RecordingConnection {
        response: Vec::new(),
        read_offset: 0,
        request: Arc::new(Mutex::new(Vec::new())),
    };
    let error = establish_tunnel(&mut connection, None, "origin.test", 443, &context(), Instant::now())
        .unwrap_err();
    assert_eq!(error.kind(), TransportErrorKind::Timeout);
}

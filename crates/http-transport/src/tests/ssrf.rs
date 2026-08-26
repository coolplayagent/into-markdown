use crate::*;
use std::io;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingDns {
    calls: AtomicUsize,
    addresses: Vec<SocketAddr>,
}

struct HostDns;

impl DnsResolver for HostDns {
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let ip = if host == "private.test" { "127.0.0.1" } else { "8.8.8.8" };
        Ok(vec![format!("{ip}:{port}").parse().unwrap()])
    }
}

struct SlowDns;

impl DnsResolver for SlowDns {
    fn resolve(&self, _: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        std::thread::sleep(Duration::from_millis(50));
        Ok(vec![format!("8.8.8.8:{port}").parse().unwrap()])
    }
}

struct CancellingDns(into_markdown_core::CancellationToken);

impl DnsResolver for CancellingDns {
    fn resolve(&self, _: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        self.0.cancel();
        std::thread::sleep(Duration::from_millis(50));
        Ok(vec![format!("8.8.8.8:{port}").parse().unwrap()])
    }
}

struct ScriptedConnection {
    response: std::io::Cursor<Vec<u8>>,
    request: Vec<u8>,
}

impl Read for ScriptedConnection {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.response.read(output)
    }
}

impl Write for ScriptedConnection {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.request.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ScriptedConnector {
    responses: Mutex<std::collections::VecDeque<Vec<u8>>>,
    hosts: Mutex<Vec<String>>,
}

impl ScriptedConnector {
    fn new(responses: impl IntoIterator<Item = &'static [u8]>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(<[u8]>::to_vec).collect()),
            hosts: Mutex::new(Vec::new()),
        }
    }
}

impl ConnectionFactory for ScriptedConnector {
    fn connect(
        &self,
        _: &str,
        host: &str,
        _: SocketAddr,
        _: &ExecutionContext,
        _: Instant,
    ) -> Result<Box<dyn Connection>, TransportError> {
        self.hosts.lock().unwrap().push(host.into());
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| TransportError::new(TransportErrorKind::Connect))?;
        Ok(Box::new(ScriptedConnection {
            response: std::io::Cursor::new(response),
            request: Vec::new(),
        }))
    }
}

impl DnsResolver for CountingDns {
    fn resolve(&self, _: &str, _: u16) -> io::Result<Vec<SocketAddr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.addresses.clone())
    }
}

fn context() -> ExecutionContext {
    ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits::default(),
    )
}

#[test]
fn offline_and_host_denial_make_zero_dns_calls() {
    let dns = Arc::new(CountingDns { calls: AtomicUsize::new(0), addresses: vec![] });
    let client = HttpClient::with_resolver(dns.clone());
    let url = Url::parse("https://secret.example/file?signed=private").unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    assert_eq!(
        client
            .authorized_addresses(&url, &NetworkPolicy::default(), &context(), deadline)
            .unwrap_err()
            .kind(),
        TransportErrorKind::NetworkDenied
    );
    let denied = NetworkPolicy {
        allow_network: true,
        allowed_hosts: vec!["other.example".into()],
        ..NetworkPolicy::default()
    };
    assert_eq!(
        client.authorized_addresses(&url, &denied, &context(), deadline).unwrap_err().kind(),
        TransportErrorKind::HostDenied
    );
    assert_eq!(dns.calls.load(Ordering::SeqCst), 0);
    assert_eq!(redacted_url(&url), "https://secret.example/file");
}

#[test]
fn mixed_dns_answer_and_plaintext_public_target_fail_closed() {
    let dns = Arc::new(CountingDns {
        calls: AtomicUsize::new(0),
        addresses: vec!["8.8.8.8:443".parse().unwrap(), "127.0.0.1:443".parse().unwrap()],
    });
    let client = HttpClient::with_resolver(dns);
    let policy = NetworkPolicy { allow_network: true, ..NetworkPolicy::default() };
    let error = client
        .authorized_addresses(
            &Url::parse("https://example.test/").unwrap(),
            &policy,
            &context(),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
    assert_eq!(error.kind(), TransportErrorKind::PrivateNetworkDenied);

    let private = NetworkPolicy {
        allow_network: true,
        allow_private_network: true,
        ..NetworkPolicy::default()
    };
    let error = HttpClient::default()
        .authorized_addresses(
            &Url::parse("http://8.8.8.8/").unwrap(),
            &private,
            &context(),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
    assert_eq!(error.kind(), TransportErrorKind::PrivateNetworkDenied);
}

#[test]
fn address_policy_covers_special_and_mapped_ranges() {
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
        "2001::1",
        "2002:0808:0808::1",
        "3fff::1",
    ] {
        assert!(!is_public_ip(address.parse().unwrap()), "{address}");
    }
    assert!(is_public_ip("8.8.8.8".parse().unwrap()));
    assert!(!is_public_ip("::ffff:8.8.8.8".parse().unwrap()));
}

#[test]
fn dns_result_shape_timeout_and_cancellation_are_bounded() {
    let mut too_many = Vec::with_capacity(MAX_DNS_ADDRESSES + 1);
    for last in 1..=MAX_DNS_ADDRESSES + 1 {
        too_many.push(format!("8.8.8.{last}:443").parse().unwrap());
    }
    let dns = Arc::new(CountingDns { calls: AtomicUsize::new(0), addresses: too_many });
    let client = HttpClient::with_resolver(dns);
    assert_eq!(
        client
            .authorized_addresses(
                &Url::parse("https://shape.test/").unwrap(),
                &public_policy(&["shape.test"], 0),
                &context(),
                Instant::now() + Duration::from_secs(1),
            )
            .err()
            .unwrap()
            .kind(),
        TransportErrorKind::Dns
    );

    let timeout_context = ExecutionContext::new(
        into_markdown_core::ExecutionOptions {
            timeout: Some(Duration::from_millis(10)),
            ..Default::default()
        },
        into_markdown_core::ResourceLimits::default(),
    );
    let client = HttpClient::with_resolver(Arc::new(SlowDns));
    assert_eq!(
        client
            .authorized_addresses(
                &Url::parse("https://slow.test/").unwrap(),
                &public_policy(&["slow.test"], 0),
                &timeout_context,
                Instant::now() + Duration::from_secs(1),
            )
            .err()
            .unwrap()
            .kind(),
        TransportErrorKind::Timeout
    );

    let cancellation = into_markdown_core::CancellationToken::new();
    let resolver_cancellation = cancellation.clone();
    let cancelled_context = ExecutionContext::new(
        into_markdown_core::ExecutionOptions { cancellation, ..Default::default() },
        into_markdown_core::ResourceLimits::default(),
    );
    let client = HttpClient::with_resolver(Arc::new(CancellingDns(resolver_cancellation)));
    assert_eq!(
        client
            .authorized_addresses(
                &Url::parse("https://slow.test/").unwrap(),
                &public_policy(&["slow.test"], 0),
                &cancelled_context,
                Instant::now() + Duration::from_secs(1),
            )
            .err()
            .unwrap()
            .kind(),
        TransportErrorKind::Cancelled
    );
}

fn scripted_client(
    responses: impl IntoIterator<Item = &'static [u8]>,
) -> (HttpClient, Arc<ScriptedConnector>) {
    let connector = Arc::new(ScriptedConnector::new(responses));
    (HttpClient::with_components(Arc::new(HostDns), connector.clone()), connector)
}

fn public_policy(hosts: &[&str], redirects: u8) -> NetworkPolicy {
    NetworkPolicy {
        allow_network: true,
        allow_private_network: false,
        allowed_hosts: hosts.iter().map(|host| (*host).into()).collect(),
        max_redirects: redirects,
    }
}

#[test]
fn redirects_reauthorize_host_and_private_address_on_every_hop() {
    let redirect =
        b"HTTP/1.1 302 Found\r\nLocation: https://other.test/final\r\nContent-Length: 0\r\n\r\n"
            .as_slice();
    let (client, _) = scripted_client([redirect]);
    let error = client
        .get(
            "https://public.test/start",
            &public_policy(&["public.test"], 3),
            FetchLimits { max_wire_bytes: 64, max_decoded_bytes: 64 },
            &context(),
        )
        .err()
        .unwrap();
    assert_eq!(error.kind(), TransportErrorKind::HostDenied);

    let private =
        b"HTTP/1.1 302 Found\r\nLocation: https://private.test/final\r\nContent-Length: 0\r\n\r\n"
            .as_slice();
    let (client, _) = scripted_client([private]);
    let error = client
        .get(
            "https://public.test/start",
            &public_policy(&["public.test", "private.test"], 3),
            FetchLimits { max_wire_bytes: 64, max_decoded_bytes: 64 },
            &context(),
        )
        .err()
        .unwrap();
    assert_eq!(error.kind(), TransportErrorKind::PrivateNetworkDenied);
}

#[test]
fn redirect_cycle_limit_and_redacted_metadata_are_deterministic() {
    let first =
        b"HTTP/1.1 302 Found\r\nLocation: /final.txt?token=second\r\nContent-Length: 0\r\n\r\n"
            .as_slice();
    let final_response = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=final.txt\r\nContent-Length: 4\r\n\r\ndata".as_slice();
    let (client, connector) = scripted_client([first, final_response]);
    let resource = client
        .get(
            "https://public.test/start?token=first",
            &public_policy(&["public.test"], 2),
            FetchLimits { max_wire_bytes: 64, max_decoded_bytes: 64 },
            &context(),
        )
        .unwrap();
    assert_eq!(resource.final_url, "https://public.test/final.txt");
    assert_eq!(resource.filename.as_deref(), Some("final.txt"));
    assert_eq!(
        resource.redirects,
        [RedirectHop {
            from: "https://public.test/start".into(),
            to: "https://public.test/final.txt".into(),
            status: 302,
        }]
    );
    assert_eq!(*connector.hosts.lock().unwrap(), ["public.test", "public.test"]);

    let cycle_a =
        b"HTTP/1.1 302 Found\r\nLocation: https://b.test/x\r\nContent-Length: 0\r\n\r\n".as_slice();
    let cycle_b =
        b"HTTP/1.1 302 Found\r\nLocation: https://a.test/x\r\nContent-Length: 0\r\n\r\n".as_slice();
    let (client, _) = scripted_client([cycle_a, cycle_b]);
    assert_eq!(
        client
            .get(
                "https://a.test/x",
                &public_policy(&["a.test", "b.test"], 3),
                FetchLimits { max_wire_bytes: 64, max_decoded_bytes: 64 },
                &context(),
            )
            .err()
            .unwrap()
            .kind(),
        TransportErrorKind::Http
    );

    let (client, _) = scripted_client([first]);
    assert_eq!(
        client
            .get(
                "https://public.test/start",
                &public_policy(&["public.test"], 0),
                FetchLimits { max_wire_bytes: 64, max_decoded_bytes: 64 },
                &context(),
            )
            .err()
            .unwrap()
            .kind(),
        TransportErrorKind::Http
    );
}

#[test]
fn streamed_identity_response_is_bounded_and_reserves_before_dns() {
    let response =
        b"HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: 4\r\n\r\ndata"
            .as_slice();
    let (client, _) = scripted_client([response]);
    let execution = context();
    let mut output = Vec::new();
    let streamed = client
        .get_to_writer(
            "https://public.test/plugin.zip",
            &public_policy(&["public.test"], 0),
            FetchLimits { max_wire_bytes: 4, max_decoded_bytes: 4 },
            &execution,
            &mut output,
        )
        .unwrap();
    assert_eq!(streamed.bytes_written, 4);
    assert_eq!(output, b"data");
    assert_eq!(execution.reserved_temporary_bytes(), 4);
    drop(streamed);
    assert_eq!(execution.reserved_temporary_bytes(), 0);

    let dns = Arc::new(CountingDns {
        calls: AtomicUsize::new(0),
        addresses: vec!["8.8.8.8:443".parse().unwrap()],
    });
    let client = HttpClient::with_resolver(dns.clone());
    let limited = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits { max_temporary_bytes: 3, ..Default::default() },
    );
    assert_eq!(
        client
            .get_to_writer(
                "https://public.test/plugin.zip",
                &public_policy(&["public.test"], 0),
                FetchLimits { max_wire_bytes: 4, max_decoded_bytes: 4 },
                &limited,
                &mut Vec::new(),
            )
            .err()
            .expect("temporary budget")
            .kind(),
        TransportErrorKind::ResourceLimit
    );
    assert_eq!(dns.calls.load(Ordering::SeqCst), 0);
    assert_eq!(limited.reserved_temporary_bytes(), 0);
}

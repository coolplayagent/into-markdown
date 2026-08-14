//! Product-engine tests for the real policy-bound image-description adapter.

use super::*;
use std::future::Future;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

#[test]
fn real_image_description_adapter_obeys_all_product_modes_and_fixed_wire_contract() {
    let server = ControlledServer::start();
    let address = server.address;

    let network = ProviderNetworkPolicy {
        allow_network: true,
        allow_private_network: true,
        allowed_hosts: vec!["127.0.0.1".into()],
    };
    let config = ProviderConfig::parse(
        &format!("http://{address}/v1"),
        "controlled-vision",
        "PATH",
        Duration::from_secs(2),
        ["image-description".into()],
    )
    .unwrap();
    let provider = Arc::new(OpenAiImageDescriptionProvider::new(
        OpenAiCompatibleClient::new(config, network),
        ProviderNetworkPolicy {
            allow_network: true,
            allow_private_network: true,
            allowed_hosts: vec!["127.0.0.1".into()],
        },
    ));
    let engine =
        default_engine_with_services(Services { ai: Some(provider), ..Services::default() })
            .unwrap();
    let bytes = std::fs::read(fixture_path("small/ocr/ocr-english-clear-1.png")).unwrap();

    let off_result = block_on(engine.convert(image_request(&bytes, AiMode::Off))).unwrap();
    assert_eq!(server.call_count(), 0);
    assert!(!off_result.markdown.contains("A controlled product image"));

    for mode in [AiMode::Fallback, AiMode::Prefer, AiMode::Only] {
        let result = block_on(engine.convert(image_request(&bytes, mode))).unwrap();
        assert!(
            result.markdown.contains("A controlled product image"),
            "mode {mode:?} markdown={:?} diagnostics={:?}",
            result.markdown,
            result.diagnostics
        );
        assert!(result.provenance.iter().any(|item| {
            item.kind == ProvenanceKind::AiProvider
                && item.provider == "openai-compatible.image-description"
                && item.locator.page == Some(1)
        }));
        assert_eq!(result.document.blocks[0].id.0, "image-page-1");
        assert!(!result.markdown.contains("pdf-page-"));
    }
    assert_eq!(server.call_count(), 3);

    let absent_engine = default_engine().unwrap();
    let error = block_on(absent_engine.convert(image_request(&bytes, AiMode::Only))).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ComponentUnavailable);
    server.shutdown();
}

struct ControlledServer {
    address: SocketAddr,
    calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl ControlledServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let server_calls = Arc::clone(&calls);
        let server_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            serve(&listener, server_calls.as_ref(), server_stop.as_ref());
        });
        Self { address, calls, stop, handle }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn shutdown(self) {
        self.stop.store(true, Ordering::Release);
        self.handle.join().unwrap();
    }
}

fn serve(listener: &TcpListener, calls: &AtomicUsize, stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let request = read_http_request(&mut stream);
                let headers_end = request.windows(4).position(|part| part == b"\r\n\r\n").unwrap();
                assert_fixed_request(&request, headers_end);
                let response = response_body();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .unwrap();
                stream.write_all(response).unwrap();
                calls.fetch_add(1, Ordering::SeqCst);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => std::thread::yield_now(),
            Err(error) => panic!("controlled image-description server failed: {error}"),
        }
    }
}

fn assert_fixed_request(request: &[u8], headers_end: usize) {
    assert!(request.starts_with(b"POST /v1/responses HTTP/1.1\r\n"));
    let body: serde_json::Value = serde_json::from_slice(&request[headers_end + 4..]).unwrap();
    assert_eq!(body["model"], "controlled-vision");
    assert_eq!(body["max_output_tokens"], 512);
    assert_eq!(
        body["input"][0]["content"][0]["text"],
        "Describe the visible content of this image accurately and concisely. Do not infer hidden \
         text or metadata."
    );
    assert!(
        body["input"][0]["content"][1]["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    assert_eq!(body["input"][0]["content"].as_array().unwrap().len(), 2);
}

fn image_request(bytes: &[u8], mode: AiMode) -> ConversionRequest {
    let mut request =
        ConversionRequest::new(InputRef::bytes(bytes.to_vec(), Some("image-description.png")));
    request.options.ai.image_description = mode;
    request.options.network.enabled = true;
    request.options.network.deny_private_networks = false;
    request.options.network.allowed_hosts = vec!["127.0.0.1".into()];
    request.execution.timeout = Some(Duration::from_secs(3));
    request
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
    }
    let header = std::str::from_utf8(&request).unwrap();
    let content_length = header
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length: ").or_else(|| line.strip_prefix("content-length: "))
        })
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let start = request.len();
    request.resize(start + content_length, 0);
    stream.read_exact(&mut request[start..]).unwrap();
    request
}

fn response_body() -> &'static [u8] {
    br#"{"id":"resp_image","object":"response","created_at":1720000000,"status":"completed","completed_at":1720000001,"error":null,"incomplete_details":null,"input":[],"model":"controlled-vision","output":[{"id":"msg_image","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"A controlled product image.","annotations":[],"logprobs":[]}]}],"background":false,"instructions":null,"max_output_tokens":512,"metadata":{},"parallel_tool_calls":false,"previous_response_id":null,"reasoning":{},"reasoning_effort":null,"service_tier":"default","store":false,"temperature":1.0,"text":{"format":{"type":"text"}},"tool_choice":"auto","tools":[],"top_p":1.0,"truncation":"disabled","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2},"user":null}"#
}

fn fixture_path(relative: &str) -> PathBuf {
    std::env::var_os("TEST_SRCDIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(relative),
        |runfiles| {
            PathBuf::from(runfiles)
                .join(std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "into_markdown".into()))
                .join("fixtures")
                .join(relative)
        },
    )
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(output) => return output,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

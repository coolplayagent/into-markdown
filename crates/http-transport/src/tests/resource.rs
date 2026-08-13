use crate::*;
use std::io::{self, Read, Write};

#[derive(Default)]
struct MemoryConnection {
    read: std::io::Cursor<Vec<u8>>,
    written: Vec<u8>,
}

impl Read for MemoryConnection {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.read.read(output)
    }
}

impl Write for MemoryConnection {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.written.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn context(memory: u64) -> ExecutionContext {
    ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits { max_memory_bytes: memory, ..Default::default() },
    )
}

#[test]
fn content_length_rejects_limit_plus_one_before_body_read() {
    let mut stream = MemoryConnection {
        read: std::io::Cursor::new(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n12345".to_vec()),
        written: vec![],
    };
    let error = read_response(
        &mut stream,
        FetchLimits { max_wire_bytes: 4, max_decoded_bytes: 4 },
        &context(1_000_000),
        Instant::now() + Duration::from_secs(1),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), TransportErrorKind::ResourceLimit);
}

#[test]
fn exact_identity_boundary_decodes_and_keeps_source_lease() {
    let context = context(1_000_000);
    let mut stream = MemoryConnection {
        read: std::io::Cursor::new(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndata".to_vec()),
        written: vec![],
    };
    let response = read_response(
        &mut stream,
        FetchLimits { max_wire_bytes: 4, max_decoded_bytes: 4 },
        &context,
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    let (bytes, lease) = finalize_body(
        response.body,
        response.content_encoding,
        FetchLimits { max_wire_bytes: 4, max_decoded_bytes: 4 },
        &context,
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(&*bytes, b"data");
    drop(lease);
}

fn decode_raw(raw: Vec<u8>, limits: FetchLimits) -> Result<Arc<[u8]>, TransportError> {
    let context = context(1_000_000);
    let mut stream = MemoryConnection { read: std::io::Cursor::new(raw), written: vec![] };
    let response =
        read_response(&mut stream, limits, &context, Instant::now() + Duration::from_secs(1))?;
    finalize_body(
        response.body,
        response.content_encoding,
        limits,
        &context,
        Instant::now() + Duration::from_secs(1),
    )
    .map(|(bytes, _)| bytes)
}

#[test]
fn chunked_and_gzip_decoded_limits_stop_bombs() {
    let chunked =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n12345\r\n0\r\n\r\n".to_vec();
    assert_eq!(
        decode_raw(chunked, FetchLimits { max_wire_bytes: 4, max_decoded_bytes: 4 })
            .unwrap_err()
            .kind(),
        TransportErrorKind::ResourceLimit
    );

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(b"12345").unwrap();
    let compressed = encoder.finish().unwrap();
    let mut raw = format!(
        "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
        compressed.len()
    )
    .into_bytes();
    raw.extend_from_slice(&compressed);
    assert_eq!(
        decode_raw(
            raw,
            FetchLimits {
                max_wire_bytes: u64::try_from(compressed.len()).unwrap(),
                max_decoded_bytes: 4,
            }
        )
        .err()
        .unwrap()
        .kind(),
        TransportErrorKind::ResourceLimit
    );
}

#[test]
fn chunked_wire_limit_counts_every_framing_octet_at_exact_boundary() {
    const BODY: &[u8] = b"000000000000000000000000000001\r\nX\r\n0\r\n\r\n";
    let mut raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    raw.extend_from_slice(BODY);
    let exact = u64::try_from(BODY.len()).unwrap();
    assert_eq!(
        &*decode_raw(raw.clone(), FetchLimits { max_wire_bytes: exact, max_decoded_bytes: 1 })
            .unwrap(),
        b"X"
    );
    assert_eq!(
        decode_raw(raw, FetchLimits { max_wire_bytes: exact - 1, max_decoded_bytes: 1 })
            .unwrap_err()
            .kind(),
        TransportErrorKind::ResourceLimit
    );
}

#[test]
fn empty_identity_body_obeys_exact_zero_boundary() {
    let bytes = decode_raw(
        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
        FetchLimits { max_wire_bytes: 0, max_decoded_bytes: 0 },
    )
    .unwrap();
    assert!(bytes.is_empty());
}

struct StalledConnection;

impl Read for StalledConnection {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        std::thread::sleep(Duration::from_millis(2));
        Err(io::Error::new(io::ErrorKind::TimedOut, "injected stall"))
    }
}

impl Write for StalledConnection {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn stalled_headers_obey_deadline_and_cancellation() {
    let mut stream = StalledConnection;
    assert_eq!(
        read_response(
            &mut stream,
            FetchLimits { max_wire_bytes: 1, max_decoded_bytes: 1 },
            &context(1_000_000),
            Instant::now() + Duration::from_millis(5),
        )
        .err()
        .unwrap()
        .kind(),
        TransportErrorKind::Timeout
    );

    let cancellation = into_markdown_core::CancellationToken::new();
    let cancel_later = cancellation.clone();
    let cancelled = ExecutionContext::new(
        into_markdown_core::ExecutionOptions { cancellation, ..Default::default() },
        into_markdown_core::ResourceLimits::default(),
    );
    let cancellation_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        cancel_later.cancel();
    });
    assert_eq!(
        read_response(
            &mut StalledConnection,
            FetchLimits { max_wire_bytes: 1, max_decoded_bytes: 1 },
            &cancelled,
            Instant::now() + Duration::from_secs(1),
        )
        .err()
        .unwrap()
        .kind(),
        TransportErrorKind::Cancelled
    );
    cancellation_thread.join().unwrap();
}

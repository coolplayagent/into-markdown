use crate::policy::portable_filename;
use crate::*;

fn parse(raw: &[u8]) -> Result<ParsedHead, TransportError> {
    let end = find_bytes(raw, b"\r\n\r\n")
        .map(|index| index + 4)
        .ok_or_else(|| TransportError::new(TransportErrorKind::InvalidMessage))?;
    parse_head(&raw[..end])
}

#[test]
fn strict_http_head_accepts_exact_content_metadata() {
    let head = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Disposition: attachment; filename*=UTF-8''report%2Emd\r\n\r\n").unwrap();
    assert_eq!(head.status, 200);
    assert!(matches!(head.framing, Framing::Length(4)));
    assert_eq!(head.media_type.as_deref(), Some("text/plain"));
    assert_eq!(head.filename.as_deref(), Some("report.md"));
}

#[test]
fn ambiguous_and_active_http_syntax_is_rejected() {
    for raw in [
        b"HTTP/1.1 2000 OK\r\nContent-Length: 0\r\n\r\n".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nBad Name: value\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nX-Test: \x01\r\n\r\n",
        b"HTTP/1.1 200 OK\r\n folded\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Encoding: br\r\n\r\n",
    ] {
        assert_eq!(parse(raw).err().unwrap().kind(), TransportErrorKind::InvalidMessage);
    }
}

#[test]
fn portable_filename_rejects_paths_devices_and_non_nfc() {
    for value in ["../a", "C:\\a", "con.txt", "a/b", "e\u{301}.txt", ".hidden"] {
        assert!(portable_filename(value).is_err(), "{value}");
    }
    assert_eq!(portable_filename("报告.md").unwrap(), "报告.md");
}

#[test]
fn non_blocking_connect_probe_classifies_pending_and_complete_states() {
    use crate::connect::{connect_pending, reconnect_complete, reconnect_pending};
    use std::io;

    let pending = [io::Error::from_raw_os_error(10035), io::Error::from_raw_os_error(36)];
    for error in &pending {
        assert!(connect_pending(error));
        assert!(reconnect_pending(error));
        assert!(!reconnect_complete(error));
    }
    let still_pending = [
        io::Error::from_raw_os_error(10037),
        io::Error::from_raw_os_error(115),
        io::Error::from_raw_os_error(114),
        io::Error::from_raw_os_error(37),
    ];
    for error in &still_pending {
        assert!(reconnect_pending(error));
        assert!(!reconnect_complete(error));
    }
    let complete = [
        io::Error::from_raw_os_error(10056),
        io::Error::from_raw_os_error(106),
        io::Error::from_raw_os_error(56),
    ];
    for error in &complete {
        assert!(reconnect_complete(error));
        assert!(!reconnect_pending(error));
    }
    let failed = [io::Error::from_raw_os_error(10061), io::Error::from_raw_os_error(111)];
    for error in &failed {
        assert!(!connect_pending(error));
        assert!(!reconnect_pending(error));
        assert!(!reconnect_complete(error));
    }
}

// SPDX-License-Identifier: Apache-2.0

use std::io::{Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::SystemTime;

const VALID_DOCUMENT: &str = r#"{"schemaVersion":1,"metadata":{"title":null,"authors":[],"properties":{}},"blocks":[]}"#;
const INVALID_IR: &str = r#"{"schemaVersion":2,"metadata":{"title":null,"authors":[],"properties":{}},"blocks":[]}"#;
const PROVENANCE_DOCUMENT: &str = r#"{"schemaVersion":1,"metadata":{"title":null,"authors":[],"properties":{}},"blocks":[{"id":"p1","block":{"type":"paragraph","data":[]},"provenance":{"kind":"nativeParser","provider":"fixture","locator":{"page":null,"slide":null,"sheet":null,"cell":null,"bounds":null,"time":null,"part":null},"confidence":1.0}}]}"#;
const BAD_PROVENANCE: &str = r#"{"schemaVersion":1,"metadata":{"title":null,"authors":[],"properties":{}},"blocks":[{"id":"p1","block":{"type":"paragraph","data":[]},"provenance":{"kind":"nativeParser","provider":"other-plugin","locator":{"page":null,"slide":null,"sheet":null,"cell":null,"bounds":null,"time":null,"part":null},"confidence":1.0}}]}"#;

fn envelope(document: &str, resources: &str) -> String {
    format!(
        r#"{{"protocolVersion":1,"documentJson":{:?},"resources":{resources}}}"#,
        document
    )
}

fn resource(path: &str, media_type: &str) -> String {
    format!(
        r#"[{{"path":{path:?},"mediaType":{media_type:?},"bytes":[97,98,99],"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}}]"#
    )
}

fn main() {
    let mut request = String::new();
    std::io::stdin().read_to_string(&mut request).unwrap();
    if request.contains("fuel-loop") {
        let mut value = 1_u64;
        loop {
            value = std::hint::black_box(value.wrapping_mul(1_664_525).wrapping_add(1_013_904_223));
        }
    }
    if request.contains("memory-growth") {
        let mut bytes = vec![0_u8; 64 * 1024 * 1024];
        for offset in (0..bytes.len()).step_by(64 * 1024) {
            bytes[offset] = 43;
        }
        std::hint::black_box(bytes);
    }
    if request.contains("memory-oob") {
        // This fixture deliberately emits a real out-of-bounds Wasm load.
        let value = unsafe { std::ptr::read_volatile(usize::MAX as *const u8) };
        std::hint::black_box(value);
    }
    if request.contains("oversized-output") {
        let bytes = vec![b'x'; 2 * 1024 * 1024];
        std::io::stdout().write_all(&bytes).unwrap();
        return;
    }
    if request.contains("clock-call") {
        std::hint::black_box(SystemTime::now());
    }
    if request.contains("random-call") {
        let mut values = std::collections::HashMap::new();
        values.insert("entropy", 43_u8);
        std::hint::black_box(values);
    }
    if request.contains("preopen-call") {
        let value = std::fs::read_to_string("/input/probe.txt").unwrap();
        assert_eq!(value, "preopen-ok");
    }
    if request.contains("symlink-escape") {
        std::hint::black_box(std::fs::read_to_string("/input/escape/secret.txt").unwrap());
    }
    if request.contains("network-call:") {
        let marker = "network-call:";
        let start = request.find(marker).unwrap() + marker.len();
        let port_end = request[start..]
            .find(|character: char| !character.is_ascii_digit())
            .map(|offset| start + offset)
            .unwrap_or(request.len());
        let port: u16 = request[start..port_end].parse().unwrap();
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        std::hint::black_box(TcpStream::connect(address).unwrap());
    }
    let response = if request.contains("invalid-ir") {
        envelope(INVALID_IR, "[]")
    } else if request.contains("valid-resource") {
        envelope(PROVENANCE_DOCUMENT, &resource("assets/probe.txt", "text/plain"))
    } else if request.contains("bad-resource") {
        envelope(VALID_DOCUMENT, &resource("../escape", "text/plain"))
    } else if request.contains("alias-resource") {
        envelope(
            VALID_DOCUMENT,
            r#"[{"path":"Asset/X.txt","mediaType":"text/plain","bytes":[97,98,99],"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"},{"path":"asset/x.TXT","mediaType":"text/plain","bytes":[97,98,99],"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}]"#,
        )
    } else if request.contains("bad-mime") {
        envelope(VALID_DOCUMENT, &resource("asset/x.txt", "text/plain; charset=utf-8"))
    } else if request.contains("bad-provenance") {
        envelope(BAD_PROVENANCE, "[]")
    } else {
        envelope(VALID_DOCUMENT, "[]")
    };
    std::io::stdout().write_all(response.as_bytes()).unwrap();
}

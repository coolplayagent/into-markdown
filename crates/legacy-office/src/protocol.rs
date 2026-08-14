//! Fixed, length-prefixed single-request worker protocol.

use crate::NormalizedFormat;
use into_markdown_core::InputFormat;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const MAGIC: [u8; 4] = *b"IMLO";
const VERSION: u16 = 1;
const HEADER_BYTES: usize = 24;
const REQUEST_META_BYTES: usize = 56;
const RESPONSE_META_BYTES: usize = 48;
pub(crate) const REQUEST: u16 = 1;
pub(crate) const RESPONSE: u16 = 2;
pub(crate) const ERROR: u16 = 255;
pub(crate) const REQUEST_ID: u64 = 1;
pub(crate) const ERROR_MALFORMED: u8 = 1;
pub(crate) const ERROR_ENCRYPTED: u8 = 2;
pub(crate) const ERROR_RESOURCE: u8 = 3;
pub(crate) const ERROR_RUNTIME: u8 = 4;
pub(crate) const ERROR_SANDBOX: u8 = 5;

pub(crate) struct RequestMeta {
    pub source: InputFormat,
    pub input_bytes: u64,
    pub maximum_output_bytes: u64,
    pub input_sha256: [u8; 32],
}

pub(crate) struct Response {
    pub format: NormalizedFormat,
    pub bytes: Vec<u8>,
}

pub(crate) enum WorkerReply {
    Output(Response),
    Error(u8),
}

pub(crate) fn write_request(
    writer: &mut impl Write,
    source: InputFormat,
    input: &[u8],
    maximum_output_bytes: u64,
) -> Result<(), ()> {
    let input_bytes = u64::try_from(input.len()).map_err(|_| ())?;
    let payload_bytes =
        u64::try_from(REQUEST_META_BYTES).map_err(|_| ())?.checked_add(input_bytes).ok_or(())?;
    write_header(writer, REQUEST, REQUEST_ID, payload_bytes)?;
    let mut metadata = [0_u8; REQUEST_META_BYTES];
    metadata[0] = encode_source(source)?;
    metadata[8..16].copy_from_slice(&input_bytes.to_le_bytes());
    metadata[16..24].copy_from_slice(&maximum_output_bytes.to_le_bytes());
    metadata[24..56].copy_from_slice(&Sha256::digest(input));
    writer.write_all(&metadata).map_err(|_| ())?;
    for chunk in input.chunks(64 * 1024) {
        writer.write_all(chunk).map_err(|_| ())?;
    }
    writer.flush().map_err(|_| ())
}

pub(crate) fn read_request_meta(reader: &mut impl Read) -> Result<RequestMeta, ()> {
    let header = read_header(reader)?;
    if header.kind != REQUEST || header.request_id != REQUEST_ID {
        return Err(());
    }
    let mut metadata = [0_u8; REQUEST_META_BYTES];
    reader.read_exact(&mut metadata).map_err(|_| ())?;
    if metadata[1..8] != [0; 7] {
        return Err(());
    }
    let input_bytes = u64::from_le_bytes(metadata[8..16].try_into().map_err(|_| ())?);
    let maximum_output_bytes = u64::from_le_bytes(metadata[16..24].try_into().map_err(|_| ())?);
    if header.payload_bytes
        != u64::try_from(REQUEST_META_BYTES).map_err(|_| ())?.checked_add(input_bytes).ok_or(())?
    {
        return Err(());
    }
    Ok(RequestMeta {
        source: decode_source(metadata[0])?,
        input_bytes,
        maximum_output_bytes,
        input_sha256: metadata[24..56].try_into().map_err(|_| ())?,
    })
}

pub(crate) fn copy_request_body(
    reader: &mut impl Read,
    writer: &mut impl Write,
    metadata: &RequestMeta,
) -> Result<(), ()> {
    let mut remaining = metadata.input_bytes;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| ())?;
        let count = reader.read(&mut buffer[..wanted]).map_err(|_| ())?;
        if count == 0 {
            return Err(());
        }
        writer.write_all(&buffer[..count]).map_err(|_| ())?;
        hash.update(&buffer[..count]);
        remaining -= u64::try_from(count).map_err(|_| ())?;
    }
    writer.flush().map_err(|_| ())?;
    if hash.finalize().as_slice() != metadata.input_sha256 {
        return Err(());
    }
    Ok(())
}

pub(crate) fn require_eof(reader: &mut impl Read) -> Result<(), ()> {
    let mut trailing = [0_u8; 1];
    match reader.read(&mut trailing) {
        Ok(0) => Ok(()),
        Ok(_) | Err(_) => Err(()),
    }
}

pub(crate) fn write_response(
    writer: &mut impl Write,
    format: NormalizedFormat,
    bytes: &[u8],
) -> Result<(), ()> {
    let output_bytes = u64::try_from(bytes.len()).map_err(|_| ())?;
    let payload_bytes =
        u64::try_from(RESPONSE_META_BYTES).map_err(|_| ())?.checked_add(output_bytes).ok_or(())?;
    write_header(writer, RESPONSE, REQUEST_ID, payload_bytes)?;
    let mut metadata = [0_u8; RESPONSE_META_BYTES];
    metadata[0] = encode_output(format);
    metadata[8..16].copy_from_slice(&output_bytes.to_le_bytes());
    metadata[16..48].copy_from_slice(&Sha256::digest(bytes));
    writer.write_all(&metadata).map_err(|_| ())?;
    for chunk in bytes.chunks(64 * 1024) {
        writer.write_all(chunk).map_err(|_| ())?;
    }
    writer.flush().map_err(|_| ())
}

pub(crate) fn write_error(writer: &mut impl Write, code: u8) -> Result<(), ()> {
    write_header(writer, ERROR, REQUEST_ID, 1)?;
    writer.write_all(&[code]).map_err(|_| ())?;
    writer.flush().map_err(|_| ())
}

pub(crate) fn read_reply(
    reader: &mut impl Read,
    maximum_output_bytes: u64,
) -> Result<WorkerReply, ()> {
    let header = read_header(reader)?;
    if header.request_id != REQUEST_ID {
        return Err(());
    }
    if header.kind == ERROR {
        if header.payload_bytes != 1 {
            return Err(());
        }
        let mut code = [0_u8; 1];
        reader.read_exact(&mut code).map_err(|_| ())?;
        require_eof(reader)?;
        return Ok(WorkerReply::Error(code[0]));
    }
    if header.kind != RESPONSE || header.payload_bytes < RESPONSE_META_BYTES as u64 {
        return Err(());
    }
    let mut metadata = [0_u8; RESPONSE_META_BYTES];
    reader.read_exact(&mut metadata).map_err(|_| ())?;
    if metadata[1..8] != [0; 7] {
        return Err(());
    }
    let length = u64::from_le_bytes(metadata[8..16].try_into().map_err(|_| ())?);
    if length == 0
        || length > maximum_output_bytes
        || header.payload_bytes != RESPONSE_META_BYTES as u64 + length
    {
        return Err(());
    }
    let length = usize::try_from(length).map_err(|_| ())?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(|_| ())?;
    bytes.resize(length, 0);
    reader.read_exact(&mut bytes).map_err(|_| ())?;
    if Sha256::digest(&bytes)[..] != metadata[16..48] {
        return Err(());
    }
    require_eof(reader)?;
    Ok(WorkerReply::Output(Response { format: decode_output(metadata[0])?, bytes }))
}

struct Header {
    kind: u16,
    request_id: u64,
    payload_bytes: u64,
}

fn write_header(
    writer: &mut impl Write,
    kind: u16,
    request_id: u64,
    payload_bytes: u64,
) -> Result<(), ()> {
    let mut header = [0_u8; HEADER_BYTES];
    header[..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&VERSION.to_le_bytes());
    header[6..8].copy_from_slice(&kind.to_le_bytes());
    header[8..16].copy_from_slice(&request_id.to_le_bytes());
    header[16..24].copy_from_slice(&payload_bytes.to_le_bytes());
    writer.write_all(&header).map_err(|_| ())
}

fn read_header(reader: &mut impl Read) -> Result<Header, ()> {
    let mut header = [0_u8; HEADER_BYTES];
    reader.read_exact(&mut header).map_err(|_| ())?;
    if header[..4] != MAGIC || u16::from_le_bytes([header[4], header[5]]) != VERSION {
        return Err(());
    }
    Ok(Header {
        kind: u16::from_le_bytes([header[6], header[7]]),
        request_id: u64::from_le_bytes(header[8..16].try_into().map_err(|_| ())?),
        payload_bytes: u64::from_le_bytes(header[16..24].try_into().map_err(|_| ())?),
    })
}

fn encode_source(format: InputFormat) -> Result<u8, ()> {
    match format {
        InputFormat::Doc => Ok(1),
        InputFormat::Ppt => Ok(2),
        InputFormat::Xls => Ok(3),
        _ => Err(()),
    }
}

fn decode_source(value: u8) -> Result<InputFormat, ()> {
    match value {
        1 => Ok(InputFormat::Doc),
        2 => Ok(InputFormat::Ppt),
        3 => Ok(InputFormat::Xls),
        _ => Err(()),
    }
}

const fn encode_output(format: NormalizedFormat) -> u8 {
    match format {
        NormalizedFormat::Docx => 1,
        NormalizedFormat::Pptx => 2,
        NormalizedFormat::Xlsx => 3,
    }
}

fn decode_output(value: u8) -> Result<NormalizedFormat, ()> {
    match value {
        1 => Ok(NormalizedFormat::Docx),
        2 => Ok(NormalizedFormat::Pptx),
        3 => Ok(NormalizedFormat::Xlsx),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_rejects_truncation_and_format_confusion() {
        let mut bytes = Vec::new();
        write_request(&mut bytes, InputFormat::Doc, b"source", 1024).unwrap();
        let mut cursor = std::io::Cursor::new(bytes.clone());
        let metadata = read_request_meta(&mut cursor).unwrap();
        let mut body = Vec::new();
        copy_request_body(&mut cursor, &mut body, &metadata).unwrap();
        assert_eq!(body, b"source");
        bytes[24] = 99;
        assert!(read_request_meta(&mut std::io::Cursor::new(bytes)).is_err());
    }

    #[test]
    fn response_requires_exact_digest_and_bound() {
        let mut bytes = Vec::new();
        write_response(&mut bytes, NormalizedFormat::Docx, b"PK-package").unwrap();
        assert!(matches!(
            read_reply(&mut std::io::Cursor::new(&bytes), 64).unwrap(),
            WorkerReply::Output(_)
        ));
        assert!(read_reply(&mut std::io::Cursor::new(&bytes), 4).is_err());
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(read_reply(&mut std::io::Cursor::new(bytes), 64).is_err());

        let mut trailing = Vec::new();
        write_response(&mut trailing, NormalizedFormat::Docx, b"PK-package").unwrap();
        trailing.push(0);
        assert!(read_reply(&mut std::io::Cursor::new(trailing), 64).is_err());
    }
}

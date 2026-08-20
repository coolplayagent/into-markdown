use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// The only process-plugin protocol revision implemented by this crate.
pub const PROTOCOL_V1: u32 = 1;
/// Hard ceiling independent of caller policy. A peer cannot make the host allocate beyond it.
pub const ABSOLUTE_MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum HostMessage {
    Hello {
        supported_versions: Vec<u32>,
        plugin_id: String,
        nonce: String,
    },
    Request {
        protocol_version: u32,
        request_id: String,
        input_format: String,
        source_name: Option<String>,
        source_base64: String,
        maximum_output_bytes: u64,
    },
    Cancel {
        protocol_version: u32,
        request_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum PluginMessage {
    Hello {
        selected_version: u32,
        plugin_id: String,
        nonce: String,
    },
    Progress {
        protocol_version: u32,
        request_id: String,
        sequence: u64,
        stage: String,
        completed_units: Option<u64>,
        total_units: Option<u64>,
        message: Option<String>,
    },
    Diagnostic {
        protocol_version: u32,
        request_id: String,
        sequence: u64,
        diagnostic_json: String,
    },
    Response {
        protocol_version: u32,
        request_id: String,
        result_json: String,
    },
    Error {
        protocol_version: u32,
        request_id: Option<String>,
        code: String,
        message: String,
    },
}

pub(crate) fn write_frame<T: Serialize>(
    output: &mut impl Write,
    value: &T,
    maximum: u32,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let length = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame length overflow"))?;
    if length == 0 || length > maximum || length > ABSOLUTE_MAX_FRAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame exceeds limit"));
    }
    output.write_all(&length.to_le_bytes())?;
    output.write_all(&bytes)?;
    output.flush()
}

pub(crate) fn read_frame<T: for<'de> Deserialize<'de>>(
    input: &mut impl Read,
    maximum: u32,
) -> io::Result<T> {
    let mut prefix = [0_u8; 4];
    let mut prefix_bytes = 0_usize;
    while prefix_bytes < prefix.len() {
        match input.read(&mut prefix[prefix_bytes..])? {
            0 if prefix_bytes == 0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended before frame",
                ));
            }
            0 => {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame prefix"));
            }
            read => prefix_bytes += read,
        }
    }
    let length = u32::from_le_bytes(prefix);
    if length == 0 || length > maximum || length > ABSOLUTE_MAX_FRAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame exceeds limit"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length as usize)
        .map_err(|_| io::Error::new(io::ErrorKind::OutOfMemory, "frame allocation failed"))?;
    bytes.resize(length as usize, 0);
    input.read_exact(&mut bytes).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame body")
        } else {
            error
        }
    })?;
    let mut decoder = serde_json::Deserializer::from_slice(&bytes);
    let value = T::deserialize(&mut decoder)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    decoder.end().map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_and_little_endian_prefix() {
        let value = HostMessage::Cancel { protocol_version: 1, request_id: "request-1".into() };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &value, 4096).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize, bytes.len() - 4);
        assert_eq!(read_frame::<HostMessage>(&mut bytes.as_slice(), 4096).unwrap(), value);
    }

    #[test]
    fn empty_oversize_truncated_unknown_and_trailing_frames_fail_closed() {
        for bytes in [
            0_u32.to_le_bytes().to_vec(),
            4097_u32.to_le_bytes().to_vec(),
            [5_u32.to_le_bytes().as_slice(), b"{}"].concat(),
        ] {
            assert!(read_frame::<HostMessage>(&mut bytes.as_slice(), 4096).is_err());
        }
        let unknown = br#"{"type":"cancel","protocol_version":1,"request_id":"x","extra":1}"#;
        let mut framed = u32::try_from(unknown.len()).unwrap().to_le_bytes().to_vec();
        framed.extend_from_slice(unknown);
        assert!(read_frame::<HostMessage>(&mut framed.as_slice(), 4096).is_err());
        let trailing = br#"{"type":"cancel","protocol_version":1,"request_id":"x"} false"#;
        let mut framed = u32::try_from(trailing.len()).unwrap().to_le_bytes().to_vec();
        framed.extend_from_slice(trailing);
        assert!(read_frame::<HostMessage>(&mut framed.as_slice(), 4096).is_err());
    }
}

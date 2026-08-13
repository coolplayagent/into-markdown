use crate::msg::budget::{MsgBudget, malformed};
use into_markdown_core::ConversionError;

const MAGIC_COMPRESSED: u32 = 0x7546_5a4c;
const MAGIC_UNCOMPRESSED: u32 = 0x414c_454d;
const WINDOW: usize = 4096;
const PRELOAD: &[u8] = b"{\\rtf1\\ansi\\mac\\deff0\\deftab720{\\fonttbl;}{\\f0\\fnil \\froman \\fswiss \\fmodern \\fscript \\fdecor MS Sans SerifSymbolArialTimes New RomanCourier{\\colortbl\\red0\\green0\\blue0\r\n\\par \\pard\\plain\\f0\\fs20\\b\\i\\u\\tab\\tx";

/// Decode the complete MS-OXRTFCP `LZFu` envelope without interpreting RTF.
pub(super) fn decompress(
    input: &[u8],
    part: &str,
    budget: &mut MsgBudget<'_>,
) -> Result<Vec<u8>, ConversionError> {
    let header = input.get(..16).ok_or_else(|| malformed(part, "truncated LZFu header"))?;
    let compressed_size = read_u32(header, 0, part)?;
    let raw_size = usize::try_from(read_u32(header, 4, part)?)
        .map_err(|_| malformed(part, "LZFu raw size cannot be represented"))?;
    let magic = read_u32(header, 8, part)?;
    let expected_crc = read_u32(header, 12, part)?;
    if usize::try_from(compressed_size).ok().and_then(|size| size.checked_add(4))
        != Some(input.len())
    {
        return Err(malformed(part, "LZFu compressed size does not match property length"));
    }
    budget.expanded(u64::try_from(raw_size).unwrap_or(u64::MAX))?;
    let payload = &input[16..];
    match magic {
        MAGIC_UNCOMPRESSED => {
            if expected_crc != 0 {
                return Err(malformed(part, "uncompressed RTF CRC is not zero"));
            }
            if payload.len() != raw_size {
                return Err(malformed(part, "uncompressed RTF length does not match LZFu header"));
            }
            Ok(payload.to_vec())
        }
        MAGIC_COMPRESSED => {
            if crc32(payload) != expected_crc {
                return Err(malformed(part, "LZFu payload CRC mismatch"));
            }
            decode_tokens(payload, raw_size, part, budget)
        }
        _ => Err(malformed(part, "invalid LZFu compression magic")),
    }
}

fn decode_tokens(
    payload: &[u8],
    raw_size: usize,
    part: &str,
    budget: &mut MsgBudget<'_>,
) -> Result<Vec<u8>, ConversionError> {
    if PRELOAD.len() >= WINDOW {
        return Err(ConversionError::Internal { detail: "LZFu preload exceeds dictionary".into() });
    }
    let mut dictionary = [0_u8; WINDOW];
    dictionary[..PRELOAD.len()].copy_from_slice(PRELOAD);
    let mut write = PRELOAD.len();
    let mut cursor = 0_usize;
    let mut output = Vec::with_capacity(raw_size);
    loop {
        budget.work(1)?;
        let flags = *payload
            .get(cursor)
            .ok_or_else(|| malformed(part, "LZFu token flags are truncated"))?;
        cursor += 1;
        for bit in 0..8 {
            if flags & (1 << bit) == 0 {
                let value = *payload
                    .get(cursor)
                    .ok_or_else(|| malformed(part, "LZFu literal is truncated"))?;
                cursor += 1;
                if output.len() == raw_size {
                    return Err(malformed(part, "LZFu literal exceeds declared raw size"));
                }
                output.push(value);
                dictionary[write] = value;
                write = (write + 1) & (WINDOW - 1);
            } else {
                let token = payload
                    .get(cursor..cursor + 2)
                    .ok_or_else(|| malformed(part, "LZFu back-reference is truncated"))?;
                cursor += 2;
                let offset = (usize::from(token[0]) << 4) | (usize::from(token[1]) >> 4);
                let length = usize::from(token[1] & 0x0f) + 2;
                if offset == write {
                    if output.len() != raw_size {
                        return Err(malformed(part, "LZFu end marker precedes declared raw size"));
                    }
                    return Ok(output);
                }
                if output.len().checked_add(length).is_none_or(|end| end > raw_size) {
                    return Err(malformed(part, "LZFu back-reference exceeds declared raw size"));
                }
                let mut read = offset;
                for _ in 0..length {
                    let value = dictionary[read];
                    read = (read + 1) & (WINDOW - 1);
                    output.push(value);
                    dictionary[write] = value;
                    write = (write + 1) & (WINDOW - 1);
                }
            }
        }
    }
}

fn read_u32(bytes: &[u8], offset: usize, part: &str) -> Result<u32, ConversionError> {
    let raw =
        bytes.get(offset..offset + 4).ok_or_else(|| malformed(part, "truncated LZFu integer"))?;
    Ok(u32::from_le_bytes(raw.try_into().map_err(|_| malformed(part, "truncated LZFu integer"))?))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ConversionOptions, ExecutionContext, ExecutionOptions};

    fn envelope(magic: u32, raw: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&u32::try_from(payload.len() + 12).unwrap().to_le_bytes());
        output.extend_from_slice(&u32::try_from(raw.len()).unwrap().to_le_bytes());
        output.extend_from_slice(&magic.to_le_bytes());
        let crc = if magic == MAGIC_UNCOMPRESSED { 0 } else { crc32(payload) };
        output.extend_from_slice(&crc.to_le_bytes());
        output.extend_from_slice(payload);
        output
    }

    #[test]
    fn uncompressed_envelope_validates_size_and_zero_crc() {
        let raw = b"{\\rtf1 test}";
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut budget = MsgBudget::new(64, &options, &context).unwrap();
        assert_eq!(
            decompress(&envelope(MAGIC_UNCOMPRESSED, raw, raw), "rtf", &mut budget).unwrap(),
            raw
        );
        let mut corrupt = envelope(MAGIC_UNCOMPRESSED, raw, raw);
        corrupt[12] = 1;
        assert!(decompress(&corrupt, "rtf", &mut budget).is_err());
    }

    #[test]
    fn literal_tokens_decode_without_an_rtf_parser() {
        let raw = b"hello";
        let payload = [0x20, b'h', b'e', b'l', b'l', b'o', 0x0d, 0x40, 0, 0];
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut budget = MsgBudget::new(64, &options, &context).unwrap();
        assert_eq!(
            decompress(&envelope(MAGIC_COMPRESSED, raw, &payload), "rtf", &mut budget).unwrap(),
            raw
        );
    }
}

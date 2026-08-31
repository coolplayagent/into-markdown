use super::budget::{Budget, limit, malformed};
use crate::text::LogicalMemory;
use base64::Engine as _;
use flate2::{Decompress, FlushDecompress, Status};
use into_markdown_core::ConversionError;

pub(super) struct Decoded {
    pub bytes: Vec<u8>,
    pub _memory: LogicalMemory,
}

pub(super) fn decode(payload: &str, budget: &mut Budget<'_>) -> Result<Decoded, ConversionError> {
    let mut memory = LogicalMemory::new(budget.context)?;
    let compressed = base64_decode(payload, budget, &mut memory)?;
    // flate2's pure-Rust DEFLATE window and state have a fixed bounded workspace.
    let _workspace = budget.context.reserve_memory(256 * 1024)?;
    let inflated = inflate(&compressed, budget, &mut memory)?;
    let bytes = uri_decode(&inflated, budget, &mut memory)?;
    Ok(Decoded { bytes, _memory: memory })
}

fn base64_decode(
    payload: &str,
    budget: &Budget<'_>,
    memory: &mut LogicalMemory,
) -> Result<Vec<u8>, ConversionError> {
    let mut compact = Vec::new();
    for chunk in payload.as_bytes().chunks(4096) {
        budget.context.checkpoint()?;
        for &byte in chunk {
            if !byte.is_ascii_whitespace() {
                memory.reserve_vec(&mut compact, 1)?;
                compact.push(byte);
            }
        }
    }
    if compact.is_empty() {
        return Err(malformed("diagram payload is empty"));
    }
    let mut result = Vec::new();
    for (index, chunk) in compact.chunks(4096).enumerate() {
        budget.context.checkpoint()?;
        let mut output = [0u8; 3072];
        let n = base64::engine::general_purpose::STANDARD
            .decode_slice(chunk, &mut output)
            .map_err(|e| malformed(format!("invalid Base64 diagram: {e}")))?;
        // Padding can occur only in the final quartet of the entire payload.
        if (index + 1) * 4096 < compact.len() && chunk.contains(&b'=') {
            return Err(malformed("Base64 padding before end of diagram"));
        }
        memory.reserve_vec(&mut result, n)?;
        result.extend_from_slice(&output[..n]);
    }
    Ok(result)
}

fn inflate(
    compressed: &[u8],
    budget: &mut Budget<'_>,
    memory: &mut LogicalMemory,
) -> Result<Vec<u8>, ConversionError> {
    let mut decoder = Decompress::new(false);
    let mut result = Vec::new();
    loop {
        budget.context.checkpoint()?;
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let mut chunk = [0; 8192];
        let status = decoder
            .decompress(
                &compressed[super::budget::size(before_in)?..],
                &mut chunk,
                FlushDecompress::None,
            )
            .map_err(|e| malformed(format!("invalid Deflate diagram: {e}")))?;
        let written = super::budget::size(decoder.total_out() - before_out)?;
        budget.expand(written)?;
        memory.reserve_vec(&mut result, written)?;
        result.extend_from_slice(&chunk[..written]);
        if status == Status::StreamEnd {
            if decoder.total_in() != compressed.len() as u64 {
                return Err(malformed("trailing bytes after Deflate stream"));
            }
            return Ok(result);
        }
        if decoder.total_in() == before_in && written == 0 {
            return Err(malformed("truncated Deflate diagram"));
        }
    }
}

fn uri_decode(
    encoded: &[u8],
    budget: &mut Budget<'_>,
    memory: &mut LogicalMemory,
) -> Result<Vec<u8>, ConversionError> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < encoded.len() {
        if index % 4096 < 3 {
            budget.context.checkpoint()?;
        }
        let byte = if encoded[index] == b'%' {
            let pair = encoded
                .get(index + 1..index + 3)
                .ok_or_else(|| malformed("truncated URI escape"))?;
            index += 3;
            (hex(pair[0])? << 4) | hex(pair[1])?
        } else {
            let b = encoded[index];
            index += 1;
            b
        };
        memory.reserve_vec(&mut result, 1)?;
        result.push(byte);
    }
    budget.expand(result.len())?;
    std::str::from_utf8(&result).map_err(|_| malformed("decoded diagram must be valid UTF-8"))?;
    if result.len() as u64 > budget.options.limits.max_decompressed_bytes {
        return Err(limit("max_decompressed_bytes", "decoded diagram is too large"));
    }
    Ok(result)
}

fn hex(byte: u8) -> Result<u8, ConversionError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(malformed("invalid URI escape")),
    }
}

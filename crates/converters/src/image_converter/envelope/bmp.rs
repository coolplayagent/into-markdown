use super::intervals::IntervalGraph;
use super::meter::Meter;
use super::{Summary, limit, malformed, read_u16, read_u32, unsupported};
use into_markdown_core::{ConversionError, ExecutionContext};

const PROFILE_LINKED: u32 = 0x4c49_4e4b;
const PROFILE_EMBEDDED: u32 = 0x4d42_4544;

#[allow(clippy::too_many_lines)]
pub(super) fn validate(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Summary, ConversionError> {
    if bytes.get(..2) != Some(b"BM") {
        return Err(malformed("BMP signature is invalid"));
    }
    if bytes.get(6..10).is_none_or(|reserved| reserved != [0, 0, 0, 0]) {
        return Err(malformed("BMP reserved file-header fields must be zero"));
    }
    scan_all(bytes, context)?;
    let file_size = usize::try_from(
        read_u32(bytes, 2, true).ok_or_else(|| malformed("BMP file size is truncated"))?,
    )
    .map_err(|_| limit("max_input_bytes", "BMP file size is unrepresentable"))?;
    if file_size != bytes.len() {
        return Err(malformed("BMP declared file size must equal the complete input"));
    }
    let pixel_offset = usize::try_from(
        read_u32(bytes, 10, true).ok_or_else(|| malformed("BMP pixel offset is truncated"))?,
    )
    .map_err(|_| limit("max_input_bytes", "BMP pixel offset is unrepresentable"))?;
    let dib = usize::try_from(
        read_u32(bytes, 14, true).ok_or_else(|| malformed("BMP DIB size is truncated"))?,
    )
    .map_err(|_| limit("max_input_bytes", "BMP DIB size is unrepresentable"))?;
    if !matches!(dib, 12 | 40 | 52 | 56 | 64 | 108 | 124) {
        return Err(unsupported(format!("BMP DIB header size {dib} is not supported")));
    }
    let header_end = 14_usize
        .checked_add(dib)
        .ok_or_else(|| limit("max_input_bytes", "BMP header offset overflowed"))?;
    if header_end > bytes.len() || pixel_offset < header_end || pixel_offset >= bytes.len() {
        return Err(malformed("BMP header and pixel ranges are inconsistent"));
    }
    let (width, height, top_down, planes, bits, compression, image_size) = if dib == 12 {
        (
            u32::from(
                read_u16(bytes, 18, true).ok_or_else(|| malformed("BMP width is truncated"))?,
            ),
            u32::from(
                read_u16(bytes, 20, true).ok_or_else(|| malformed("BMP height is truncated"))?,
            ),
            false,
            read_u16(bytes, 22, true),
            read_u16(bytes, 24, true),
            0,
            0,
        )
    } else {
        let signed_width = i32::from_le_bytes(
            read_u32(bytes, 18, true)
                .ok_or_else(|| malformed("BMP width is truncated"))?
                .to_le_bytes(),
        );
        let signed_height = i32::from_le_bytes(
            read_u32(bytes, 22, true)
                .ok_or_else(|| malformed("BMP height is truncated"))?
                .to_le_bytes(),
        );
        if signed_width <= 0 || signed_height == 0 || signed_height == i32::MIN {
            return Err(malformed("BMP signed dimensions are invalid"));
        }
        (
            signed_width.unsigned_abs(),
            signed_height.unsigned_abs(),
            signed_height < 0,
            read_u16(bytes, 26, true),
            read_u16(bytes, 28, true),
            read_u32(bytes, 30, true).ok_or_else(|| malformed("BMP compression is truncated"))?,
            read_u32(bytes, 34, true).ok_or_else(|| malformed("BMP image size is truncated"))?,
        )
    };
    if width == 0
        || height == 0
        || planes != Some(1)
        || bits.is_none_or(|value| !matches!(value, 1 | 2 | 4 | 8 | 16 | 24 | 32))
    {
        return Err(malformed("BMP dimensions, planes, or bit depth are invalid"));
    }
    let bits = bits.unwrap_or(0);
    if matches!(compression, 1) && bits != 8
        || matches!(compression, 2) && bits != 4
        || matches!(compression, 3 | 6) && !matches!(bits, 16 | 32)
        || top_down && !matches!(compression, 0 | 3 | 6)
    {
        return Err(malformed("BMP compression is inconsistent with its bit depth or row order"));
    }

    let mut intervals = IntervalGraph::new(6, context)?;
    intervals.add(0, 14, bytes.len())?;
    intervals.add(14, header_end, bytes.len())?;
    let mut supplemental_end = header_end;
    if matches!(compression, 3 | 6) {
        let external_masks = match (dib, compression) {
            (40, 3) => 12_usize,
            (40, 6) => 16,
            (52, 6) => {
                return Err(malformed("BMP alpha bitfields require a four-mask DIB variant"));
            }
            _ => 0,
        };
        if external_masks > 0 {
            let end = supplemental_end
                .checked_add(external_masks)
                .ok_or_else(|| malformed("BMP bitfield-mask range overflow"))?;
            intervals.add(supplemental_end, end, bytes.len())?;
            supplemental_end = end;
        }
        validate_masks(bytes, dib, compression, bits, header_end)?;
    } else if dib >= 52 && bytes[54..66].iter().any(|byte| *byte != 0) {
        return Err(malformed("BMP declares bitfield masks without bitfield compression"));
    }

    let palette_entries = palette_entries(bytes, dib, bits)?;
    if palette_entries > 0 {
        let entry_size = if dib == 12 { 3_usize } else { 4 };
        let palette_bytes = palette_entries
            .checked_mul(entry_size)
            .ok_or_else(|| malformed("BMP palette size overflow"))?;
        let end = supplemental_end
            .checked_add(palette_bytes)
            .ok_or_else(|| malformed("BMP palette range overflow"))?;
        intervals.add(supplemental_end, end, bytes.len())?;
        supplemental_end = end;
    }

    let pixel_end = match compression {
        0 | 3 | 6 => {
            let row_bits = u64::from(width)
                .checked_mul(u64::from(bits))
                .ok_or_else(|| limit("image_pixels", "BMP row size overflowed"))?;
            let row_bytes = row_bits
                .checked_add(31)
                .map(|value| value / 32 * 4)
                .ok_or_else(|| limit("image_pixels", "BMP row size overflowed"))?;
            let pixels = row_bytes
                .checked_mul(u64::from(height))
                .ok_or_else(|| limit("image_pixels", "BMP pixel size overflowed"))?;
            let end = u64::try_from(pixel_offset)
                .unwrap_or(u64::MAX)
                .checked_add(pixels)
                .ok_or_else(|| limit("image_pixels", "BMP pixel end overflowed"))?;
            if image_size != 0 && u64::from(image_size) != pixels {
                return Err(malformed("BMP image-size field disagrees with exact pixel rows"));
            }
            usize::try_from(end)
                .map_err(|_| limit("image_pixels", "BMP pixel end is unrepresentable"))?
        }
        1 | 2 => {
            if image_size == 0 {
                return Err(malformed("BMP RLE payload must declare an exact size"));
            }
            pixel_offset
                .checked_add(usize::try_from(image_size).map_err(|_| {
                    limit("image_pixels", "BMP RLE payload size is unrepresentable")
                })?)
                .ok_or_else(|| limit("image_pixels", "BMP RLE pixel end overflowed"))?
        }
        4 | 5 => return Err(unsupported("BMP embedded JPEG/PNG payloads are not executed")),
        other => return Err(unsupported(format!("BMP compression mode {other} is unsupported"))),
    };
    intervals.add(pixel_offset, pixel_end, bytes.len())?;

    if dib == 124 {
        let color_space = read_u32(bytes, 14 + 56, true)
            .ok_or_else(|| malformed("BMP V5 color-space field is truncated"))?;
        let profile_offset = usize::try_from(
            read_u32(bytes, 14 + 112, true)
                .ok_or_else(|| malformed("BMP V5 profile offset is truncated"))?,
        )
        .map_err(|_| malformed("BMP V5 profile offset is unrepresentable"))?;
        let profile_size = usize::try_from(
            read_u32(bytes, 14 + 116, true)
                .ok_or_else(|| malformed("BMP V5 profile size is truncated"))?,
        )
        .map_err(|_| malformed("BMP V5 profile size is unrepresentable"))?;
        if color_space == PROFILE_LINKED {
            return Err(unsupported("BMP linked color profiles are not opened"));
        }
        if color_space == PROFILE_EMBEDDED {
            if profile_offset < dib || profile_size == 0 {
                return Err(malformed("BMP embedded profile range is invalid"));
            }
            let start = 14_usize
                .checked_add(profile_offset)
                .ok_or_else(|| malformed("BMP embedded profile offset overflow"))?;
            let end = start
                .checked_add(profile_size)
                .ok_or_else(|| malformed("BMP embedded profile range overflow"))?;
            intervals.add(start, end, bytes.len())?;
        } else if profile_offset != 0 || profile_size != 0 {
            return Err(malformed("BMP profile fields require an embedded profile color space"));
        }
    }
    if pixel_offset != supplemental_end && dib != 124 {
        return Err(malformed("BMP contains bytes between its palette or masks and pixels"));
    }
    intervals.require_exact_coverage(bytes, 1)?;
    Ok(Summary { frames: 1, animated: false })
}

fn palette_entries(bytes: &[u8], dib: usize, bits: u16) -> Result<usize, ConversionError> {
    if dib == 12 {
        return if bits <= 8 {
            1_usize
                .checked_shl(u32::from(bits))
                .ok_or_else(|| malformed("BMP core palette count overflow"))
        } else {
            Ok(0)
        };
    }
    let declared = usize::try_from(
        read_u32(bytes, 46, true).ok_or_else(|| malformed("BMP palette count is truncated"))?,
    )
    .map_err(|_| malformed("BMP palette count is unrepresentable"))?;
    let maximum = if bits <= 8 {
        1_usize
            .checked_shl(u32::from(bits))
            .ok_or_else(|| malformed("BMP palette count overflow"))?
    } else {
        256
    };
    let entries = if declared == 0 && bits <= 8 { maximum } else { declared };
    if entries > maximum {
        return Err(malformed("BMP palette count exceeds its bit-depth policy"));
    }
    Ok(entries)
}

fn validate_masks(
    bytes: &[u8],
    dib: usize,
    compression: u32,
    bits: u16,
    header_end: usize,
) -> Result<(), ConversionError> {
    let start = if dib == 40 { header_end } else { 14 + 40 };
    let count = if compression == 6 { 4_usize } else { 3 };
    let mut union = 0_u32;
    for index in 0..count {
        let mask = read_u32(bytes, start + index * 4, true)
            .ok_or_else(|| malformed("BMP bitfield mask is truncated"))?;
        if mask == 0 || mask & union != 0 || (bits < 32 && mask >= (1_u32 << bits)) {
            return Err(malformed("BMP bitfield masks are empty, overlapping, or out of range"));
        }
        union |= mask;
    }
    Ok(())
}

fn scan_all(bytes: &[u8], context: &ExecutionContext) -> Result<(), ConversionError> {
    let mut meter = Meter::new(context);
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let length = (bytes.len() - offset).min(meter.next_batch());
        meter.consume(length)?;
        offset += length;
    }
    Ok(())
}

use super::meter::Meter;
use super::{Summary, limit, malformed, read_u16, read_u32, unsupported};
use into_markdown_core::{ConversionError, ExecutionContext};

#[allow(clippy::too_many_lines)]
pub(super) fn validate(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Summary, ConversionError> {
    if bytes.get(..2) != Some(b"BM") {
        return Err(malformed("BMP signature is invalid"));
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
    let bits = u64::from(bits.unwrap_or(0));
    if matches!(compression, 1) && bits != 8
        || matches!(compression, 2) && bits != 4
        || matches!(compression, 3 | 6) && !matches!(bits, 16 | 32)
        || top_down && !matches!(compression, 0 | 3 | 6)
    {
        return Err(malformed("BMP compression is inconsistent with its bit depth or row order"));
    }
    match compression {
        0 | 3 | 6 => {
            let row_bits = u64::from(width)
                .checked_mul(bits)
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
            if end != u64::try_from(bytes.len()).unwrap_or(u64::MAX) {
                return Err(malformed("BMP uncompressed pixel rows must end exactly at EOF"));
            }
            if image_size != 0 && u64::from(image_size) != pixels {
                return Err(malformed("BMP image-size field disagrees with exact pixel rows"));
            }
        }
        1 | 2 => {
            if image_size == 0
                || usize::try_from(image_size).ok().and_then(|size| pixel_offset.checked_add(size))
                    != Some(bytes.len())
            {
                return Err(malformed("BMP RLE payload must declare an exact size ending at EOF"));
            }
        }
        4 | 5 => return Err(unsupported("BMP embedded JPEG/PNG payloads are not executed")),
        other => return Err(unsupported(format!("BMP compression mode {other} is unsupported"))),
    }
    Ok(Summary { frames: 1, animated: false })
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

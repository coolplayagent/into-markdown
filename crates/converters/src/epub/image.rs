//! Complete, budgeted validation for retained EPUB raster resources.

use ::image::{
    AnimationDecoder as _, ImageDecoder as _, ImageFormat, ImageReader, Limits as ImageLimits,
    codecs::gif::GifDecoder,
};
use into_markdown_core::{ConversionError, ExecutionContext, MAX_DOCUMENT_NODES, ResourceLimits};
use std::io::Cursor;

struct RasterInfo {
    dimensions: (u32, u32),
    frames: u64,
}

#[allow(clippy::too_many_lines)] // Decode budget and complete payload checks share one transaction.
pub(super) fn validate(
    bytes: &[u8],
    media_type: &str,
    part: &str,
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let format = format(media_type)?;
    context.checkpoint()?;
    let info = envelope_info(bytes, format, context);
    context.checkpoint()?;
    let info = info
        .ok_or_else(|| malformed(part, "raster container is truncated or structurally invalid"))?;
    let dimensions = info.dimensions;
    let decoded_rgba = u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| limit("image_pixels", part, "image dimensions overflow"))?;
    let cumulative_rgba = decoded_rgba
        .checked_mul(info.frames)
        .ok_or_else(|| limit("max_decompressed_bytes", part, "decoded frame total overflow"))?;
    if cumulative_rgba > limits.max_decompressed_bytes {
        return Err(limit(
            "max_decompressed_bytes",
            part,
            format!("{} raster frame(s) exceed the decompressed-byte budget", info.frames),
        ));
    }
    let maximum_pixel_bytes = decoded_rgba
        .checked_mul(2)
        .ok_or_else(|| limit("image_decode_memory", part, "decode size overflow"))?;
    let compressed = u64::try_from(bytes.len())
        .map_err(|_| limit("image_decode_memory", part, "compressed size is unrepresentable"))?;
    let working_set = maximum_pixel_bytes
        .checked_mul(3)
        .and_then(|value| compressed.checked_mul(2).and_then(|size| value.checked_add(size)))
        .and_then(|value| value.checked_add(256 * 1024))
        .ok_or_else(|| limit("image_decode_memory", part, "decode working set overflow"))?;
    let _decode_memory = context.reserve_memory(working_set)?;
    context.checkpoint()?;

    let mut image_limits = ImageLimits::default();
    image_limits.max_image_width = Some(dimensions.0);
    image_limits.max_image_height = Some(dimensions.1);
    image_limits.max_alloc = Some(working_set);
    if format == ImageFormat::Gif {
        let mut decoder = GifDecoder::new(Cursor::new(bytes))
            .map_err(|_| malformed(part, "GIF decoder rejected the image header"))?;
        decoder
            .set_limits(image_limits)
            .map_err(|_| malformed(part, "GIF decoder rejected the resource limits"))?;
        if decoder.dimensions() != dimensions {
            return Err(malformed(part, "GIF dimensions disagree with its container"));
        }
        let mut frames = 0_usize;
        let mut decoded_total = 0_u64;
        for frame in decoder.into_frames() {
            context.checkpoint()?;
            let frame =
                frame.map_err(|_| malformed(part, "GIF payload is not completely decodable"))?;
            decoded_total = decoded_total
                .checked_add(u64::try_from(frame.buffer().as_raw().len()).unwrap_or(u64::MAX))
                .ok_or_else(|| limit("max_decompressed_bytes", part, "GIF work overflow"))?;
            if decoded_total > limits.max_decompressed_bytes {
                return Err(limit(
                    "max_decompressed_bytes",
                    part,
                    "GIF decoded work exceeds the configured budget",
                ));
            }
            frames = frames
                .checked_add(1)
                .ok_or_else(|| limit("image_frames", part, "GIF frame count overflow"))?;
            if frames > MAX_DOCUMENT_NODES {
                return Err(limit("image_frames", part, "GIF frame count exceeds IR limits"));
            }
            context.checkpoint()?;
        }
        if frames == 0 || u64::try_from(frames).ok() != Some(info.frames) {
            return Err(malformed(part, "GIF has no decodable frames"));
        }
        return context.checkpoint();
    }

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(image_limits);
    let decoder = reader
        .into_decoder()
        .map_err(|_| malformed(part, "raster decoder rejected the image header"))?;
    if decoder.dimensions() != dimensions {
        return Err(malformed(part, "decoder dimensions disagree with the raster container"));
    }
    let decoded_size = decoder.total_bytes();
    if decoded_size > limits.max_decompressed_bytes {
        return Err(limit(
            "max_decompressed_bytes",
            part,
            "decoded raster exceeds the configured budget",
        ));
    }
    let decoded_size = usize::try_from(decoded_size)
        .map_err(|_| limit("max_decompressed_bytes", part, "decoded size is unrepresentable"))?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(decoded_size)
        .map_err(|_| limit("max_memory_bytes", part, "cannot reserve decoded raster"))?;
    pixels.resize(decoded_size, 0);
    decoder
        .read_image(&mut pixels)
        .map_err(|_| malformed(part, "raster payload is not completely decodable"))?;
    context.checkpoint()
}

fn format(media_type: &str) -> Result<ImageFormat, ConversionError> {
    match media_type {
        "image/png" => Ok(ImageFormat::Png),
        "image/jpeg" => Ok(ImageFormat::Jpeg),
        "image/gif" => Ok(ImageFormat::Gif),
        "image/webp" => Ok(ImageFormat::WebP),
        _ => Err(ConversionError::Unsupported {
            detail: format!("EPUB raster media type {media_type} is not supported"),
        }),
    }
}

fn envelope_info(
    bytes: &[u8],
    format: ImageFormat,
    context: &ExecutionContext,
) -> Option<RasterInfo> {
    match format {
        ImageFormat::Png => png_info(bytes, context),
        ImageFormat::Jpeg => jpeg_info(bytes, context),
        ImageFormat::Gif => gif_info(bytes, context),
        ImageFormat::WebP => webp_info(bytes, context),
        _ => None,
    }
}

fn png_info(bytes: &[u8], context: &ExecutionContext) -> Option<RasterInfo> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut offset = 8_usize;
    let mut dimensions = None;
    while offset < bytes.len() {
        if context.checkpoint().is_err() {
            return None;
        }
        let length = usize::try_from(big_u32(bytes, offset)?).ok()?;
        let kind = bytes.get(offset + 4..offset + 8)?;
        let end = offset.checked_add(12)?.checked_add(length)?;
        if end > bytes.len() {
            return None;
        }
        if dimensions.is_none() {
            if kind != b"IHDR" || length != 13 {
                return None;
            }
            dimensions = nonzero((big_u32(bytes, offset + 8)?, big_u32(bytes, offset + 12)?));
        }
        offset = end;
        if kind == b"IEND" {
            if length != 0 || offset != bytes.len() {
                return None;
            }
            return Some(RasterInfo { dimensions: dimensions?, frames: 1 });
        }
    }
    None
}

fn gif_info(bytes: &[u8], context: &ExecutionContext) -> Option<RasterInfo> {
    if bytes.len() < 14 || !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return None;
    }
    let dimensions = nonzero((u32::from(little_u16(bytes, 6)?), u32::from(little_u16(bytes, 8)?)))?;
    let packed = *bytes.get(10)?;
    let mut offset = 13_usize;
    if packed & 0x80 != 0 {
        offset = offset.checked_add(3_usize.checked_mul(1 << (usize::from(packed & 7) + 1))?)?;
    }
    let mut frames = 0_u64;
    loop {
        if context.checkpoint().is_err() {
            return None;
        }
        match *bytes.get(offset)? {
            0x3b => {
                return (offset + 1 == bytes.len() && frames > 0)
                    .then_some(RasterInfo { dimensions, frames });
            }
            0x21 => {
                offset = offset.checked_add(2)?;
                offset = skip_sub_blocks(bytes, offset)?;
            }
            0x2c => {
                let packed = *bytes.get(offset + 9)?;
                offset = offset.checked_add(10)?;
                if packed & 0x80 != 0 {
                    offset = offset
                        .checked_add(3_usize.checked_mul(1 << (usize::from(packed & 7) + 1))?)?;
                }
                bytes.get(offset)?;
                offset = skip_sub_blocks(bytes, offset + 1)?;
                frames = frames.checked_add(1)?;
            }
            _ => return None,
        }
    }
}

fn skip_sub_blocks(bytes: &[u8], mut offset: usize) -> Option<usize> {
    loop {
        let length = usize::from(*bytes.get(offset)?);
        offset = offset.checked_add(1)?;
        if length == 0 {
            return Some(offset);
        }
        offset = offset.checked_add(length)?;
        bytes.get(offset - 1)?;
    }
}

fn webp_info(bytes: &[u8], context: &ExecutionContext) -> Option<RasterInfo> {
    if bytes.len() < 20
        || !bytes.starts_with(b"RIFF")
        || bytes.get(8..12) != Some(b"WEBP")
        || usize::try_from(little_u32(bytes, 4)?).ok()? != bytes.len() - 8
    {
        return None;
    }
    let mut offset = 12_usize;
    let mut dimensions = None;
    let mut primary = false;
    while offset < bytes.len() {
        if context.checkpoint().is_err() {
            return None;
        }
        let kind = bytes.get(offset..offset + 4)?;
        let length = usize::try_from(little_u32(bytes, offset + 4)?).ok()?;
        let data = offset.checked_add(8)?;
        let end = data.checked_add(length)?;
        if end > bytes.len() {
            return None;
        }
        match kind {
            b"VP8X" if dimensions.is_none() && length == 10 => {
                dimensions = nonzero((
                    1 + u32::from_le_bytes([bytes[data + 4], bytes[data + 5], bytes[data + 6], 0]),
                    1 + u32::from_le_bytes([bytes[data + 7], bytes[data + 8], bytes[data + 9], 0]),
                ));
            }
            b"VP8L" if !primary && length >= 5 && bytes[data] == 0x2f => {
                let bits = little_u32(bytes, data + 1)?;
                dimensions
                    .get_or_insert(nonzero(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))?);
                primary = true;
            }
            b"VP8 "
                if !primary
                    && length >= 10
                    && bytes.get(data + 3..data + 6) == Some(&[0x9d, 0x01, 0x2a]) =>
            {
                dimensions.get_or_insert(nonzero((
                    u32::from(little_u16(bytes, data + 6)? & 0x3fff),
                    u32::from(little_u16(bytes, data + 8)? & 0x3fff),
                ))?);
                primary = true;
            }
            b"VP8L" | b"VP8 " | b"VP8X" => return None,
            _ => {}
        }
        offset = end.checked_add(length & 1)?;
    }
    (offset == bytes.len() && primary).then_some(RasterInfo { dimensions: dimensions?, frames: 1 })
}

fn jpeg_info(bytes: &[u8], context: &ExecutionContext) -> Option<RasterInfo> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut offset = 2_usize;
    let mut dimensions = None;
    while offset < bytes.len() {
        if context.checkpoint().is_err() {
            return None;
        }
        if bytes[offset] != 0xff {
            return None;
        }
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        if marker == 0xd9 {
            return (offset == bytes.len())
                .then_some(RasterInfo { dimensions: dimensions?, frames: 1 });
        }
        if marker == 0x01 || matches!(marker, 0xd0..=0xd7) {
            continue;
        }
        let length = usize::from(big_u16(bytes, offset)?);
        if length < 2 {
            return None;
        }
        let end = offset.checked_add(length)?;
        if end > bytes.len() {
            return None;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if length < 8 || dimensions.is_some() {
                return None;
            }
            dimensions = nonzero((
                u32::from(big_u16(bytes, offset + 5)?),
                u32::from(big_u16(bytes, offset + 3)?),
            ));
        }
        offset = offset.checked_add(length)?;
        if marker == 0xda {
            offset = skip_jpeg_scan(bytes, offset)?;
        }
    }
    None
}

fn skip_jpeg_scan(bytes: &[u8], mut offset: usize) -> Option<usize> {
    while offset < bytes.len() {
        if bytes[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker_start = offset;
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        match *bytes.get(offset)? {
            0x00 | 0xd0..=0xd7 => offset += 1,
            _ => return Some(marker_start),
        }
    }
    None
}

fn nonzero(dimensions: (u32, u32)) -> Option<(u32, u32)> {
    (dimensions.0 != 0 && dimensions.1 != 0).then_some(dimensions)
}

fn little_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?))
}

fn little_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

fn big_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?))
}

fn big_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

fn malformed(part: &str, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some(part.into()), detail: detail.into() }
}

fn limit(limit: &'static str, part: &str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit {
        limit,
        detail: format!("EPUB image {part:?}: {}", detail.into()),
    }
}

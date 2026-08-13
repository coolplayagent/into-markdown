//! Complete, budgeted validation for retained EPUB raster resources.

use ::image::{
    AnimationDecoder as _, ImageDecoder as _, ImageFormat, ImageReader, Limits as ImageLimits,
    codecs::gif::GifDecoder,
};
use into_markdown_core::{ConversionError, ExecutionContext, MAX_DOCUMENT_NODES, ResourceLimits};
use std::io::Cursor;

pub(super) fn validate(
    bytes: &[u8],
    media_type: &str,
    part: &str,
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let format = format(media_type)?;
    let dimensions = envelope_dimensions(bytes, format)
        .ok_or_else(|| malformed(part, "raster container is truncated or structurally invalid"))?;
    let decoded_rgba = u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| limit("image_pixels", part, "image dimensions overflow"))?;
    if decoded_rgba > limits.max_decompressed_bytes {
        return Err(limit(
            "image_pixels",
            part,
            format!(
                "{}x{} raster exceeds the decompressed-byte budget",
                dimensions.0, dimensions.1
            ),
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
        for frame in decoder.into_frames() {
            context.checkpoint()?;
            frame.map_err(|_| malformed(part, "GIF payload is not completely decodable"))?;
            frames = frames
                .checked_add(1)
                .ok_or_else(|| limit("image_frames", part, "GIF frame count overflow"))?;
            if frames > MAX_DOCUMENT_NODES {
                return Err(limit("image_frames", part, "GIF frame count exceeds IR limits"));
            }
        }
        if frames == 0 {
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

fn envelope_dimensions(bytes: &[u8], format: ImageFormat) -> Option<(u32, u32)> {
    match format {
        ImageFormat::Png => png_dimensions(bytes),
        ImageFormat::Jpeg => jpeg_dimensions(bytes),
        ImageFormat::Gif => gif_dimensions(bytes),
        ImageFormat::WebP => webp_dimensions(bytes),
        _ => None,
    }
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 45
        || !bytes.starts_with(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR")
        || bytes.get(bytes.len() - 12..) != Some(b"\0\0\0\0IEND\xaeB`\x82")
    {
        return None;
    }
    nonzero((big_u32(bytes, 16)?, big_u32(bytes, 20)?))
}

fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 14
        || !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"))
        || bytes.last() != Some(&0x3b)
    {
        return None;
    }
    nonzero((u32::from(little_u16(bytes, 6)?), u32::from(little_u16(bytes, 8)?)))
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 20
        || !bytes.starts_with(b"RIFF")
        || bytes.get(8..12) != Some(b"WEBP")
        || usize::try_from(little_u32(bytes, 4)?).ok()? != bytes.len() - 8
    {
        return None;
    }
    match bytes.get(12..16)? {
        b"VP8X" if bytes.get(16..20) == Some(&[10, 0, 0, 0]) && bytes.len() >= 30 => nonzero((
            1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]),
            1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]),
        )),
        b"VP8L" if bytes.len() >= 25 && bytes[20] == 0x2f => {
            let bits = little_u32(bytes, 21)?;
            nonzero(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8 " if bytes.len() >= 30 && bytes.get(23..26) == Some(&[0x9d, 0x01, 0x2a]) => {
            nonzero((
                u32::from(little_u16(bytes, 26)? & 0x3fff),
                u32::from(little_u16(bytes, 28)? & 0x3fff),
            ))
        }
        _ => None,
    }
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return None;
    }
    let mut offset = 2_usize;
    while offset + 4 <= bytes.len() - 2 {
        if bytes[offset] != 0xff {
            return None;
        }
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let length = usize::from(big_u16(bytes, offset)?);
            if length < 8 || offset.checked_add(length)? > bytes.len() {
                return None;
            }
            return nonzero((
                u32::from(big_u16(bytes, offset + 5)?),
                u32::from(big_u16(bytes, offset + 3)?),
            ));
        }
        if marker == 0xda || marker == 0xd9 {
            return None;
        }
        if marker == 0x01 || matches!(marker, 0xd0..=0xd7) {
            continue;
        }
        let length = usize::from(big_u16(bytes, offset)?);
        if length < 2 {
            return None;
        }
        offset = offset.checked_add(length)?;
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

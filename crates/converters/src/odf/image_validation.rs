use crate::odf::model::{MAX_IMAGE_DIMENSION, MAX_IMAGE_PIXELS, limit, malformed};
use crate::odf::raw_zip::image_decode_plan;
use image::{ImageDecoder as _, ImageFormat, ImageReader, Limits as ImageLimits};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use std::io::Cursor;
use std::path::Path;

pub(super) fn unsupported_media(media_type: &str) -> bool {
    matches!(
        media_type,
        "image/svg+xml" | "application/x-openoffice-gdimetafile;windows_formatname=\"GDIMetaFile\""
    )
}

pub(super) fn image_profile(path: &str, media_type: &str) -> Result<ImageFormat, ConversionError> {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| malformed(Some(path), "image part lacks a canonical extension"))?;
    match (media_type, extension.as_str()) {
        ("image/png", "png") => Ok(ImageFormat::Png),
        ("image/jpeg", "jpg" | "jpeg") => Ok(ImageFormat::Jpeg),
        ("image/gif", "gif") => Ok(ImageFormat::Gif),
        ("image/webp", "webp") => Ok(ImageFormat::WebP),
        _ => Err(malformed(
            Some(path),
            "ODF image manifest media type and canonical extension disagree or are unsupported",
        )),
    }
}

pub(super) fn validate_image(
    bytes: &[u8],
    media_type: &str,
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
    package_peak: u64,
    preflight: u64,
) -> Result<(), ConversionError> {
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if size > options.limits.max_asset_bytes {
        return Err(limit(
            "max_asset_bytes",
            format!("{part}: {size} > {}", options.limits.max_asset_bytes),
        ));
    }
    let format = ImageFormat::from_mime_type(media_type)
        .filter(|format| {
            matches!(
                format,
                ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
            )
        })
        .ok_or_else(|| malformed(Some(part), "unsupported ODF image media type"))?;
    if image_profile(part, media_type)? != format
        || image::guess_format(bytes).map_err(|_| {
            malformed(Some(part), "image bytes do not have a supported sniffed signature")
        })? != format
    {
        return Err(malformed(
            Some(part),
            "image manifest media type, canonical extension, and sniffed bytes disagree",
        ));
    }
    // Authenticate the maximum codec/header/decoded-output working set before constructing the
    // third-party decoder. The dimension-specific value below can only reduce this ceiling.
    let decoder_ceiling = image_decode_plan(size, MAX_IMAGE_PIXELS)?;
    let decoder_ceiling_peak = package_peak
        .checked_add(decoder_ceiling)
        .ok_or_else(|| limit("max_memory_bytes", "ODF package/image ceiling overflow"))?;
    if decoder_ceiling_peak > preflight {
        return Err(limit(
            "max_memory_bytes",
            format!(
                "ODF package/image decoder ceiling {decoder_ceiling_peak} > preflight {preflight}"
            ),
        ));
    }
    let mut decoder = ImageReader::with_format(Cursor::new(bytes), format)
        .into_decoder()
        .map_err(|_| malformed(Some(part), "image decoder rejected the header"))?;
    let (width, height) = decoder.dimensions();
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| limit("image_pixels", "image dimensions overflow"))?;
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || pixels > MAX_IMAGE_PIXELS
    {
        return Err(limit("image_pixels", format!("unsafe dimensions {width}x{height} in {part}")));
    }
    let mut limits = ImageLimits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    let decode_peak = image_decode_plan(size, pixels)?;
    let combined_peak = package_peak
        .checked_add(decode_peak)
        .ok_or_else(|| limit("max_memory_bytes", "ODF package/image peak overflow"))?;
    if combined_peak > preflight {
        return Err(limit(
            "max_memory_bytes",
            format!("ODF package/image working plan {combined_peak} > preflight {preflight}"),
        ));
    }
    limits.max_alloc = Some(decode_peak);
    decoder
        .set_limits(limits)
        .map_err(|_| limit("image_decode_memory", format!("decoder limits rejected {part}")))?;
    let total = usize::try_from(decoder.total_bytes())
        .map_err(|_| limit("image_decode_memory", "decoded image size cannot be represented"))?;
    let mut output = Vec::new();
    output.try_reserve_exact(total).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve decoded image: {error}"))
    })?;
    output.resize(total, 0);
    // The codec is given a strict 16 MP/explicit-allocation ceiling, so its only
    // non-interruptible slice is deterministically bounded. Check immediately around it.
    context.checkpoint()?;
    decoder
        .read_image(&mut output)
        .map_err(|_| malformed(Some(part), "image codec payload is not decodable"))?;
    context.checkpoint()?;
    Ok(())
}

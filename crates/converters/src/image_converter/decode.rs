//! Bounded codec entry after complete envelope validation.

use super::envelope::Summary;
use super::format::RasterFormat;
use image::codecs::webp::WebPDecoder;
use image::metadata::Orientation;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageReader, Rgba, RgbaImage};
use into_markdown_core::{ConversionError, ExecutionContext, ResourceLimits, ResourceReservation};
use std::io::{BufReader, Cursor};
use tiff::decoder::{Decoder as TiffDecoder, DecodingResult, Limits as TiffLimits};
use tiff::{ColorType as TiffColorType, tags::Tag};

#[cfg(test)]
std::thread_local! {
    static DECODER_ENTRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) struct DecodedSet {
    pub(crate) frames: Vec<DecodedFrame>,
    pub(super) color: String,
    pub(super) orientation: u8,
    _memory: ResourceReservation,
}

pub(crate) struct DecodedFrame {
    pub(crate) pixels: RgbaImage,
    pub(super) has_alpha: bool,
}

pub(crate) struct StaticHeader {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) color: String,
    pub(crate) orientation: u8,
    pub(crate) has_alpha_channel: bool,
}

/// Inspect a completely audited single-frame image without materializing its
/// pixel buffer. TIFF uses a separate directory audit and animated WebP needs
/// frame decoding, so callers only use this for the other static formats.
pub(crate) fn inspect_static(
    format: RasterFormat,
    bytes: &[u8],
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<StaticHeader, ConversionError> {
    let image_format =
        format.image_format().ok_or_else(|| malformed("static decoder format is unavailable"))?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), image_format);
    reader.limits(image_limits(limits));
    mark_decoder_entry();
    let mut decoder = reader.into_decoder().map_err(map_image_error)?;
    let (width, height) = decoder.dimensions();
    validate_dimensions(width, height, limits)?;
    let orientation = decoder.orientation().map_err(map_image_error)?.to_exif();
    let color = decoder.original_color_type();
    context.checkpoint()?;
    Ok(StaticHeader {
        width,
        height,
        color: format!("{color:?}"),
        orientation,
        has_alpha_channel: image::ColorType::try_from(color)
            .map_or(true, image::ColorType::has_alpha),
    })
}

pub(crate) fn decode(
    format: RasterFormat,
    bytes: &[u8],
    summary: Summary,
    source_frames: u32,
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<DecodedSet, ConversionError> {
    match (format, summary.animated) {
        (RasterFormat::Tiff, _) => decode_tiff(bytes, summary, source_frames, limits, context),
        (RasterFormat::WebP, true) => {
            decode_webp_animation(bytes, summary, source_frames, limits, context)
        }
        _ => decode_static(format, bytes, limits, context),
    }
}

fn decode_static(
    format: RasterFormat,
    bytes: &[u8],
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<DecodedSet, ConversionError> {
    let image_format =
        format.image_format().ok_or_else(|| malformed("static decoder format is unavailable"))?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), image_format);
    reader.limits(image_limits(limits));
    mark_decoder_entry();
    let mut decoder = reader.into_decoder().map_err(map_image_error)?;
    let (width, height) = decoder.dimensions();
    validate_dimensions(width, height, limits)?;
    let orientation = decoder.orientation().map_err(map_image_error)?;
    let allocation = working_bytes(width, height, 20)?;
    let memory = context.reserve_memory(allocation)?;
    context.checkpoint()?;
    let color = format!("{:?}", decoder.original_color_type());
    let mut image = DynamicImage::from_decoder(decoder).map_err(map_image_error)?;
    image.apply_orientation(orientation);
    let pixels = image.into_rgba8();
    context.checkpoint()?;
    let has_alpha = has_alpha(&pixels, context)?;
    Ok(DecodedSet {
        frames: vec![DecodedFrame { pixels, has_alpha }],
        color,
        orientation: orientation.to_exif(),
        _memory: memory,
    })
}

fn decode_webp_animation(
    bytes: &[u8],
    summary: Summary,
    _source_frames: u32,
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<DecodedSet, ConversionError> {
    let reader = BufReader::new(Cursor::new(bytes));
    mark_decoder_entry();
    let mut webp_decoder = WebPDecoder::new(reader).map_err(map_image_error)?;
    webp_decoder.set_limits(image_limits(limits)).map_err(map_image_error)?;
    let (width, height) = webp_decoder.dimensions();
    validate_dimensions(width, height, limits)?;
    let orientation = webp_decoder.orientation().map_err(map_image_error)?;
    let allocation = working_bytes(width, height, 12)?
        .checked_mul(u64::from(summary.frames))
        .ok_or_else(|| resource("max_memory_bytes", "animated WebP allocation overflow"))?;
    let total_pixel_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| bytes.checked_mul(u64::from(summary.frames)))
        .ok_or_else(|| resource("max_decompressed_bytes", "animated WebP pixel size overflow"))?;
    enforce_decompressed(total_pixel_bytes, limits)?;
    let memory = context.reserve_memory(allocation)?;
    let frame_stream = webp_decoder.into_frames();
    let mut frames = Vec::new();
    frames
        .try_reserve_exact(summary.frames as usize)
        .map_err(|_| resource("max_memory_bytes", "animated WebP frame allocation failed"))?;
    for (index, frame) in frame_stream.take(summary.frames as usize).enumerate() {
        context.checkpoint()?;
        if index >= summary.frames as usize {
            return Err(malformed("WebP decoder produced more frames than its envelope"));
        }
        let mut image = DynamicImage::ImageRgba8(frame.map_err(map_image_error)?.into_buffer());
        image.apply_orientation(orientation);
        let pixels = image.into_rgba8();
        let has_alpha = has_alpha(&pixels, context)?;
        frames.push(DecodedFrame { pixels, has_alpha });
    }
    if frames.len() != summary.frames as usize {
        return Err(malformed("WebP decoder produced fewer frames than the selected sequence"));
    }
    Ok(DecodedSet {
        frames,
        color: "Rgba8".into(),
        orientation: orientation.to_exif(),
        _memory: memory,
    })
}

fn decode_tiff(
    bytes: &[u8],
    summary: Summary,
    source_frames: u32,
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<DecodedSet, ConversionError> {
    let decoder_limits = tiff_limits(limits)?;
    mark_decoder_entry();
    let mut audit = TiffDecoder::new(Cursor::new(bytes))
        .map_err(map_tiff_error)?
        .with_limits(decoder_limits.clone());
    let mut allocation = 0_u64;
    let mut decoded_bytes = 0_u64;
    let mut color = None;
    for index in 0..summary.frames {
        context.checkpoint()?;
        let (width, height) = audit.dimensions().map_err(map_tiff_error)?;
        validate_dimensions(width, height, limits)?;
        let kind = audit.colortype().map_err(map_tiff_error)?;
        validate_tiff_color(kind)?;
        let page = working_bytes(width, height, tiff_working_multiplier(kind)?)?;
        allocation = allocation
            .checked_add(page)
            .ok_or_else(|| resource("max_memory_bytes", "TIFF allocation overflow"))?;
        decoded_bytes = decoded_bytes
            .checked_add(u64::from(width) * u64::from(height) * 4)
            .ok_or_else(|| resource("max_decompressed_bytes", "TIFF pixel total overflow"))?;
        color.get_or_insert_with(|| format!("{kind:?}"));
        if index + 1 < summary.frames {
            if !audit.more_images() {
                return Err(malformed("TIFF decoder found fewer directories than its envelope"));
            }
            audit.next_image().map_err(map_tiff_error)?;
        }
    }
    if summary.frames == source_frames && audit.more_images() {
        return Err(malformed("TIFF decoder found more directories than its envelope"));
    }
    enforce_decompressed(decoded_bytes, limits)?;
    let memory = context.reserve_memory(allocation)?;

    mark_decoder_entry();
    let mut decoder =
        TiffDecoder::new(Cursor::new(bytes)).map_err(map_tiff_error)?.with_limits(decoder_limits);
    let mut frames = Vec::new();
    frames
        .try_reserve_exact(summary.frames as usize)
        .map_err(|_| resource("max_memory_bytes", "TIFF frame allocation failed"))?;
    let mut first_orientation = 1_u8;
    for index in 0..summary.frames {
        context.checkpoint()?;
        let (width, height) = decoder.dimensions().map_err(map_tiff_error)?;
        let kind = decoder.colortype().map_err(map_tiff_error)?;
        let orientation = decoder
            .find_tag_unsigned::<u8>(Tag::Orientation)
            .map_err(map_tiff_error)?
            .and_then(Orientation::from_exif)
            .unwrap_or(Orientation::NoTransforms);
        if index == 0 {
            first_orientation = orientation.to_exif();
        }
        let result = decoder.read_image().map_err(map_tiff_error)?;
        let mut image = DynamicImage::ImageRgba8(to_rgba(result, kind, width, height, context)?);
        image.apply_orientation(orientation);
        let pixels = image.into_rgba8();
        let has_alpha = has_alpha(&pixels, context)?;
        frames.push(DecodedFrame { pixels, has_alpha });
        if index + 1 < summary.frames {
            decoder.next_image().map_err(map_tiff_error)?;
        }
    }
    if summary.frames == source_frames && decoder.more_images() {
        return Err(malformed("TIFF decoder was not exhausted"));
    }
    Ok(DecodedSet {
        frames,
        color: color.unwrap_or_else(|| "unknown".into()),
        orientation: first_orientation,
        _memory: memory,
    })
}

#[cfg(test)]
fn mark_decoder_entry() {
    DECODER_ENTRIES.with(|entries| entries.set(entries.get() + 1));
}

#[cfg(not(test))]
const fn mark_decoder_entry() {}

#[cfg(test)]
pub(super) fn reset_decoder_entries() {
    DECODER_ENTRIES.with(|entries| entries.set(0));
}

#[cfg(test)]
pub(super) fn decoder_entries() -> usize {
    DECODER_ENTRIES.with(std::cell::Cell::get)
}

fn to_rgba(
    result: DecodingResult,
    color: TiffColorType,
    width: u32,
    height: u32,
    context: &ExecutionContext,
) -> Result<RgbaImage, ConversionError> {
    let pixels = usize::try_from(u64::from(width) * u64::from(height))
        .map_err(|_| malformed("TIFF pixel count overflow"))?;
    let mut output = RgbaImage::new(width, height);
    match (result, color) {
        (DecodingResult::U8(values), TiffColorType::Gray(1)) => {
            let row_bytes = width.div_ceil(8) as usize;
            if values.len() != row_bytes.saturating_mul(height as usize) {
                return Err(malformed("TIFF bilevel sample count mismatch"));
            }
            for y in 0..height {
                if y % 64 == 0 {
                    context.checkpoint()?;
                }
                let row = usize::try_from(y)
                    .map_err(|_| malformed("TIFF row index is unrepresentable"))?;
                for x in 0..width {
                    let column = usize::try_from(x)
                        .map_err(|_| malformed("TIFF column index is unrepresentable"))?;
                    let value =
                        if values[row * row_bytes + column / 8] & (0x80 >> (column % 8)) == 0 {
                            0
                        } else {
                            255
                        };
                    output.put_pixel(x, y, Rgba([value, value, value, 255]));
                }
            }
        }
        (DecodingResult::U8(values), kind) => {
            fill_u8(&mut output, &values, kind, pixels, context)?;
        }
        (DecodingResult::U16(values), kind) => {
            let mut narrowed = Vec::new();
            narrowed
                .try_reserve_exact(values.len())
                .map_err(|_| resource("max_memory_bytes", "TIFF sample allocation failed"))?;
            for (index, value) in values.into_iter().enumerate() {
                if index % 4096 == 0 {
                    context.checkpoint()?;
                }
                narrowed.push(u8::try_from(value >> 8).unwrap_or(u8::MAX));
            }
            fill_u8(&mut output, &narrowed, kind, pixels, context)?;
        }
        _ => return Err(unsupported("TIFF sample representation is not supported")),
    }
    Ok(output)
}

fn fill_u8(
    output: &mut RgbaImage,
    values: &[u8],
    color: TiffColorType,
    pixels: usize,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let channels = match color {
        TiffColorType::Gray(8 | 16) => 1,
        TiffColorType::GrayA(8 | 16) => 2,
        TiffColorType::RGB(8 | 16) => 3,
        TiffColorType::RGBA(8 | 16) | TiffColorType::CMYK(8) => 4,
        _ => return Err(unsupported("TIFF color layout is not supported")),
    };
    if values.len() != pixels.saturating_mul(channels) {
        return Err(malformed("TIFF decoded sample count mismatch"));
    }
    for (index, (target, source)) in
        output.pixels_mut().zip(values.chunks_exact(channels)).enumerate()
    {
        if index % 1024 == 0 {
            context.checkpoint()?;
        }
        *target = match color {
            TiffColorType::Gray(_) => Rgba([source[0], source[0], source[0], 255]),
            TiffColorType::GrayA(_) => Rgba([source[0], source[0], source[0], source[1]]),
            TiffColorType::RGB(_) => Rgba([source[0], source[1], source[2], 255]),
            TiffColorType::RGBA(_) => Rgba([source[0], source[1], source[2], source[3]]),
            TiffColorType::CMYK(_) => {
                let k = u16::from(source[3]);
                let r = u8::try_from(255_u16.saturating_sub((u16::from(source[0]) + k).min(255)))
                    .unwrap_or(0);
                let g = u8::try_from(255_u16.saturating_sub((u16::from(source[1]) + k).min(255)))
                    .unwrap_or(0);
                let b = u8::try_from(255_u16.saturating_sub((u16::from(source[2]) + k).min(255)))
                    .unwrap_or(0);
                Rgba([r, g, b, 255])
            }
            _ => unreachable!(),
        };
    }
    Ok(())
}

fn has_alpha(pixels: &RgbaImage, context: &ExecutionContext) -> Result<bool, ConversionError> {
    for (index, pixel) in pixels.pixels().enumerate() {
        if index % 1024 == 0 {
            context.checkpoint()?;
        }
        if pixel.0[3] != u8::MAX {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_tiff_color(color: TiffColorType) -> Result<(), ConversionError> {
    match color {
        TiffColorType::Gray(1 | 8 | 16)
        | TiffColorType::GrayA(8 | 16)
        | TiffColorType::RGB(8 | 16)
        | TiffColorType::RGBA(8 | 16)
        | TiffColorType::CMYK(8) => Ok(()),
        _ => Err(unsupported(format!("TIFF color type {color:?} is not supported"))),
    }
}

fn tiff_working_multiplier(color: TiffColorType) -> Result<u64, ConversionError> {
    validate_tiff_color(color)?;
    Ok(match color {
        TiffColorType::RGBA(16) => 16,
        TiffColorType::RGB(16) => 14,
        TiffColorType::GrayA(16) => 12,
        _ => 10,
    })
}

fn image_limits(limits: &ResourceLimits) -> image::Limits {
    let mut image_limits = image::Limits::default();
    image_limits.max_alloc = Some(limits.max_decompressed_bytes.min(limits.max_memory_bytes));
    image_limits
}

fn tiff_limits(limits: &ResourceLimits) -> Result<TiffLimits, ConversionError> {
    let allocation = usize::try_from(limits.max_decompressed_bytes.min(limits.max_memory_bytes))
        .map_err(|_| resource("max_memory_bytes", "TIFF decoder limit is not representable"))?;
    let mut decoder_limits = TiffLimits::default();
    decoder_limits.decoding_buffer_size = allocation;
    // A valid TIFF may store the complete image in one strip. Binding this to
    // a smaller fixed constant rejects high-resolution exports even when the
    // caller explicitly grants enough decompression and request memory.
    decoder_limits.intermediate_buffer_size = allocation;
    decoder_limits.ifd_value_size = allocation.min(16 * 1024 * 1024);
    Ok(decoder_limits)
}

fn validate_dimensions(
    width: u32,
    height: u32,
    limits: &ResourceLimits,
) -> Result<(), ConversionError> {
    if width == 0 || height == 0 {
        return Err(malformed("image dimensions must be non-zero"));
    }
    let decoded = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| resource("max_decompressed_bytes", "decoded pixel size overflow"))?;
    enforce_decompressed(decoded, limits)
}

fn working_bytes(width: u32, height: u32, multiplier: u64) -> Result<u64, ConversionError> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(multiplier))
        .ok_or_else(|| resource("max_memory_bytes", "image working-set overflow"))
}

fn enforce_decompressed(bytes: u64, limits: &ResourceLimits) -> Result<(), ConversionError> {
    if bytes > limits.max_decompressed_bytes {
        return Err(resource(
            "max_decompressed_bytes",
            format!("image working set {bytes} > {}", limits.max_decompressed_bytes),
        ));
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn map_image_error(error: image::ImageError) -> ConversionError {
    match error {
        image::ImageError::Limits(_) => resource("max_decompressed_bytes", error.to_string()),
        image::ImageError::Unsupported(_) => unsupported(error.to_string()),
        _ => malformed(format!("image decoder rejected the audited envelope: {error}")),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn map_tiff_error(error: tiff::TiffError) -> ConversionError {
    match error {
        tiff::TiffError::LimitsExceeded => {
            resource("max_decompressed_bytes", "TIFF decoder allocation limit exceeded")
        }
        tiff::TiffError::UnsupportedError(_) => unsupported(error.to_string()),
        _ => malformed(format!("TIFF decoder rejected the audited envelope: {error}")),
    }
}

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some("image.decoder".into()), detail: detail.into() }
}

fn unsupported(detail: impl Into<String>) -> ConversionError {
    ConversionError::Unsupported { detail: detail.into() }
}

fn resource(limit: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: detail.into() }
}

#[cfg(test)]
mod resource_bound_tests {
    use super::*;

    #[test]
    fn tiff_segment_limit_tracks_the_request_budget_without_a_fixed_64_mib_cap() {
        let granted = 512 * 1024 * 1024;
        let limits = ResourceLimits {
            max_memory_bytes: granted,
            max_decompressed_bytes: granted,
            ..ResourceLimits::default()
        };
        let decoder = tiff_limits(&limits).unwrap();
        assert_eq!(decoder.decoding_buffer_size, granted as usize);
        assert_eq!(decoder.intermediate_buffer_size, granted as usize);
        assert_eq!(decoder.ifd_value_size, 16 * 1024 * 1024);
    }

    #[test]
    fn dimensions_are_bounded_by_decoded_bytes_instead_of_fixed_geometry() {
        let limits = ResourceLimits::default();
        assert!(validate_dimensions(40_000, 1, &limits).is_ok());
        assert!(validate_dimensions(20_000, 10_000, &limits).is_ok());

        let bounded = ResourceLimits { max_decompressed_bytes: 799_999_999, ..limits };
        assert!(matches!(
            validate_dimensions(20_000, 10_000, &bounded),
            Err(ConversionError::ResourceLimit { limit: "max_decompressed_bytes", .. })
        ));
        let decoder_limits = image_limits(&bounded);
        assert_eq!(decoder_limits.max_image_width, None);
        assert_eq!(decoder_limits.max_image_height, None);
    }
}

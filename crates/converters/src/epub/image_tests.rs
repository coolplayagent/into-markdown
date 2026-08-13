use super::image;
use ::image::{DynamicImage, Frame, ImageFormat, RgbaImage, codecs::gif::GifEncoder};
use into_markdown_core::{ExecutionContext, ExecutionOptions, ResourceLimits};
use std::io::Cursor;

fn context(limits: &ResourceLimits) -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), limits.clone())
}

fn encoded(format: ImageFormat) -> Vec<u8> {
    let mut bytes = Vec::new();
    DynamicImage::new_rgba8(1, 1).write_to(&mut Cursor::new(&mut bytes), format).unwrap();
    bytes
}

#[test]
fn every_retained_raster_must_exhaust_its_container_exactly() {
    for (media, format) in [
        ("image/png", ImageFormat::Png),
        ("image/jpeg", ImageFormat::Jpeg),
        ("image/gif", ImageFormat::Gif),
        ("image/webp", ImageFormat::WebP),
    ] {
        let bytes = encoded(format);
        let limits = ResourceLimits::default();
        image::validate(&bytes, media, "fixture", &limits, &context(&limits)).unwrap();

        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(image::validate(&trailing, media, "fixture", &limits, &context(&limits)).is_err());

        let terminator = match format {
            ImageFormat::Png => bytes[bytes.len() - 12..].to_vec(),
            ImageFormat::Jpeg => bytes[bytes.len() - 2..].to_vec(),
            ImageFormat::Gif => bytes[bytes.len() - 1..].to_vec(),
            ImageFormat::WebP => bytes[12..].to_vec(),
            _ => unreachable!(),
        };
        let mut repeated = bytes.clone();
        repeated.extend_from_slice(&terminator);
        if format == ImageFormat::WebP {
            let riff_size = u32::try_from(repeated.len() - 8).unwrap().to_le_bytes();
            repeated[4..8].copy_from_slice(&riff_size);
        }
        assert!(image::validate(&repeated, media, "fixture", &limits, &context(&limits)).is_err());

        let truncated = &bytes[..bytes.len() - 1];
        assert!(image::validate(truncated, media, "fixture", &limits, &context(&limits)).is_err());
    }
}

#[test]
fn gif_uses_the_cumulative_decoded_budget() {
    let mut bytes = Vec::new();
    {
        let mut encoder = GifEncoder::new(&mut bytes);
        encoder.encode_frame(Frame::new(RgbaImage::new(1, 1))).unwrap();
        encoder.encode_frame(Frame::new(RgbaImage::new(1, 1))).unwrap();
    }
    let limits = ResourceLimits { max_decompressed_bytes: 7, ..ResourceLimits::default() };
    assert!(image::validate(&bytes, "image/gif", "fixture", &limits, &context(&limits)).is_err());

    let mut repeated_terminator = encoded(ImageFormat::Gif);
    repeated_terminator.push(0x3b);
    let ordinary = ResourceLimits::default();
    assert!(
        image::validate(
            &repeated_terminator,
            "image/gif",
            "fixture",
            &ordinary,
            &context(&ordinary)
        )
        .is_err()
    );
}

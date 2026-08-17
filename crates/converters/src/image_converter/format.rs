//! Exact raster-family identification.

use into_markdown_core::{ConversionError, ExecutionContext};

/// Audited raster codecs accepted by the image converter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RasterFormat {
    Png,
    Jpeg,
    Tiff,
    WebP,
    Bmp,
}

impl RasterFormat {
    pub(crate) const fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Tiff => "image/tiff",
            Self::WebP => "image/webp",
            Self::Bmp => "image/bmp",
        }
    }

    pub(crate) const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Tiff => "tiff",
            Self::WebP => "webp",
            Self::Bmp => "bmp",
        }
    }

    pub(super) const fn image_format(self) -> Option<image::ImageFormat> {
        match self {
            Self::Png => Some(image::ImageFormat::Png),
            Self::Jpeg => Some(image::ImageFormat::Jpeg),
            Self::WebP => Some(image::ImageFormat::WebP),
            Self::Bmp => Some(image::ImageFormat::Bmp),
            Self::Tiff => None,
        }
    }
}

pub(crate) fn detect(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Option<RasterFormat>, ConversionError> {
    context.checkpoint()?;
    Ok(if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(RasterFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(RasterFormat::Jpeg)
    } else if matches!(bytes.get(..4), Some(b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+")) {
        Some(RasterFormat::Tiff)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(RasterFormat::WebP)
    } else if bytes.starts_with(b"BM") {
        Some(RasterFormat::Bmp)
    } else {
        None
    })
}

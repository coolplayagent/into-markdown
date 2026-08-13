//! Bounded picture collection and raster validation.

use super::budget::{hex, limit, locator, malformed, reserve_vec};
use super::parser::{CHECKPOINT_INTERVAL, Parser};
use image::{
    ImageDecoder as _, Limits as ImageLimits,
    codecs::{jpeg::JpegDecoder, png::PngDecoder},
};
use into_markdown_core::{
    Asset, AssetId, Block, ConversionError, ConversionOptions, DiagnosticSeverity,
    ExecutionContext, ResourceReservation,
};
use std::io::Cursor;

impl Parser<'_> {
    pub(super) fn picture_hex(
        &mut self,
        bytes: &[u8],
        start: usize,
    ) -> Result<(), ConversionError> {
        let Some(mut picture) = self.picture.take() else {
            return Err(ConversionError::Internal {
                detail: "pict destination lacks state".into(),
            });
        };
        for (index, byte) in bytes.iter().copied().enumerate() {
            if index.is_multiple_of(CHECKPOINT_INTERVAL) {
                self.context.checkpoint()?;
            }
            if byte.is_ascii_whitespace() {
                continue;
            }
            let nibble = hex(byte).ok_or_else(|| {
                malformed(format!("invalid pict hexadecimal byte at {}", start + index))
            })?;
            if let Some(high) = picture.high_nibble.take() {
                let next = picture
                    .bytes
                    .len()
                    .checked_add(1)
                    .ok_or_else(|| limit("max_asset_bytes", "picture byte count overflow"))?;
                if u64::try_from(next).unwrap_or(u64::MAX) > self.options.limits.max_asset_bytes {
                    return Err(limit(
                        "max_asset_bytes",
                        format!("{next} > {}", self.options.limits.max_asset_bytes),
                    ));
                }
                reserve_vec(&mut picture.bytes, 1, &mut self.memory)?;
                picture.bytes.push((high << 4) | nibble);
            } else {
                picture.high_nibble = Some(nibble);
            }
        }
        picture.saw_odd_nibble = picture.high_nibble.is_some();
        self.picture = Some(picture);
        Ok(())
    }

    pub(super) fn picture_binary(&mut self, count: usize) -> Result<(), ConversionError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| limit("max_asset_bytes", "bin range overflow"))?;
        let bytes =
            self.bytes.get(self.offset..end).ok_or_else(|| malformed("truncated pict bin data"))?;
        let Some(mut picture) = self.picture.take() else {
            return Err(ConversionError::Internal { detail: "pict bin lacks state".into() });
        };
        if picture.high_nibble.is_some() {
            return Err(malformed("pict bin data follows an incomplete hexadecimal byte"));
        }
        let next = picture
            .bytes
            .len()
            .checked_add(count)
            .ok_or_else(|| limit("max_asset_bytes", "picture byte count overflow"))?;
        if u64::try_from(next).unwrap_or(u64::MAX) > self.options.limits.max_asset_bytes {
            return Err(limit(
                "max_asset_bytes",
                format!("{next} > {}", self.options.limits.max_asset_bytes),
            ));
        }
        reserve_vec(&mut picture.bytes, count, &mut self.memory)?;
        picture.bytes.extend_from_slice(bytes);
        self.picture = Some(picture);
        self.offset = end;
        Ok(())
    }

    pub(super) fn finish_picture(&mut self, end: usize) -> Result<(), ConversionError> {
        let Some(picture) = self.picture.take() else {
            return Err(ConversionError::Internal { detail: "pict close lacks state".into() });
        };
        if picture.saw_odd_nibble {
            return Err(malformed("pict hexadecimal data has an odd number of nibbles"));
        }
        let Some(media_type) = picture.media_type else {
            return self.add_diagnostic(
                "rtf.unsupportedVectorImage",
                DiagnosticSeverity::Warning,
                "EMF/WMF or untyped pict content was not retained",
                Some(locator(picture.start, end)),
            );
        };
        audit_image(&picture.bytes, media_type, self.options, self.context, &mut self.memory)?;
        let size = u64::try_from(picture.bytes.len()).unwrap_or(u64::MAX);
        self.total_asset_bytes = self
            .total_asset_bytes
            .checked_add(size)
            .ok_or_else(|| limit("max_total_asset_bytes", "asset total overflow"))?;
        if self.total_asset_bytes > self.options.limits.max_total_asset_bytes {
            return Err(limit(
                "max_total_asset_bytes",
                format!(
                    "{} > {}",
                    self.total_asset_bytes, self.options.limits.max_total_asset_bytes
                ),
            ));
        }
        // Prepay bounded IDs, filename, and MIME before their formatting allocates.
        self.memory.grow(256)?;
        let id = format!("rtf-image-{}", self.assets.len() + 1);
        let filename = format!("{id}.{}", if media_type == "image/png" { "png" } else { "jpg" });
        reserve_vec(&mut self.assets, 1, &mut self.memory)?;
        self.assets.push(Asset {
            id: AssetId(id.clone()),
            filename: Some(filename),
            media_type: media_type.into(),
            bytes: picture.bytes,
            external_uri: None,
        });
        let node = self.node(Block::Image { asset: AssetId(id), alt: None }, picture.start, end)?;
        self.push_block(node)
    }
}

pub(super) fn audit_image(
    bytes: &[u8],
    media_type: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    const MAX_DIMENSION: u32 = 32_768;
    context.checkpoint()?;
    let compressed = u64::try_from(bytes.len())
        .map_err(|_| limit("max_asset_bytes", "picture size cannot be represented"))?;
    let (dimensions, decoded_bytes) = if media_type == "image/png" {
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(malformed("pict bytes do not match the declared PNG signature"));
        }
        let mut decoder = PngDecoder::new(Cursor::new(bytes))
            .map_err(|_| malformed("PNG pict header is invalid"))?;
        let dimensions = decoder.dimensions();
        set_image_limits(
            &mut decoder,
            dimensions,
            image_working_bound(dimensions, compressed)?,
            options,
        )?;
        (dimensions, decoder.total_bytes())
    } else {
        if !bytes.starts_with(&[0xff, 0xd8, 0xff]) || !bytes.ends_with(&[0xff, 0xd9]) {
            return Err(malformed("pict bytes do not match the declared JPEG signature"));
        }
        let mut decoder = JpegDecoder::new(Cursor::new(bytes))
            .map_err(|_| malformed("JPEG pict header is invalid"))?;
        let dimensions = decoder.dimensions();
        set_image_limits(
            &mut decoder,
            dimensions,
            image_working_bound(dimensions, compressed)?,
            options,
        )?;
        (dimensions, decoder.total_bytes())
    };
    if dimensions.0 == 0
        || dimensions.1 == 0
        || dimensions.0 > MAX_DIMENSION
        || dimensions.1 > MAX_DIMENSION
    {
        return Err(limit(
            "image_dimensions",
            format!("{}x{} exceeds the audited image bounds", dimensions.0, dimensions.1),
        ));
    }
    if decoded_bytes > options.limits.max_decompressed_bytes {
        return Err(limit(
            "max_decompressed_bytes",
            format!("decoded picture {decoded_bytes} > {}", options.limits.max_decompressed_bytes),
        ));
    }
    // Decoder internals are bounded through ImageLimits. Reserve the decoded output plus a
    // conservative compressed-input copy and 256 KiB codec state before decoding.
    let working = decoded_bytes
        .checked_add(compressed)
        .and_then(|value| value.checked_add(256 * 1024))
        .ok_or_else(|| limit("max_memory_bytes", "picture audit working set overflow"))?;
    memory.grow(working)?;
    let length = usize::try_from(decoded_bytes)
        .map_err(|_| limit("max_decompressed_bytes", "decoded picture is too large"))?;
    let mut pixels = Vec::new();
    reserve_vec(&mut pixels, length, memory)?;
    pixels.resize(length, 0);
    if media_type == "image/png" {
        let mut decoder = PngDecoder::new(Cursor::new(bytes))
            .map_err(|_| malformed("PNG pict header is invalid"))?;
        set_image_limits(&mut decoder, dimensions, working, options)?;
        decoder.read_image(&mut pixels).map_err(|_| malformed("PNG pict stream is invalid"))?;
    } else {
        let mut decoder = JpegDecoder::new(Cursor::new(bytes))
            .map_err(|_| malformed("JPEG pict header is invalid"))?;
        set_image_limits(&mut decoder, dimensions, working, options)?;
        decoder.read_image(&mut pixels).map_err(|_| malformed("JPEG pict stream is invalid"))?;
    }
    context.checkpoint()
}

pub(super) fn image_working_bound(
    dimensions: (u32, u32),
    compressed: u64,
) -> Result<u64, ConversionError> {
    u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|pixels| pixels.checked_add(compressed))
        .and_then(|value| value.checked_add(256 * 1024))
        .ok_or_else(|| limit("image_decode_memory", "picture working set overflow"))
}

pub(super) fn set_image_limits<D: image::ImageDecoder>(
    decoder: &mut D,
    dimensions: (u32, u32),
    max_alloc: u64,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let mut limits = ImageLimits::default();
    limits.max_image_width = Some(dimensions.0.min(32_768));
    limits.max_image_height = Some(dimensions.1.min(32_768));
    limits.max_alloc = Some(max_alloc.min(options.limits.max_memory_bytes));
    decoder
        .set_limits(limits)
        .map_err(|_| limit("image_decode_memory", "image decoder rejected resource limits"))
}

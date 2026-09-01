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
        self.context.checkpoint()?;
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
        self.context.checkpoint()
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
        if self.table.active || self.state().in_table {
            reserve_vec(&mut self.table.cell_blocks, 1, &mut self.memory)?;
            self.table.cell_blocks.push(node);
            Ok(())
        } else {
            self.push_block(node)
        }
    }
}

pub(super) fn audit_image(
    bytes: &[u8],
    media_type: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    context.checkpoint()?;
    validate_exact_image_envelope(bytes, media_type, context)?;
    let compressed = u64::try_from(bytes.len())
        .map_err(|_| limit("max_asset_bytes", "picture size cannot be represented"))?;
    let (dimensions, decoded_bytes) = if media_type == "image/png" {
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
    if dimensions.0 == 0 || dimensions.1 == 0 {
        return Err(limit("image_dimensions", "picture dimensions must be non-zero"));
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

fn validate_exact_image_envelope(
    bytes: &[u8],
    media_type: &str,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    if media_type == "image/png" {
        exact_png_envelope(bytes, context)
    } else if media_type == "image/jpeg" {
        exact_jpeg_envelope(bytes, context)
    } else {
        Err(malformed("pict media type is not an audited raster format"))
    }
}

type EnvelopeCheckpointHook<'a> = Option<&'a dyn Fn(usize)>;

const ENVELOPE_CHECKPOINT_BYTES: usize = 4 * 1024;

struct EnvelopeMeter<'a> {
    context: &'a ExecutionContext,
    bytes_until_checkpoint: usize,
    checkpoints: usize,
    hook: EnvelopeCheckpointHook<'a>,
}

impl<'a> EnvelopeMeter<'a> {
    fn new(context: &'a ExecutionContext, hook: EnvelopeCheckpointHook<'a>) -> Self {
        Self { context, bytes_until_checkpoint: ENVELOPE_CHECKPOINT_BYTES, checkpoints: 0, hook }
    }

    fn consume(&mut self, mut bytes: usize) -> Result<(), ConversionError> {
        while bytes >= self.bytes_until_checkpoint {
            bytes -= self.bytes_until_checkpoint;
            self.bytes_until_checkpoint = ENVELOPE_CHECKPOINT_BYTES;
            self.checkpoints = self.checkpoints.checked_add(1).ok_or_else(|| {
                limit("max_asset_bytes", "image envelope checkpoint count overflowed")
            })?;
            if let Some(hook) = self.hook {
                hook(self.checkpoints);
            }
            self.context.checkpoint()?;
        }
        self.bytes_until_checkpoint -= bytes;
        Ok(())
    }

    fn next_batch_bytes(&self) -> usize {
        self.bytes_until_checkpoint
    }
}

fn exact_png_envelope(bytes: &[u8], context: &ExecutionContext) -> Result<(), ConversionError> {
    exact_png_envelope_inner(bytes, context, None)
}

fn exact_png_envelope_inner(
    bytes: &[u8],
    context: &ExecutionContext,
    hook: EnvelopeCheckpointHook<'_>,
) -> Result<(), ConversionError> {
    context.checkpoint()?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(malformed("pict bytes do not match the declared PNG signature"));
    }
    let mut meter = EnvelopeMeter::new(context, hook);
    meter.consume(8)?;
    let mut cursor = 8_usize;
    loop {
        let header = bytes
            .get(cursor..cursor.saturating_add(8))
            .ok_or_else(|| malformed("PNG chunk header is truncated"))?;
        let length = usize::try_from(u32::from_be_bytes(
            header[..4].try_into().map_err(|_| malformed("PNG chunk length is truncated"))?,
        ))
        .map_err(|_| limit("max_asset_bytes", "PNG chunk length cannot be represented"))?;
        let chunk_type = &header[4..8];
        if !chunk_type.iter().all(u8::is_ascii_alphabetic) {
            return Err(malformed("PNG chunk type is invalid"));
        }
        let data_start = cursor
            .checked_add(8)
            .ok_or_else(|| limit("max_asset_bytes", "PNG chunk offset overflowed"))?;
        let data_end = data_start
            .checked_add(length)
            .ok_or_else(|| limit("max_asset_bytes", "PNG chunk length overflowed"))?;
        let end = data_end
            .checked_add(4)
            .ok_or_else(|| limit("max_asset_bytes", "PNG chunk CRC offset overflowed"))?;
        let data = bytes
            .get(data_start..data_end)
            .ok_or_else(|| malformed("PNG chunk data is truncated"))?;
        let stored_crc =
            bytes.get(data_end..end).ok_or_else(|| malformed("PNG chunk CRC is truncated"))?;
        let stored_crc = u32::from_be_bytes(
            stored_crc.try_into().map_err(|_| malformed("PNG chunk CRC is truncated"))?,
        );
        meter.consume(4)?;
        if png_crc(chunk_type, data, &mut meter)? != stored_crc {
            return Err(malformed("PNG chunk CRC mismatch"));
        }
        meter.consume(4)?;
        if chunk_type == b"IEND" {
            if length != 0 || end != bytes.len() {
                return Err(malformed("PNG IEND must be unique and end exactly at EOF"));
            }
            return Ok(());
        }
        cursor = end;
    }
}

fn png_crc(
    chunk_type: &[u8],
    data: &[u8],
    meter: &mut EnvelopeMeter<'_>,
) -> Result<u32, ConversionError> {
    let mut crc = u32::MAX;
    crc = png_crc_metered(crc, chunk_type, meter)?;
    crc = png_crc_metered(crc, data, meter)?;
    Ok(!crc)
}

fn png_crc_metered(
    mut crc: u32,
    bytes: &[u8],
    meter: &mut EnvelopeMeter<'_>,
) -> Result<u32, ConversionError> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let batch_length = remaining.len().min(meter.next_batch_bytes());
        let (batch, rest) = remaining.split_at(batch_length);
        crc = png_crc_update(crc, batch);
        meter.consume(batch.len())?;
        remaining = rest;
    }
    Ok(crc)
}

fn png_crc_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 { crc >> 1 } else { (crc >> 1) ^ 0xedb8_8320 };
        }
    }
    crc
}

fn exact_jpeg_envelope(bytes: &[u8], context: &ExecutionContext) -> Result<(), ConversionError> {
    exact_jpeg_envelope_inner(bytes, context, None)
}

fn exact_jpeg_envelope_inner(
    bytes: &[u8],
    context: &ExecutionContext,
    hook: EnvelopeCheckpointHook<'_>,
) -> Result<(), ConversionError> {
    context.checkpoint()?;
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err(malformed("pict bytes do not match the declared JPEG signature"));
    }
    let mut meter = EnvelopeMeter::new(context, hook);
    meter.consume(2)?;
    let mut cursor = 2_usize;
    let mut pending_marker = None;
    loop {
        let marker = if let Some(marker) = pending_marker.take() {
            marker
        } else {
            read_jpeg_marker(bytes, &mut cursor, &mut meter)?
        };
        match marker {
            0xd9 => {
                if cursor != bytes.len() {
                    return Err(malformed("JPEG EOI must be the first real terminator and EOF"));
                }
                return Ok(());
            }
            0xd8 | 0x00 => return Err(malformed("JPEG contains an invalid structural marker")),
            0x01 | 0xd0..=0xd7 => {}
            0xda => {
                let end = jpeg_segment_end(bytes, cursor)?;
                meter.consume(end - cursor)?;
                cursor = end;
                pending_marker = Some(read_jpeg_entropy_marker(bytes, &mut cursor, &mut meter)?);
            }
            _ => {
                let end = jpeg_segment_end(bytes, cursor)?;
                meter.consume(end - cursor)?;
                cursor = end;
            }
        }
    }
}

fn read_jpeg_marker(
    bytes: &[u8],
    cursor: &mut usize,
    meter: &mut EnvelopeMeter<'_>,
) -> Result<u8, ConversionError> {
    if bytes.get(*cursor) != Some(&0xff) {
        return Err(malformed("JPEG data appears outside a marker or entropy scan"));
    }
    while bytes.get(*cursor) == Some(&0xff) {
        let batch_start = *cursor;
        let batch_end = batch_start.saturating_add(meter.next_batch_bytes());
        while *cursor < batch_end && bytes.get(*cursor) == Some(&0xff) {
            *cursor = cursor
                .checked_add(1)
                .ok_or_else(|| limit("max_asset_bytes", "JPEG marker offset overflowed"))?;
        }
        meter.consume(*cursor - batch_start)?;
    }
    let marker = *bytes.get(*cursor).ok_or_else(|| malformed("JPEG marker is truncated"))?;
    *cursor = cursor
        .checked_add(1)
        .ok_or_else(|| limit("max_asset_bytes", "JPEG marker offset overflowed"))?;
    meter.consume(1)?;
    Ok(marker)
}

fn read_jpeg_entropy_marker(
    bytes: &[u8],
    cursor: &mut usize,
    meter: &mut EnvelopeMeter<'_>,
) -> Result<u8, ConversionError> {
    loop {
        let run_start = *cursor;
        let batch_end = run_start.saturating_add(meter.next_batch_bytes());
        while *cursor < batch_end && bytes.get(*cursor).is_some_and(|byte| *byte != 0xff) {
            *cursor = cursor
                .checked_add(1)
                .ok_or_else(|| limit("max_asset_bytes", "JPEG scan offset overflowed"))?;
        }
        meter.consume(*cursor - run_start)?;
        if *cursor < bytes.len() && bytes.get(*cursor) != Some(&0xff) {
            continue;
        }
        if *cursor == bytes.len() {
            return Err(malformed("JPEG entropy scan has no EOI"));
        }
        let marker = read_jpeg_marker(bytes, cursor, meter)?;
        if !matches!(marker, 0x00 | 0xd0..=0xd7) {
            return Ok(marker);
        }
    }
}

fn jpeg_segment_end(bytes: &[u8], length_offset: usize) -> Result<usize, ConversionError> {
    let raw = bytes
        .get(length_offset..length_offset.saturating_add(2))
        .ok_or_else(|| malformed("JPEG segment length is truncated"))?;
    let length = usize::from(u16::from_be_bytes([raw[0], raw[1]]));
    if length < 2 {
        return Err(malformed("JPEG segment length is smaller than its header"));
    }
    let end = length_offset
        .checked_add(length)
        .ok_or_else(|| limit("max_asset_bytes", "JPEG segment length overflowed"))?;
    if end > bytes.len() {
        return Err(malformed("JPEG segment exceeds source bytes"));
    }
    Ok(end)
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
    // Bind the decoder to the dimensions authenticated from its own header without imposing an
    // independent product ceiling. Allocation and decompression budgets remain authoritative.
    limits.max_image_width = Some(dimensions.0);
    limits.max_image_height = Some(dimensions.1);
    limits.max_alloc = Some(max_alloc.min(options.limits.max_memory_bytes));
    decoder
        .set_limits(limits)
        .map_err(|_| limit("image_decode_memory", "image decoder rejected resource limits"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ExtendedColorType, codecs::jpeg::JpegEncoder};
    use into_markdown_core::{
        CancellationToken, ConversionOptions, ErrorCode, ExecutionContext, ExecutionOptions,
    };
    use std::time::Duration;

    #[test]
    fn png_envelope_requires_one_real_iend_at_eof() {
        let context = context();
        let png = tiny_png();
        exact_png_envelope(&png, &context).unwrap();

        let mut trailing = png.clone();
        trailing.push(0);
        assert_malformed(&exact_png_envelope(&trailing, &context).unwrap_err());

        let mut repeated = png.clone();
        repeated.extend_from_slice(&png[png.len() - 12..]);
        assert_malformed(&exact_png_envelope(&repeated, &context).unwrap_err());

        let mut fake_only = png[..png.len() - 12].to_vec();
        append_png_chunk(&mut fake_only, *b"tEXt", b"fake IEND marker");
        assert_malformed(&exact_png_envelope(&fake_only, &context).unwrap_err());
    }

    #[test]
    fn jpeg_envelope_skips_fake_eoi_in_segments_and_requires_real_eoi_at_eof() {
        let context = context();
        let jpeg = tiny_jpeg();
        exact_jpeg_envelope(&jpeg, &context).unwrap();

        let mut trailing = jpeg.clone();
        trailing.push(0);
        assert_malformed(&exact_jpeg_envelope(&trailing, &context).unwrap_err());

        let mut repeated = jpeg.clone();
        repeated.extend_from_slice(&[0xff, 0xd9]);
        assert_malformed(&exact_jpeg_envelope(&repeated, &context).unwrap_err());

        let mut fake_then_real = vec![0xff, 0xd8, 0xff, 0xe1, 0x00, 0x04, 0xff, 0xd9];
        fake_then_real.extend_from_slice(&jpeg[2..]);
        exact_jpeg_envelope(&fake_then_real, &context).unwrap();

        let fake_only = [0xff, 0xd8, 0xff, 0xe1, 0x00, 0x04, 0xff, 0xd9];
        assert_malformed(&exact_jpeg_envelope(&fake_only, &context).unwrap_err());

        let missing_real_eoi = &jpeg[..jpeg.len() - 2];
        assert_malformed(&exact_jpeg_envelope(missing_real_eoi, &context).unwrap_err());
    }

    #[test]
    fn exact_envelopes_still_receive_complete_pixel_decode_audit() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        for (media_type, bytes) in [("image/png", tiny_png()), ("image/jpeg", tiny_jpeg())] {
            let mut memory = context.reserve_memory(0).unwrap();
            audit_image(&bytes, media_type, &options, &context, &mut memory).unwrap();
        }
    }

    #[test]
    fn large_png_chunk_crc_observes_mid_scan_cancellation_and_releases_lease() {
        let cancellation = CancellationToken::new();
        let context = controlled_context(ExecutionOptions {
            cancellation: cancellation.clone(),
            ..ExecutionOptions::default()
        });
        let mut png = tiny_png();
        png.truncate(png.len() - 12);
        append_png_chunk(&mut png, *b"ruSt", &vec![0x5a; ENVELOPE_CHECKPOINT_BYTES * 3 + 17]);
        append_png_chunk(&mut png, *b"IEND", &[]);
        let hook = |checkpoint| {
            if checkpoint == 2 {
                cancellation.cancel();
            }
        };
        let lease = context.reserve_memory(128).unwrap();
        let error = exact_png_envelope_inner(&png, &context, Some(&hook)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled, "{error}");
        assert_eq!(context.reserved_memory_bytes(), 128);
        drop(lease);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn non_aligned_jpeg_segment_jumps_observe_cumulative_cancellation() {
        let cancellation = CancellationToken::new();
        let context = controlled_context(ExecutionOptions {
            cancellation: cancellation.clone(),
            ..ExecutionOptions::default()
        });
        let mut jpeg = vec![0xff, 0xd8];
        for _ in 0..48 {
            jpeg.extend_from_slice(&[0xff, 0xe1, 0x01, 0x03]);
            jpeg.extend_from_slice(&[0x41; 257]);
        }
        jpeg.extend_from_slice(&[0xff, 0xd9]);
        let hook = |checkpoint| {
            if checkpoint == 2 {
                cancellation.cancel();
            }
        };
        let lease = context.reserve_memory(96).unwrap();
        let error = exact_jpeg_envelope_inner(&jpeg, &context, Some(&hook)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled, "{error}");
        assert_eq!(context.reserved_memory_bytes(), 96);
        drop(lease);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn long_jpeg_ff_fill_observes_mid_scan_timeout_and_releases_lease() {
        let context = controlled_context(ExecutionOptions {
            timeout: Some(Duration::from_millis(250)),
            ..ExecutionOptions::default()
        });
        let mut jpeg = vec![0xff, 0xd8];
        jpeg.extend(std::iter::repeat_n(0xff, ENVELOPE_CHECKPOINT_BYTES * 3 + 19));
        jpeg.push(0xd9);
        let hook = |checkpoint| {
            if checkpoint == 1 {
                std::thread::sleep(Duration::from_millis(300));
            }
        };
        let lease = context.reserve_memory(64).unwrap();
        let error = exact_jpeg_envelope_inner(&jpeg, &context, Some(&hook)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Timeout, "{error}");
        assert_eq!(context.reserved_memory_bytes(), 64);
        drop(lease);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    fn context() -> ExecutionContext {
        controlled_context(ExecutionOptions::default())
    }

    fn controlled_context(execution: ExecutionOptions) -> ExecutionContext {
        let options = ConversionOptions::default();
        ExecutionContext::new(execution, options.limits)
    }

    fn tiny_png() -> Vec<u8> {
        let hex = b"89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c6360606060000000050001a5f645400000000049454e44ae426082";
        hex.chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn tiny_jpeg() -> Vec<u8> {
        let mut output = Vec::new();
        JpegEncoder::new(&mut output).encode(&[0, 0, 0], 1, 1, ExtendedColorType::Rgb8).unwrap();
        output
    }

    fn append_png_chunk(output: &mut Vec<u8>, chunk_type: [u8; 4], data: &[u8]) {
        output.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        output.extend_from_slice(&chunk_type);
        output.extend_from_slice(data);
        let crc = !png_crc_update(png_crc_update(u32::MAX, &chunk_type), data);
        output.extend_from_slice(&crc.to_be_bytes());
    }

    fn assert_malformed(error: &ConversionError) {
        assert_eq!(error.code(), ErrorCode::Malformed, "{error}");
    }
}

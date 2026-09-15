use super::{
    Bitmap, ConversionError, ConversionOptions, Digest, ExecutionContext, ImageBitmap, PixelFormat,
    ResourceReservation, Sha256, malformed, map_pdfium_error, resource,
};

/// Publish lossless compressed pixels instead of retaining a raw bitmap per page.
pub(super) fn image_bitmap_to_png(
    bitmap: &ImageBitmap,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Vec<u8>, ResourceReservation), ConversionError> {
    let channels = match bitmap.format {
        PixelFormat::Gray => 1_u32,
        PixelFormat::Bgr => 3,
        PixelFormat::Bgrx | PixelFormat::Bgra => 4,
    };
    let row_bytes = bitmap
        .width
        .checked_mul(channels)
        .ok_or_else(|| malformed("image", "pixel row size overflow"))?;
    let source_bytes = u64::from(bitmap.stride) * u64::from(bitmap.height);
    if bitmap.stride < row_bytes || source_bytes != bitmap.bytes.len() as u64 {
        return Err(malformed("image", "bitmap stride or payload length is invalid"));
    }
    let raw = u64::from(bitmap.width)
        .checked_mul(u64::from(bitmap.height))
        .and_then(|v| v.checked_mul(4))
        .ok_or_else(|| resource("max_memory_bytes", "pixel size overflow"))?;
    let _pixels_memory = context.reserve_memory(raw)?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(
            usize::try_from(raw)
                .map_err(|_| resource("max_memory_bytes", "pixel size is not representable"))?,
        )
        .map_err(|_| resource("max_memory_bytes", "pixel allocation failed"))?;
    for row in 0..bitmap.height {
        context.checkpoint()?;
        let start = (u64::from(row) * u64::from(bitmap.stride)) as usize;
        for pixel in bitmap.bytes[start..start + row_bytes as usize].chunks_exact(channels as usize)
        {
            let rgba = match bitmap.format {
                PixelFormat::Gray => [pixel[0], pixel[0], pixel[0], 255],
                PixelFormat::Bgr | PixelFormat::Bgrx => [pixel[2], pixel[1], pixel[0], 255],
                PixelFormat::Bgra => [pixel[2], pixel[1], pixel[0], pixel[3]],
            };
            pixels.extend_from_slice(&rgba);
        }
    }
    let pixels = image::RgbaImage::from_raw(bitmap.width, bitmap.height, pixels)
        .ok_or_else(|| malformed("image", "invalid pixel dimensions"))?;
    Ok(crate::image_converter::encode::png(&pixels, false, &options.limits, context)?.into_parts())
}

#[cfg(test)]
pub(super) fn image_bitmap_to_bmp(
    bitmap: &ImageBitmap,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Vec<u8>, ResourceReservation), ConversionError> {
    encode_bmp(
        bitmap.width,
        bitmap.height,
        bitmap.stride,
        bitmap.format,
        &bitmap.bytes,
        options,
        context,
    )
}

pub(super) fn rendered_bitmap_to_bmp(
    bitmap: &Bitmap,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Vec<u8>, ResourceReservation), ConversionError> {
    encode_bmp(
        bitmap.width,
        bitmap.height,
        bitmap.stride,
        PixelFormat::Bgra,
        &bitmap.bytes,
        options,
        context,
    )
}

#[allow(clippy::too_many_lines)]
pub(super) fn encode_bmp(
    width: u32,
    height: u32,
    source_stride: u32,
    format: PixelFormat,
    source: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Vec<u8>, ResourceReservation), ConversionError> {
    context.checkpoint()?;
    let source_size = u64::from(source_stride)
        .checked_mul(u64::from(height))
        .ok_or_else(|| resource("max_asset_bytes", "source bitmap size overflow"))?;
    if source_size != u64::try_from(source.len()).unwrap_or(u64::MAX) {
        return Err(malformed("image", "bitmap byte length does not match stride and height"));
    }
    let stride =
        width.checked_mul(4).ok_or_else(|| resource("max_asset_bytes", "BMP stride overflow"))?;
    let pixel_bytes = u64::from(stride)
        .checked_mul(u64::from(height))
        .ok_or_else(|| resource("max_asset_bytes", "BMP pixel size overflow"))?;
    let size = 54_u64
        .checked_add(pixel_bytes)
        .ok_or_else(|| resource("max_asset_bytes", "BMP size overflow"))?;
    if size > options.limits.max_asset_bytes {
        return Err(resource(
            "max_asset_bytes",
            format!("{size} > {}", options.limits.max_asset_bytes),
        ));
    }
    let reservation = context.reserve_memory(size)?;
    let capacity = usize::try_from(size)
        .map_err(|_| resource("max_asset_bytes", "BMP size does not fit usize"))?;
    let mut output =
        into_markdown_pdfium::fixed_zeroed_bytes(capacity).map_err(map_pdfium_error)?;
    let mut cursor = 0_usize;
    {
        let mut write = |bytes: &[u8]| -> Result<(), ConversionError> {
            let end = cursor
                .checked_add(bytes.len())
                .ok_or_else(|| malformed("image", "BMP write offset overflow"))?;
            output
                .get_mut(cursor..end)
                .ok_or_else(|| malformed("image", "BMP write exceeds planned buffer"))?
                .copy_from_slice(bytes);
            cursor = end;
            Ok(())
        };
        write(b"BM")?;
        write(
            &u32::try_from(size)
                .map_err(|_| resource("max_asset_bytes", "BMP exceeds 32-bit format"))?
                .to_le_bytes(),
        )?;
        write(&[0; 4])?;
        write(&54_u32.to_le_bytes())?;
        write(&40_u32.to_le_bytes())?;
        write(
            &i32::try_from(width)
                .map_err(|_| resource("max_asset_bytes", "BMP width exceeds format"))?
                .to_le_bytes(),
        )?;
        write(
            &i32::try_from(height)
                .map_err(|_| resource("max_asset_bytes", "BMP height exceeds format"))?
                .checked_neg()
                .ok_or_else(|| resource("max_asset_bytes", "BMP height cannot be negated"))?
                .to_le_bytes(),
        )?;
        write(&1_u16.to_le_bytes())?;
        write(&32_u16.to_le_bytes())?;
        write(&0_u32.to_le_bytes())?;
        write(
            &u32::try_from(pixel_bytes)
                .map_err(|_| resource("max_asset_bytes", "BMP pixels exceed 32-bit format"))?
                .to_le_bytes(),
        )?;
        write(&[0; 16])?;
        let bytes_per_pixel = match format {
            PixelFormat::Gray => 1,
            PixelFormat::Bgr => 3,
            PixelFormat::Bgrx | PixelFormat::Bgra => 4,
        };
        let minimum_source_stride = width
            .checked_mul(u32::try_from(bytes_per_pixel).unwrap_or(u32::MAX))
            .ok_or_else(|| malformed("image", "minimum source stride overflow"))?;
        if source_stride < minimum_source_stride {
            return Err(malformed("image", "bitmap stride is shorter than one pixel row"));
        }
        for row in 0..height {
            let row_start = usize::try_from(u64::from(row) * u64::from(source_stride))
                .map_err(|_| malformed("image", "row offset overflow"))?;
            for column in 0..width {
                let offset = row_start
                    .checked_add(
                        usize::try_from(column)
                            .unwrap_or(usize::MAX)
                            .checked_mul(bytes_per_pixel)
                            .ok_or_else(|| malformed("image", "pixel offset overflow"))?,
                    )
                    .ok_or_else(|| malformed("image", "pixel offset overflow"))?;
                let end = offset
                    .checked_add(bytes_per_pixel)
                    .ok_or_else(|| malformed("image", "pixel end overflow"))?;
                let pixel = source
                    .get(offset..end)
                    .ok_or_else(|| malformed("image", "pixel lies outside bitmap bytes"))?;
                match format {
                    PixelFormat::Gray => {
                        let gray = pixel[0];
                        write(&[gray, gray, gray, 255])?;
                    }
                    PixelFormat::Bgr | PixelFormat::Bgrx => {
                        write(&[pixel[0], pixel[1], pixel[2], 255])?;
                    }
                    PixelFormat::Bgra => write(pixel)?,
                }
            }
        }
    }
    if cursor != capacity {
        return Err(malformed("image", "BMP encoder length mismatch"));
    }
    Ok((output, reservation))
}

pub(super) fn content_asset_id(prefix: &str, bytes: &[u8]) -> Result<String, ConversionError> {
    use std::fmt::Write as _;

    let mut id = String::new();
    id.try_reserve_exact(prefix.len().saturating_add(65))
        .map_err(|_| resource("max_memory_bytes", "asset ID allocation failed"))?;
    id.push_str(prefix);
    id.push('-');
    for byte in Sha256::digest(bytes) {
        write!(&mut id, "{byte:02x}")
            .map_err(|_| resource("max_memory_bytes", "asset ID formatting failed"))?;
    }
    Ok(id)
}

pub(super) fn account_asset(
    bytes: &[u8],
    total: &mut u64,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let size = u64::try_from(bytes.len())
        .map_err(|_| resource("max_asset_bytes", "asset size does not fit u64"))?;
    *total = total
        .checked_add(size)
        .ok_or_else(|| resource("max_total_asset_bytes", "asset total overflow"))?;
    if *total > options.limits.max_total_asset_bytes {
        return Err(resource(
            "max_total_asset_bytes",
            format!("{} > {}", *total, options.limits.max_total_asset_bytes),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod compressed_tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits};

    #[test]
    fn published_scan_pages_keep_pixels_and_release_raw_working_memory() {
        let options = ConversionOptions {
            limits: ResourceLimits {
                max_memory_bytes: 4 * 1024 * 1024,
                max_asset_bytes: 32 * 1024,
                ..ResourceLimits::default()
            },
            ..ConversionOptions::default()
        };
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let bitmap = ImageBitmap {
            width: 256,
            height: 256,
            stride: 1024,
            format: PixelFormat::Bgra,
            bytes: [3, 7, 11, 127].repeat(256 * 256),
        };
        let mut pages = Vec::new();
        for _ in 0..64 {
            pages.push(image_bitmap_to_png(&bitmap, &options, &context).unwrap());
        }
        assert!(context.reserved_memory_bytes() < 1024 * 1024);
        let decoded = image::load_from_memory(&pages[0].0).unwrap().to_rgba8();
        assert!(decoded.pixels().all(|p| p.0 == [11, 7, 3, 127]));
        let mut limited = options.clone();
        limited.limits.max_asset_bytes = 1;
        assert!(image_bitmap_to_png(&bitmap, &limited, &context).is_err());
        drop(pages);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn compressed_bitmap_honors_stride_and_channel_order() {
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        for (format, bytes, expected) in [
            (PixelFormat::Gray, vec![42, 99], [42, 42, 42, 255]),
            (PixelFormat::Bgr, vec![3, 7, 11, 99], [11, 7, 3, 255]),
            (PixelFormat::Bgrx, vec![3, 7, 11, 0, 99], [11, 7, 3, 255]),
        ] {
            let bitmap =
                ImageBitmap { width: 1, height: 1, stride: bytes.len() as u32, format, bytes };
            let (bytes, _lease) =
                image_bitmap_to_png(&bitmap, &ConversionOptions::default(), &context).unwrap();
            assert_eq!(
                image::load_from_memory(&bytes).unwrap().to_rgba8().get_pixel(0, 0).0,
                expected
            );
        }
    }
}

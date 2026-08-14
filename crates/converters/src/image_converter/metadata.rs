//! Numeric-only density metadata extraction; profiles and free-form payloads stay opaque.

use super::envelope::meter::Meter;
use super::format::RasterFormat;
use into_markdown_core::{ConversionError, ExecutionContext};

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Density {
    pub(super) x_dpi: Option<f64>,
    pub(super) y_dpi: Option<f64>,
}

pub(super) fn density(
    format: RasterFormat,
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Density, ConversionError> {
    context.checkpoint()?;
    let density = match format {
        RasterFormat::Png => png(bytes, context)?,
        RasterFormat::Jpeg => jpeg(bytes, context)?,
        RasterFormat::Bmp => bmp(bytes),
        RasterFormat::Tiff => tiff(bytes, context)?,
        RasterFormat::WebP => Density::default(),
    };
    context.checkpoint()?;
    Ok(density)
}

fn png(bytes: &[u8], context: &ExecutionContext) -> Result<Density, ConversionError> {
    let mut offset = 8_usize;
    let mut meter = Meter::new(context);
    meter.consume(8)?;
    while offset.checked_add(12).is_some_and(|end| end <= bytes.len()) {
        context.checkpoint()?;
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let end = match offset.checked_add(12).and_then(|value| value.checked_add(length)) {
            Some(end) if end <= bytes.len() => end,
            _ => break,
        };
        if &bytes[offset + 4..offset + 8] == b"pHYs" && length == 9 {
            let data = &bytes[offset + 8..offset + 17];
            if data[8] == 1 {
                return Ok(Density {
                    x_dpi: dpi_from_ppm(u32::from_be_bytes(data[..4].try_into().unwrap())),
                    y_dpi: dpi_from_ppm(u32::from_be_bytes(data[4..8].try_into().unwrap())),
                });
            }
        }
        meter.consume(end - offset)?;
        offset = end;
    }
    Ok(Density::default())
}

fn jpeg(bytes: &[u8], context: &ExecutionContext) -> Result<Density, ConversionError> {
    let mut offset = 2_usize;
    let mut meter = Meter::new(context);
    meter.consume(2)?;
    while offset + 4 <= bytes.len() {
        context.checkpoint()?;
        while bytes.get(offset) == Some(&0xff) {
            let start = offset;
            let end = start.saturating_add(meter.next_batch());
            while offset < end && bytes.get(offset) == Some(&0xff) {
                offset += 1;
            }
            meter.consume(offset - start)?;
        }
        let Some(&marker) = bytes.get(offset) else { break };
        offset += 1;
        meter.consume(1)?;
        if marker == 0xda || marker == 0xd9 {
            break;
        }
        if matches!(marker, 0x01 | 0xd0..=0xd7) {
            continue;
        }
        let Some(length_bytes) = bytes.get(offset..offset + 2) else { break };
        let length = u16::from_be_bytes(length_bytes.try_into().unwrap()) as usize;
        if length < 2 || offset + length > bytes.len() {
            break;
        }
        let data = &bytes[offset + 2..offset + length];
        if marker == 0xe0 && data.len() >= 12 && data.starts_with(b"JFIF\0") {
            let unit = data[7];
            let x = u16::from_be_bytes([data[8], data[9]]);
            let y = u16::from_be_bytes([data[10], data[11]]);
            return Ok(match unit {
                1 => Density { x_dpi: nonzero(x), y_dpi: nonzero(y) },
                2 => Density {
                    x_dpi: nonzero(x).map(|value| value * 2.54),
                    y_dpi: nonzero(y).map(|value| value * 2.54),
                },
                _ => Density::default(),
            });
        }
        offset += length;
        meter.consume(length)?;
    }
    Ok(Density::default())
}

fn bmp(bytes: &[u8]) -> Density {
    if bytes.len() < 46 {
        return Density::default();
    }
    let dib = u32::from_le_bytes(bytes[14..18].try_into().unwrap());
    if dib < 40 {
        return Density::default();
    }
    let x = i32::from_le_bytes(bytes[38..42].try_into().unwrap());
    let y = i32::from_le_bytes(bytes[42..46].try_into().unwrap());
    Density {
        x_dpi: u32::try_from(x).ok().and_then(dpi_from_ppm),
        y_dpi: u32::try_from(y).ok().and_then(dpi_from_ppm),
    }
}

fn tiff(bytes: &[u8], context: &ExecutionContext) -> Result<Density, ConversionError> {
    context.checkpoint()?;
    let little = match bytes.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return Err(tiff_malformed("invalid byte order")),
    };
    let big = match read_u16(bytes, 2, little) {
        Some(42) => false,
        Some(43) => true,
        _ => return Err(tiff_malformed("invalid magic")),
    };
    let directory =
        if big { read_u64(bytes, 8, little) } else { read_u32(bytes, 4, little).map(u64::from) }
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| tiff_malformed("invalid first directory offset"))?;
    let (count, entries, entry_size) = if big {
        (
            read_u64(bytes, directory, little)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| tiff_malformed("invalid BigTIFF entry count"))?,
            directory.checked_add(8),
            20_usize,
        )
    } else {
        (
            usize::from(
                read_u16(bytes, directory, little)
                    .ok_or_else(|| tiff_malformed("invalid TIFF entry count"))?,
            ),
            directory.checked_add(2),
            12_usize,
        )
    };
    let entries = entries.ok_or_else(|| tiff_malformed("directory offset overflow"))?;
    let mut unit = None;
    let mut x = None;
    let mut y = None;
    for index in 0..count {
        if index % 256 == 0 {
            context.checkpoint()?;
        }
        let entry = index
            .checked_mul(entry_size)
            .and_then(|relative| entries.checked_add(relative))
            .ok_or_else(|| tiff_malformed("entry offset overflow"))?;
        let tag = read_u16(bytes, entry, little).ok_or_else(|| tiff_malformed("truncated tag"))?;
        match tag {
            282 => set_once(&mut x, rational(bytes, entry, little, big)?, "XResolution")?,
            283 => set_once(&mut y, rational(bytes, entry, little, big)?, "YResolution")?,
            296 => {
                set_once(&mut unit, resolution_unit(bytes, entry, little, big)?, "ResolutionUnit")?;
            }
            _ => {}
        }
    }
    let unit = unit.unwrap_or(2);
    match unit {
        2 => Ok(Density { x_dpi: x.flatten(), y_dpi: y.flatten() }),
        3 => Ok(Density {
            x_dpi: x.flatten().map(|value| value * 2.54),
            y_dpi: y.flatten().map(|value| value * 2.54),
        }),
        _ => Ok(Density::default()),
    }
}

fn rational(
    bytes: &[u8],
    entry: usize,
    little: bool,
    big: bool,
) -> Result<Option<f64>, ConversionError> {
    let field_type = read_u16(bytes, entry + 2, little);
    let count = if big {
        read_u64(bytes, entry + 4, little)
    } else {
        read_u32(bytes, entry + 4, little).map(u64::from)
    };
    if field_type != Some(5) || count != Some(1) {
        return Err(tiff_malformed("resolution fields require exactly one unsigned rational"));
    }
    let value = entry + if big { 12 } else { 8 };
    let payload = if big {
        value
    } else {
        read_u32(bytes, value, little)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| tiff_malformed("invalid resolution offset"))?
    };
    let numerator = read_u32(bytes, payload, little)
        .ok_or_else(|| tiff_malformed("truncated resolution numerator"))?;
    let denominator = read_u32(bytes, payload + 4, little)
        .ok_or_else(|| tiff_malformed("truncated resolution denominator"))?;
    if numerator == 0 || denominator == 0 {
        Ok(None)
    } else {
        Ok(Some(f64::from(numerator) / f64::from(denominator)))
    }
}

fn resolution_unit(
    bytes: &[u8],
    entry: usize,
    little: bool,
    big: bool,
) -> Result<u16, ConversionError> {
    let field_type = read_u16(bytes, entry + 2, little);
    let count = if big {
        read_u64(bytes, entry + 4, little)
    } else {
        read_u32(bytes, entry + 4, little).map(u64::from)
    };
    if field_type != Some(3) || count != Some(1) {
        return Err(tiff_malformed("resolution unit requires exactly one SHORT"));
    }
    read_u16(bytes, entry + if big { 12 } else { 8 }, little)
        .ok_or_else(|| tiff_malformed("truncated resolution unit"))
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> Result<(), ConversionError> {
    if slot.replace(value).is_some() {
        return Err(tiff_malformed(format!("duplicate {name} field")));
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize, little: bool) -> Option<u16> {
    let raw: [u8; 2] = bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
    Some(if little { u16::from_le_bytes(raw) } else { u16::from_be_bytes(raw) })
}

fn read_u32(bytes: &[u8], offset: usize, little: bool) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(if little { u32::from_le_bytes(raw) } else { u32::from_be_bytes(raw) })
}

fn read_u64(bytes: &[u8], offset: usize, little: bool) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?;
    Some(if little { u64::from_le_bytes(raw) } else { u64::from_be_bytes(raw) })
}

fn tiff_malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some("image.tiff.metadata".into()), detail: detail.into() }
}

fn dpi_from_ppm(value: u32) -> Option<f64> {
    (value != 0).then(|| f64::from(value) * 0.0254)
}

fn nonzero(value: u16) -> Option<f64> {
    (value != 0).then(|| f64::from(value))
}

//! Numeric-only density metadata extraction; profiles and free-form payloads stay opaque.

use super::envelope::meter::Meter;
use super::format::RasterFormat;
use into_markdown_core::{ConversionError, ExecutionContext};
use std::io::Cursor;
use tiff::decoder::Decoder as TiffDecoder;
use tiff::decoder::ifd::Value;
use tiff::tags::Tag;

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
        RasterFormat::Tiff => tiff(bytes),
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

fn tiff(bytes: &[u8]) -> Density {
    let Ok(mut decoder) = TiffDecoder::new(Cursor::new(bytes)) else {
        return Density::default();
    };
    let unit = decoder.find_tag_unsigned::<u16>(Tag::ResolutionUnit).ok().flatten().unwrap_or(2);
    let x = decoder.get_tag(Tag::XResolution).ok().as_ref().and_then(tiff_number);
    let y = decoder.get_tag(Tag::YResolution).ok().as_ref().and_then(tiff_number);
    match unit {
        2 => Density { x_dpi: x, y_dpi: y },
        3 => Density { x_dpi: x.map(|value| value * 2.54), y_dpi: y.map(|value| value * 2.54) },
        _ => Density::default(),
    }
}

fn tiff_number(value: &Value) -> Option<f64> {
    let value = match value {
        Value::Rational(numerator, denominator) if *denominator != 0 => {
            f64::from(*numerator) / f64::from(*denominator)
        }
        Value::Float(value) => f64::from(*value),
        Value::Double(value) => *value,
        Value::Byte(value) => f64::from(*value),
        Value::Short(value) => f64::from(*value),
        Value::Unsigned(value) => f64::from(*value),
        _ => return None,
    };
    (value.is_finite() && value > 0.0).then_some(value)
}

fn dpi_from_ppm(value: u32) -> Option<f64> {
    (value != 0).then(|| f64::from(value) * 0.0254)
}

fn nonzero(value: u16) -> Option<f64> {
    (value != 0).then(|| f64::from(value))
}

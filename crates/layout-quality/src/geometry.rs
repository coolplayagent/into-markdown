use crate::{NormalizedBounds, SourceBoundary};
use into_markdown_core::{ConversionError, Rect, SourceLocator};

pub(crate) fn boundary(locator: &SourceLocator) -> SourceBoundary {
    SourceBoundary {
        page: locator.page,
        slide: locator.slide,
        sheet: locator.sheet.clone(),
        cell: locator.cell.as_ref().map(|cell| format!("{},{}", cell.row, cell.column)),
        part: locator.part.clone(),
        byte_start: locator.byte_start,
        byte_end: locator.byte_end,
    }
}

pub(crate) fn normalize(bounds: Option<Rect>) -> Result<Option<NormalizedBounds>, ConversionError> {
    bounds.map(normalize_rect).transpose()
}

fn normalize_rect(bounds: Rect) -> Result<NormalizedBounds, ConversionError> {
    if !bounds.x.is_finite()
        || !bounds.y.is_finite()
        || !bounds.width.is_finite()
        || !bounds.height.is_finite()
        || bounds.width < 0.0
        || bounds.height < 0.0
    {
        return Err(ConversionError::Malformed {
            part: None,
            detail: "semantic layout quality received non-finite or negative source geometry"
                .into(),
        });
    }
    Ok(NormalizedBounds {
        x_milli: quantize(bounds.x)?,
        y_milli: quantize(bounds.y)?,
        width_milli: quantize(bounds.width)?,
        height_milli: quantize(bounds.height)?,
    })
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "finite range is checked before the intentional thousandth-unit quantization"
)]
fn quantize(value: f32) -> Result<i64, ConversionError> {
    let scaled = f64::from(value) * 1000.0;
    if scaled < i64::MIN as f64 || scaled > i64::MAX as f64 {
        return Err(ConversionError::ResourceLimit {
            limit: "source_geometry",
            detail: "source coordinate cannot be represented in normalized geometry".into(),
        });
    }
    Ok(scaled.round() as i64)
}

pub(crate) fn within_tolerance(
    expected: Option<NormalizedBounds>,
    actual: Option<NormalizedBounds>,
    tolerance: u32,
) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            let tolerance = u64::from(tolerance);
            absolute_delta(expected.x_milli, actual.x_milli) <= tolerance
                && absolute_delta(expected.y_milli, actual.y_milli) <= tolerance
                && absolute_delta(expected.width_milli, actual.width_milli) <= tolerance
                && absolute_delta(expected.height_milli, actual.height_milli) <= tolerance
        }
        _ => false,
    }
}

const fn absolute_delta(left: i64, right: i64) -> u64 {
    left.abs_diff(right)
}

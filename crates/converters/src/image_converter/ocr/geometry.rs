//! Admit only image-local evidence that satisfies the published IR geometry.

use super::{MaterializeContext, dimension};
use into_markdown_core::{ConversionError, Rect, SourcePoint};

#[cfg(test)]
#[path = "geometry_tests.rs"]
mod tests;

pub(super) fn validate_region_bounds(
    polygon: &[(f32, f32); 4],
    materialize: &MaterializeContext<'_>,
) -> Result<(), ConversionError> {
    let width = dimension(materialize.width, materialize.engine_id)?;
    let height = dimension(materialize.height, materialize.engine_id)?;
    if polygon.iter().any(|(x, y)| {
        !x.is_finite() || !y.is_finite() || *x < 0.0 || *y < 0.0 || *x > width || *y > height
    }) {
        return Err(ConversionError::Ocr {
            provider: materialize.engine_id.into(),
            detail: "OCR polygon lies outside the normalized image".into(),
        });
    }
    Ok(())
}

pub(super) fn validate_region_shape(
    polygon: &[(f32, f32); 4],
    provider: &str,
) -> Result<(), ConversionError> {
    if strictly_convex(polygon) {
        Ok(())
    } else {
        Err(ConversionError::Ocr {
            provider: provider.into(),
            detail: "OCR polygon must be a non-degenerate convex quadrilateral".into(),
        })
    }
}

fn strictly_convex(polygon: &[(f32, f32); 4]) -> bool {
    let mut sign = 0_i8;
    let mut twice_area = 0.0_f64;
    for index in 0..4 {
        let a = polygon[index];
        let b = polygon[(index + 1) % 4];
        let c = polygon[(index + 2) % 4];
        twice_area += f64::from(a.0) * f64::from(b.1) - f64::from(a.1) * f64::from(b.0);
        // Match IR validation's f32 differences and widened cross products.
        let cross = f64::from(b.0 - a.0) * f64::from(c.1 - b.1)
            - f64::from(b.1 - a.1) * f64::from(c.0 - b.0);
        if cross.abs() <= f64::EPSILON {
            return false;
        }
        let current = if cross.is_sign_positive() { 1 } else { -1 };
        if sign != 0 && sign != current {
            return false;
        }
        sign = current;
    }
    twice_area.is_finite() && twice_area.abs() > f64::EPSILON
}

pub(super) fn polygon_bounds(points: &[SourcePoint; 4]) -> Rect {
    let min_x = points.iter().map(|point| point.x).fold(f32::INFINITY, f32::min);
    let max_x = points.iter().map(|point| point.x).fold(f32::NEG_INFINITY, f32::max);
    let min_y = points.iter().map(|point| point.y).fold(f32::INFINITY, f32::min);
    let max_y = points.iter().map(|point| point.y).fold(f32::NEG_INFINITY, f32::max);
    Rect { x: min_x, y: min_y, width: max_x - min_x, height: max_y - min_y }
}

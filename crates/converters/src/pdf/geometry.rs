use super::{
    ConversionError, LinkTarget, MAX_PAGE_RENDER_DIMENSION, PageInfo, PdfRect, Rect, SourceLocator,
    malformed,
};

pub(super) fn page_locator(page: u32, info: &PageInfo) -> SourceLocator {
    let (width, height) = displayed_dimensions(info);
    SourceLocator {
        page: Some(page),
        page_width: Some(width),
        page_height: Some(height),
        rotation_degrees: Some(f32::from(info.rotation_degrees)),
        ..SourceLocator::default()
    }
}

pub(super) fn displayed_dimensions(info: &PageInfo) -> (f32, f32) {
    (info.width_points, info.height_points)
}

#[allow(clippy::cast_possible_truncation)]
pub(super) fn normalize_rect(rect: PdfRect, info: &PageInfo) -> Result<Rect, ConversionError> {
    let points = [
        normalize_point(rect.left, rect.bottom, info)?,
        normalize_point(rect.left, rect.top, info)?,
        normalize_point(rect.right, rect.bottom, info)?,
        normalize_point(rect.right, rect.top, info)?,
    ];
    let min_x = points.iter().map(|point| point.0).fold(f64::INFINITY, f64::min);
    let max_x = points.iter().map(|point| point.0).fold(f64::NEG_INFINITY, f64::max);
    let min_y = points.iter().map(|point| point.1).fold(f64::INFINITY, f64::min);
    let max_y = points.iter().map(|point| point.1).fold(f64::NEG_INFINITY, f64::max);
    let values = [min_x, min_y, max_x, max_y, max_x - min_x, max_y - min_y];
    if values.iter().any(|value| !value.is_finite() || value.abs() > f64::from(f32::MAX)) {
        return Err(malformed("geometry", "normalized rectangle is not representable"));
    }
    Ok(Rect {
        x: min_x as f32,
        y: min_y as f32,
        width: (max_x - min_x) as f32,
        height: (max_y - min_y) as f32,
    })
}

pub(super) fn normalize_point(
    x: f32,
    y: f32,
    info: &PageInfo,
) -> Result<(f64, f64), ConversionError> {
    let (raw_width, raw_height) = if matches!(info.rotation_degrees, 90 | 270) {
        (f64::from(info.height_points), f64::from(info.width_points))
    } else {
        (f64::from(info.width_points), f64::from(info.height_points))
    };
    let (x, y) = (f64::from(x), f64::from(y));
    let point = match info.rotation_degrees {
        0 => (x, raw_height - y),
        90 => (y, x),
        180 => (raw_width - x, y),
        270 => (raw_height - y, raw_width - x),
        _ => unreachable!("PDFium boundary validates page rotation"),
    };
    if !point.0.is_finite() || !point.1.is_finite() {
        return Err(malformed("geometry", "normalized point is not finite"));
    }
    Ok(point)
}

pub(super) fn safe_link_target(
    target: LinkTarget,
    page_count: u32,
) -> Result<String, ConversionError> {
    match target {
        LinkTarget::InternalPage { page_index } => {
            if page_index >= page_count {
                return Err(malformed("link", "internal destination is outside the document"));
            }
            let page = page_index
                .checked_add(1)
                .ok_or_else(|| malformed("link", "internal destination overflow"))?;
            Ok(format!("#pdf-page-{page}"))
        }
        LinkTarget::ExternalUri(value) => {
            if value.contains('\0') || value.chars().any(char::is_control) {
                return Err(malformed("link", "URI contains a NUL or control character"));
            }
            let parsed = url::Url::parse(&value)
                .map_err(|_| malformed("link", "external URI is not absolute"))?;
            if !matches!(parsed.scheme(), "http" | "https" | "mailto") {
                return Err(malformed("link", "external URI scheme is not permitted"));
            }
            Ok(value)
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn render_dimensions(info: &PageInfo) -> Result<(u32, u32), ConversionError> {
    let (width, height) = displayed_dimensions(info);
    let scale = (f64::from(MAX_PAGE_RENDER_DIMENSION) / f64::from(width.max(height))).min(2.0);
    let width = (f64::from(width) * scale).ceil();
    let height = (f64::from(height) * scale).ceil();
    if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
        return Err(malformed("page", "invalid render dimensions"));
    }
    Ok((width as u32, height as u32))
}

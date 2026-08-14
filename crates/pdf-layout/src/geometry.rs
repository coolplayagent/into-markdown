use into_markdown_core::Rect;

pub(crate) fn union(left: Rect, right: Rect) -> Rect {
    let x = left.x.min(right.x);
    let y = left.y.min(right.y);
    let right_edge = (left.x + left.width).max(right.x + right.width);
    let bottom = (left.y + left.height).max(right.y + right.height);
    Rect { x, y, width: right_edge - x, height: bottom - y }
}

pub(crate) fn intersection_area(left: Rect, right: Rect) -> f32 {
    let width = (left.x + left.width).min(right.x + right.width) - left.x.max(right.x);
    let height = (left.y + left.height).min(right.y + right.height) - left.y.max(right.y);
    width.max(0.0) * height.max(0.0)
}

pub(crate) fn overlap_ratio(left: Rect, right: Rect) -> f32 {
    let intersection = intersection_area(left, right);
    let smaller = (left.width * left.height).min(right.width * right.height);
    if smaller <= 0.0 { 0.0 } else { intersection / smaller }
}

pub(crate) fn major_start(rect: Rect, orientation: u16) -> f32 {
    match orientation {
        90 => rect.y,
        180 => -(rect.x + rect.width),
        270 => -(rect.y + rect.height),
        _ => rect.x,
    }
}

pub(crate) fn major_end(rect: Rect, orientation: u16) -> f32 {
    major_start(rect, orientation) + major_extent(rect, orientation)
}

pub(crate) fn major_extent(rect: Rect, orientation: u16) -> f32 {
    if matches!(orientation, 90 | 270) { rect.height } else { rect.width }
}

pub(crate) fn minor_center(rect: Rect, orientation: u16) -> f32 {
    match orientation {
        90 => -(rect.x + rect.width / 2.0),
        180 => -(rect.y + rect.height / 2.0),
        270 => rect.x + rect.width / 2.0,
        _ => rect.y + rect.height / 2.0,
    }
}

pub(crate) fn minor_extent(rect: Rect, orientation: u16) -> f32 {
    if matches!(orientation, 90 | 270) { rect.width } else { rect.height }
}

pub(crate) fn reading_cmp(
    left: (u16, Rect, usize),
    right: (u16, Rect, usize),
) -> std::cmp::Ordering {
    left.0
        .cmp(&right.0)
        .then_with(|| minor_center(left.1, left.0).total_cmp(&minor_center(right.1, right.0)))
        .then_with(|| major_start(left.1, left.0).total_cmp(&major_start(right.1, right.0)))
        .then_with(|| left.2.cmp(&right.2))
}

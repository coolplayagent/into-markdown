use into_markdown_core::{Rect, SourcePoint};

const EPSILON: f64 = 1.0e-6;

#[derive(Debug, Clone, Copy)]
pub(crate) struct RegionGeometry {
    pub(crate) polygon: [SourcePoint; 4],
    pub(crate) bounds: Rect,
    pub(crate) center: SourcePoint,
    pub(crate) direction: SourcePoint,
    pub(crate) thickness: f32,
    pub(crate) angle_degrees: f32,
}

impl RegionGeometry {
    pub(crate) fn from_polygon(points: [(f32, f32); 4]) -> Option<Self> {
        let polygon = points.map(|(x, y)| SourcePoint { x, y });
        if !valid_polygon(&polygon) {
            return None;
        }
        let top = midpoint_vector(polygon[0], polygon[1], polygon[3], polygon[2]);
        let side = midpoint_vector(polygon[0], polygon[3], polygon[1], polygon[2]);
        let top_length = norm(top);
        let side_length = norm(side);
        let (mut direction, length, thickness) = if top_length >= side_length {
            (top, top_length, side_length)
        } else {
            (side, side_length, top_length)
        };
        if length <= f32::EPSILON || thickness <= f32::EPSILON {
            return None;
        }
        direction.x /= length;
        direction.y /= length;
        // A sign-normalized axis makes ordering deterministic without guessing
        // the writing language or changing the raw polygon.
        if direction.x < 0.0 || (direction.x == 0.0 && direction.y < 0.0) {
            direction.x = -direction.x;
            direction.y = -direction.y;
        }
        let center = SourcePoint {
            x: polygon.iter().map(|point| point.x).sum::<f32>() / 4.0,
            y: polygon.iter().map(|point| point.y).sum::<f32>() / 4.0,
        };
        let bounds = polygon_bounds(&polygon);
        let angle_degrees = direction.y.atan2(direction.x).to_degrees().rem_euclid(180.0);
        Some(Self { polygon, bounds, center, direction, thickness, angle_degrees })
    }

    pub(crate) fn projection(&self, axis: SourcePoint) -> (f32, f32) {
        project_polygon(&self.polygon, axis)
    }
}

pub(crate) fn valid_polygon(polygon: &[SourcePoint; 4]) -> bool {
    if polygon.iter().any(|point| !point.x.is_finite() || !point.y.is_finite()) {
        return false;
    }
    let mut sign = 0_i8;
    let mut area = 0.0_f64;
    for index in 0..4 {
        let a = polygon[index];
        let b = polygon[(index + 1) % 4];
        let c = polygon[(index + 2) % 4];
        area += f64::from(a.x) * f64::from(b.y) - f64::from(a.y) * f64::from(b.x);
        let value = cross(a, b, c);
        if value.abs() <= EPSILON {
            return false;
        }
        let current = if value.is_sign_positive() { 1 } else { -1 };
        if sign != 0 && sign != current {
            return false;
        }
        sign = current;
    }
    area.is_finite() && area.abs() > EPSILON
}

pub(crate) fn line_compatible(left: &RegionGeometry, right: &RegionGeometry) -> bool {
    let dot = (left.direction.x * right.direction.x + left.direction.y * right.direction.y)
        .abs()
        .clamp(0.0, 1.0);
    if dot.acos().to_degrees() > 15.0 {
        return false;
    }
    let axis = normalized(SourcePoint {
        x: left.direction.x + signed_axis(right.direction, left.direction).x,
        y: left.direction.y + signed_axis(right.direction, left.direction).y,
    })
    .unwrap_or(left.direction);
    let normal = SourcePoint { x: -axis.y, y: axis.x };
    let (left_cross_start, left_cross_end) = left.projection(normal);
    let (right_cross_start, right_cross_end) = right.projection(normal);
    let cross_overlap =
        interval_overlap(left_cross_start, left_cross_end, right_cross_start, right_cross_end);
    let minimum_thickness = left.thickness.min(right.thickness).max(f32::EPSILON);
    let cross_centers =
        ((left_cross_start + left_cross_end) - (right_cross_start + right_cross_end)).abs() / 2.0;
    if cross_overlap / minimum_thickness < 0.30
        && cross_centers > 0.65 * left.thickness.max(right.thickness)
    {
        return false;
    }
    let (left_start, left_end) = left.projection(axis);
    let (right_start, right_end) = right.projection(axis);
    interval_gap(left_start, left_end, right_start, right_end)
        <= 2.5 * left.thickness.max(right.thickness)
}

pub(crate) fn paragraph_compatible(left: &Rect, right: &Rect, line_height: f32) -> bool {
    let vertical_gap = interval_gap(left.y, left.y + left.height, right.y, right.y + right.height);
    let horizontal_overlap =
        interval_overlap(left.x, left.x + left.width, right.x, right.x + right.width);
    let minimum_width = left.width.min(right.width).max(1.0);
    vertical_gap <= 1.8 * line_height.max(1.0)
        && (horizontal_overlap / minimum_width >= 0.15
            || (left.x - right.x).abs() <= 1.5 * line_height.max(1.0))
}

pub(crate) fn polygon_overlap_ratio(left: &[SourcePoint; 4], right: &[SourcePoint; 4]) -> f64 {
    let intersection = convex_intersection_area(left, right);
    if intersection <= 0.0 {
        return 0.0;
    }
    let minimum = polygon_area(left).min(polygon_area(right));
    if minimum <= EPSILON { 0.0 } else { (intersection / minimum).clamp(0.0, 1.0) }
}

pub(crate) fn rect_overlap_ratio(left: Rect, right: Rect) -> f64 {
    let width =
        f64::from((left.x + left.width).min(right.x + right.width) - left.x.max(right.x)).max(0.0);
    let height =
        f64::from((left.y + left.height).min(right.y + right.height) - left.y.max(right.y))
            .max(0.0);
    let intersection = width * height;
    let minimum = (f64::from(left.width) * f64::from(left.height))
        .min(f64::from(right.width) * f64::from(right.height));
    if minimum <= EPSILON { 0.0 } else { (intersection / minimum).clamp(0.0, 1.0) }
}

pub(crate) fn union_rect(left: Rect, right: Rect) -> Rect {
    let x = left.x.min(right.x);
    let y = left.y.min(right.y);
    let right_edge = (left.x + left.width).max(right.x + right.width);
    let bottom = (left.y + left.height).max(right.y + right.height);
    Rect { x, y, width: right_edge - x, height: bottom - y }
}

fn midpoint_vector(a: SourcePoint, b: SourcePoint, c: SourcePoint, d: SourcePoint) -> SourcePoint {
    SourcePoint { x: (b.x - a.x + d.x - c.x) / 2.0, y: (b.y - a.y + d.y - c.y) / 2.0 }
}

fn norm(value: SourcePoint) -> f32 {
    value.x.hypot(value.y)
}

fn normalized(value: SourcePoint) -> Option<SourcePoint> {
    let length = norm(value);
    (length > f32::EPSILON).then_some(SourcePoint { x: value.x / length, y: value.y / length })
}

fn signed_axis(value: SourcePoint, reference: SourcePoint) -> SourcePoint {
    if value.x * reference.x + value.y * reference.y < 0.0 {
        SourcePoint { x: -value.x, y: -value.y }
    } else {
        value
    }
}

fn project_polygon(polygon: &[SourcePoint; 4], axis: SourcePoint) -> (f32, f32) {
    polygon.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(minimum, maximum), point| {
        let value = point.x * axis.x + point.y * axis.y;
        (minimum.min(value), maximum.max(value))
    })
}

fn polygon_bounds(polygon: &[SourcePoint; 4]) -> Rect {
    let (minimum_x, maximum_x) = polygon
        .iter()
        .map(|point| point.x)
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(minimum, maximum), value| {
            (minimum.min(value), maximum.max(value))
        });
    let (minimum_y, maximum_y) = polygon
        .iter()
        .map(|point| point.y)
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(minimum, maximum), value| {
            (minimum.min(value), maximum.max(value))
        });
    Rect { x: minimum_x, y: minimum_y, width: maximum_x - minimum_x, height: maximum_y - minimum_y }
}

fn interval_overlap(left_start: f32, left_end: f32, right_start: f32, right_end: f32) -> f32 {
    (left_end.min(right_end) - left_start.max(right_start)).max(0.0)
}

fn interval_gap(left_start: f32, left_end: f32, right_start: f32, right_end: f32) -> f32 {
    (left_start - right_end).max(right_start - left_end).max(0.0)
}

fn polygon_area(polygon: &[SourcePoint; 4]) -> f64 {
    (0..4)
        .map(|index| {
            let a = polygon[index];
            let b = polygon[(index + 1) % 4];
            f64::from(a.x) * f64::from(b.y) - f64::from(a.y) * f64::from(b.x)
        })
        .sum::<f64>()
        .abs()
        / 2.0
}

fn convex_intersection_area(left: &[SourcePoint; 4], right: &[SourcePoint; 4]) -> f64 {
    let mut points = [SourcePoint::default(); 24];
    let mut length = 0_usize;
    for point in left {
        if point_in_convex(*point, right) {
            push_unique(&mut points, &mut length, *point);
        }
    }
    for point in right {
        if point_in_convex(*point, left) {
            push_unique(&mut points, &mut length, *point);
        }
    }
    for left_index in 0..4 {
        for right_index in 0..4 {
            if let Some(point) = segment_intersection(
                left[left_index],
                left[(left_index + 1) % 4],
                right[right_index],
                right[(right_index + 1) % 4],
            ) {
                push_unique(&mut points, &mut length, point);
            }
        }
    }
    if length < 3 {
        return 0.0;
    }
    let divisor = f32::from(u16::try_from(length).unwrap_or(u16::MAX));
    let center = SourcePoint {
        x: points[..length].iter().map(|point| point.x).sum::<f32>() / divisor,
        y: points[..length].iter().map(|point| point.y).sum::<f32>() / divisor,
    };
    points[..length].sort_by(|left, right| {
        (left.y - center.y)
            .atan2(left.x - center.x)
            .total_cmp(&(right.y - center.y).atan2(right.x - center.x))
    });
    (0..length)
        .map(|index| {
            let a = points[index];
            let b = points[(index + 1) % length];
            f64::from(a.x) * f64::from(b.y) - f64::from(a.y) * f64::from(b.x)
        })
        .sum::<f64>()
        .abs()
        / 2.0
}

fn point_in_convex(point: SourcePoint, polygon: &[SourcePoint; 4]) -> bool {
    let mut sign = 0_i8;
    for index in 0..4 {
        let value = cross(polygon[index], polygon[(index + 1) % 4], point);
        if value.abs() <= EPSILON {
            continue;
        }
        let current = if value.is_sign_positive() { 1 } else { -1 };
        if sign != 0 && sign != current {
            return false;
        }
        sign = current;
    }
    true
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "finite intersections of finite source-f32 segments remain representable as source f32"
)]
fn segment_intersection(
    left_start: SourcePoint,
    left_end: SourcePoint,
    right_start: SourcePoint,
    right_end: SourcePoint,
) -> Option<SourcePoint> {
    let denominator = f64::from(left_end.x - left_start.x) * f64::from(right_end.y - right_start.y)
        - f64::from(left_end.y - left_start.y) * f64::from(right_end.x - right_start.x);
    if denominator.abs() <= EPSILON {
        return None;
    }
    let left_fraction = (f64::from(right_start.x - left_start.x)
        * f64::from(right_end.y - right_start.y)
        - f64::from(right_start.y - left_start.y) * f64::from(right_end.x - right_start.x))
        / denominator;
    let right_fraction = (f64::from(right_start.x - left_start.x)
        * f64::from(left_end.y - left_start.y)
        - f64::from(right_start.y - left_start.y) * f64::from(left_end.x - left_start.x))
        / denominator;
    if !(-EPSILON..=1.0 + EPSILON).contains(&left_fraction)
        || !(-EPSILON..=1.0 + EPSILON).contains(&right_fraction)
    {
        return None;
    }
    Some(SourcePoint {
        x: (f64::from(left_start.x) + left_fraction * f64::from(left_end.x - left_start.x)) as f32,
        y: (f64::from(left_start.y) + left_fraction * f64::from(left_end.y - left_start.y)) as f32,
    })
}

fn push_unique(points: &mut [SourcePoint; 24], length: &mut usize, point: SourcePoint) {
    if points[..*length].iter().any(|existing| {
        f64::from(existing.x - point.x).abs() <= EPSILON
            && f64::from(existing.y - point.y).abs() <= EPSILON
    }) {
        return;
    }
    points[*length] = point;
    *length += 1;
}

fn cross(a: SourcePoint, b: SourcePoint, c: SourcePoint) -> f64 {
    f64::from(b.x - a.x) * f64::from(c.y - a.y) - f64::from(b.y - a.y) * f64::from(c.x - a.x)
}

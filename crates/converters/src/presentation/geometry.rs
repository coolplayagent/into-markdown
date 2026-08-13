use super::budget::{MAX_EXACT_EMU, MAX_GEOMETRY_COMPARISONS};
use super::error::{limit, malformed};
use super::model::{DisplayPoint, DisplayRect, Geometry, GroupTransform, Shape};
use super::schema::{SEEN_CHILD_EXTENT, SEEN_EXTENT};
use into_markdown_core::{ConversionError, ExecutionContext, Rect};
impl GroupTransform {
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn apply(self, geometry: Geometry) -> Result<Geometry, ConversionError> {
        if (self.child_extent_x == 0 && self.extent_x != 0)
            || (self.child_extent_y == 0 && self.extent_y != 0)
        {
            return Err(malformed(None, "non-empty group has a zero child extent"));
        }
        let mut corners = geometry.display_corners()?;
        let scale_x = if self.child_extent_x == 0 {
            if self.semantic_seen & (SEEN_EXTENT | SEEN_CHILD_EXTENT) == 0 { 1.0 } else { 0.0 }
        } else {
            coordinate_as_f64(self.extent_x, "group extent x")?
                / coordinate_as_f64(self.child_extent_x, "group child extent x")?
        };
        let scale_y = if self.child_extent_y == 0 {
            if self.semantic_seen & (SEEN_EXTENT | SEEN_CHILD_EXTENT) == 0 { 1.0 } else { 0.0 }
        } else {
            coordinate_as_f64(self.extent_y, "group extent y")?
                / coordinate_as_f64(self.child_extent_y, "group child extent y")?
        };
        let offset_x = coordinate_as_f64(self.offset_x, "group offset x")?;
        let offset_y = coordinate_as_f64(self.offset_y, "group offset y")?;
        let child_x = coordinate_as_f64(self.child_x, "group child x")?;
        let child_y = coordinate_as_f64(self.child_y, "group child y")?;
        let extent_x = coordinate_as_f64(self.extent_x, "group extent x")?;
        let extent_y = coordinate_as_f64(self.extent_y, "group extent y")?;
        let (sin, cos) = drawingml_sin_cos(self.rotation)?;
        let center_x = checked_display_coordinate(offset_x + extent_x / 2.0, "group center x")?;
        let center_y = checked_display_coordinate(offset_y + extent_y / 2.0, "group center y")?;
        for point in &mut corners {
            point.x = offset_x + (point.x - child_x) * scale_x;
            point.y = offset_y + (point.y - child_y) * scale_y;
            if self.flip_h {
                point.x = 2.0 * center_x - point.x;
            }
            if self.flip_v {
                point.y = 2.0 * center_y - point.y;
            }
            let delta_x = point.x - center_x;
            let delta_y = point.y - center_y;
            point.x = checked_display_coordinate(
                center_x + cos * delta_x - sin * delta_y,
                "group transformed x",
            )?;
            point.y = checked_display_coordinate(
                center_y + sin * delta_x + cos * delta_y,
                "group transformed y",
            )?;
        }
        Ok(Geometry { transformed_corners: Some(corners), ..geometry })
    }
}

impl DisplayRect {
    fn overlaps(self, other: Self) -> bool {
        self.left < other.right
            && other.left < self.right
            && self.top < other.bottom
            && other.top < self.bottom
    }
}

pub(super) fn convex_quadrilaterals_overlap(
    left: &[DisplayPoint; 4],
    right: &[DisplayPoint; 4],
) -> Result<bool, ConversionError> {
    if !quadrilateral_has_display_area(left)? || !quadrilateral_has_display_area(right)? {
        return Ok(false);
    }
    for polygon in [left, right] {
        for index in 0..4 {
            let next = (index + 1) % 4;
            let edge_x = polygon[next].x - polygon[index].x;
            let edge_y = polygon[next].y - polygon[index].y;
            let axis_x = -edge_y;
            let axis_y = edge_x;
            let magnitude_squared = axis_x.mul_add(axis_x, axis_y * axis_y);
            if !magnitude_squared.is_finite() {
                return Err(malformed(None, "non-finite overlap axis"));
            }
            if magnitude_squared == 0.0 {
                continue;
            }
            let project = |point: &DisplayPoint| point.x.mul_add(axis_x, point.y * axis_y);
            let left_min = left.iter().map(&project).fold(f64::INFINITY, f64::min);
            let left_max = left.iter().map(&project).fold(f64::NEG_INFINITY, f64::max);
            let right_min = right.iter().map(&project).fold(f64::INFINITY, f64::min);
            let right_max = right.iter().map(&project).fold(f64::NEG_INFINITY, f64::max);
            if ![left_min, left_max, right_min, right_max].iter().all(|value| value.is_finite()) {
                return Err(malformed(None, "non-finite overlap projection"));
            }
            // Edge-only contact has no display area and does not impose z-order coupling.
            if left_max <= right_min || right_max <= left_min {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn quadrilateral_has_display_area(polygon: &[DisplayPoint; 4]) -> Result<bool, ConversionError> {
    let origin = polygon[0];
    for first in 1..3 {
        for second in (first + 1)..4 {
            let first_x = polygon[first].x - origin.x;
            let first_y = polygon[first].y - origin.y;
            let second_x = polygon[second].x - origin.x;
            let second_y = polygon[second].y - origin.y;
            let cross = first_x.mul_add(second_y, -(first_y * second_x));
            if !cross.is_finite() {
                return Err(malformed(None, "non-finite quadrilateral area"));
            }
            if cross != 0.0 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

impl Geometry {
    pub(super) fn display_corners(self) -> Result<[DisplayPoint; 4], ConversionError> {
        if let Some(corners) = self.transformed_corners {
            return Ok(corners);
        }
        if self.cx < 0 || self.cy < 0 {
            return Err(malformed(None, "negative shape extent"));
        }
        let (sin, cos) = drawingml_sin_cos(self.rotation)?;
        let x = coordinate_as_f64(self.x, "shape x")?;
        let y = coordinate_as_f64(self.y, "shape y")?;
        let cx = coordinate_as_f64(self.cx, "shape width")?;
        let cy = coordinate_as_f64(self.cy, "shape height")?;
        let center_x = x + cx / 2.0;
        let center_y = y + cy / 2.0;
        let mut corners = [
            DisplayPoint { x, y },
            DisplayPoint { x: x + cx, y },
            DisplayPoint { x: x + cx, y: y + cy },
            DisplayPoint { x, y: y + cy },
        ];
        for point in &mut corners {
            if self.flip_h {
                point.x = 2.0 * center_x - point.x;
            }
            if self.flip_v {
                point.y = 2.0 * center_y - point.y;
            }
            let delta_x = point.x - center_x;
            let delta_y = point.y - center_y;
            point.x = checked_display_coordinate(
                center_x + cos * delta_x - sin * delta_y,
                "shape corner x",
            )?;
            point.y = checked_display_coordinate(
                center_y + sin * delta_x + cos * delta_y,
                "shape corner y",
            )?;
        }
        Ok(corners)
    }

    fn display_rect(self) -> Result<DisplayRect, ConversionError> {
        let corners = self.display_corners()?;
        let rect = DisplayRect {
            left: corners.iter().map(|point| point.x).fold(f64::INFINITY, f64::min),
            top: corners.iter().map(|point| point.y).fold(f64::INFINITY, f64::min),
            right: corners.iter().map(|point| point.x).fold(f64::NEG_INFINITY, f64::max),
            bottom: corners.iter().map(|point| point.y).fold(f64::NEG_INFINITY, f64::max),
        };
        if [rect.left, rect.top, rect.right, rect.bottom].iter().all(|value| value.is_finite()) {
            Ok(rect)
        } else {
            Err(malformed(None, "non-finite rotated shape bounds"))
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn bounds(self) -> Result<Rect, ConversionError> {
        let display = self.display_rect()?;
        #[allow(clippy::cast_precision_loss)]
        let bounds = Rect {
            x: (display.left / 914_400.0) as f32,
            y: (display.top / 914_400.0) as f32,
            width: ((display.right - display.left) / 914_400.0) as f32,
            height: ((display.bottom - display.top) / 914_400.0) as f32,
        };
        if [bounds.x, bounds.y, bounds.width, bounds.height].iter().all(|value| value.is_finite()) {
            Ok(bounds)
        } else {
            Err(malformed(None, "shape bounds exceed finite IR coordinates"))
        }
    }
}

fn drawingml_sin_cos(rotation: i32) -> Result<(f64, f64), ConversionError> {
    let degrees = f64::from(rotation.rem_euclid(21_600_000)) / 60_000.0;
    let radians = degrees.to_radians();
    let (sin, cos) = radians.sin_cos();
    if sin.is_finite() && cos.is_finite() {
        Ok((sin, cos))
    } else {
        Err(malformed(None, "non-finite DrawingML rotation"))
    }
}

#[allow(clippy::cast_precision_loss)]
fn checked_display_coordinate(value: f64, label: &str) -> Result<f64, ConversionError> {
    if !value.is_finite() || value < -(MAX_EXACT_EMU as f64) || value > MAX_EXACT_EMU as f64 {
        return Err(malformed(None, format!("{label} overflow")));
    }
    Ok(value)
}

#[allow(clippy::cast_precision_loss)]
fn coordinate_as_f64(value: i64, label: &str) -> Result<f64, ConversionError> {
    if !(-MAX_EXACT_EMU..=MAX_EXACT_EMU).contains(&value) {
        return Err(malformed(None, format!("{label} exceeds exact geometry range")));
    }
    Ok(value as f64)
}

fn geometry_root(parents: &mut [usize], mut index: usize) -> usize {
    while parents[index] != index {
        parents[index] = parents[parents[index]];
        index = parents[index];
    }
    index
}

#[allow(clippy::too_many_lines)]
pub(super) fn sort_shapes_for_reading(
    shapes: &mut [Shape],
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let temporary_bytes = u64::try_from(shapes.len())
        .unwrap_or(u64::MAX)
        .checked_mul(384)
        .and_then(|value| value.checked_add(4096))
        .ok_or_else(|| limit("max_memory_bytes", "geometry working-set plan overflow"))?;
    let _working_set = context.reserve_memory(temporary_bytes)?;
    let mut rects = Vec::new();
    rects.try_reserve_exact(shapes.len()).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve geometry bounds: {error}"))
    })?;
    let mut corners = Vec::new();
    corners.try_reserve_exact(shapes.len()).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve geometry corners: {error}"))
    })?;
    for (index, shape) in shapes.iter().enumerate() {
        if index.is_multiple_of(256) {
            context.checkpoint()?;
        }
        let shape_corners = shape.geometry.display_corners()?;
        rects.push(shape.geometry.display_rect()?);
        corners.push(shape_corners);
    }
    let mut order = Vec::new();
    order.try_reserve_exact(shapes.len()).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve geometry order: {error}"))
    })?;
    order.extend(0..shapes.len());
    order.sort_unstable_by(|left, right| {
        rects[*left]
            .left
            .total_cmp(&rects[*right].left)
            .then_with(|| rects[*left].top.total_cmp(&rects[*right].top))
    });
    let mut parents = Vec::new();
    parents.try_reserve_exact(shapes.len()).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve overlap groups: {error}"))
    })?;
    parents.extend(0..shapes.len());
    let mut comparisons = 0_usize;
    for (position, left) in order.iter().copied().enumerate() {
        for right in order.iter().copied().skip(position + 1) {
            if rects[right].left >= rects[left].right {
                break;
            }
            comparisons = comparisons
                .checked_add(1)
                .ok_or_else(|| limit("geometry_comparisons", "comparison count overflow"))?;
            if comparisons > MAX_GEOMETRY_COMPARISONS {
                return Err(limit(
                    "geometry_comparisons",
                    format!("more than {MAX_GEOMETRY_COMPARISONS} candidate overlaps"),
                ));
            }
            if comparisons.is_multiple_of(1024) {
                context.checkpoint()?;
            }
            if rects[left].overlaps(rects[right])
                && convex_quadrilaterals_overlap(&corners[left], &corners[right])?
            {
                let left_root = geometry_root(&mut parents, left);
                let right_root = geometry_root(&mut parents, right);
                if left_root != right_root {
                    parents[right_root] = left_root;
                }
            }
        }
    }
    let mut component_keys = Vec::<Option<(f64, f64, usize)>>::new();
    component_keys.try_reserve_exact(shapes.len()).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve component keys: {error}"))
    })?;
    component_keys.resize(shapes.len(), None);
    let component_slots = shapes
        .iter()
        .map(|shape| shape.z_order)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| limit("max_memory_bytes", "component index count overflow"))?;
    let mut shape_components = Vec::<usize>::new();
    shape_components.try_reserve_exact(component_slots).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve component indexes: {error}"))
    })?;
    shape_components.resize(component_slots, 0);
    for (index, shape) in shapes.iter().enumerate() {
        if index.is_multiple_of(256) {
            context.checkpoint()?;
        }
        let root = geometry_root(&mut parents, index);
        shape_components[shape.z_order] = root;
        let key = component_keys[root].get_or_insert((
            rects[index].top,
            rects[index].left,
            shape.z_order,
        ));
        if rects[index].top.total_cmp(&key.0).is_lt()
            || (rects[index].top.total_cmp(&key.0).is_eq()
                && rects[index].left.total_cmp(&key.1).is_lt())
        {
            key.0 = rects[index].top;
            key.1 = rects[index].left;
        }
        key.2 = key.2.min(shape.z_order);
    }
    shapes.sort_unstable_by(|left, right| {
        let left_root = shape_components[left.z_order];
        let right_root = shape_components[right.z_order];
        if left_root == right_root {
            left.z_order.cmp(&right.z_order)
        } else {
            let left_key = component_keys[left_root].expect("geometry component has key");
            let right_key = component_keys[right_root].expect("geometry component has key");
            left_key
                .0
                .total_cmp(&right_key.0)
                .then_with(|| left_key.1.total_cmp(&right_key.1))
                .then_with(|| left_key.2.cmp(&right_key.2))
        }
    });
    Ok(())
}

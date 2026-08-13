use super::Candidate;
use super::budget::MergeBudget;
use super::geometry::{RegionGeometry, line_compatible, union_rect};
use into_markdown_core::{ConversionError, Rect, SourcePoint};

pub(crate) struct MergedLine {
    pub(crate) candidates: Vec<Candidate>,
    pub(crate) text: String,
    pub(crate) bounds: Rect,
    pub(crate) angle_degrees: f32,
    pub(crate) line_height: f32,
}

pub(crate) fn merge_lines(
    candidates: Vec<Candidate>,
    language_hint: Option<&str>,
    budget: &mut MergeBudget<'_>,
    materialize_meter: &mut super::text::TextMeter,
) -> Result<Vec<MergedLine>, ConversionError> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let maximum_thickness =
        candidates.iter().map(|candidate| candidate.geometry.thickness).fold(0.0_f32, f32::max);
    let mut parent = Vec::new();
    parent.try_reserve_exact(candidates.len()).map_err(|_| super::memory())?;
    parent.extend(0..candidates.len());
    let mut ordered = Vec::new();
    ordered.try_reserve_exact(candidates.len()).map_err(|_| super::memory())?;
    ordered.extend(0..candidates.len());
    ordered.sort_by(|left, right| {
        candidates[*left]
            .geometry
            .bounds
            .x
            .total_cmp(&candidates[*right].geometry.bounds.x)
            .then_with(|| {
                candidates[*left].geometry.bounds.y.total_cmp(&candidates[*right].geometry.bounds.y)
            })
            .then_with(|| candidates[*left].source_index.cmp(&candidates[*right].source_index))
    });
    for (position, &left) in ordered.iter().enumerate() {
        let search_right = candidates[left].geometry.bounds.x
            + candidates[left].geometry.bounds.width
            + 2.5 * maximum_thickness;
        for &right in &ordered[position + 1..] {
            if candidates[right].geometry.bounds.x > search_right {
                break;
            }
            budget.consume(1)?;
            if line_compatible(&candidates[left].geometry, &candidates[right].geometry) {
                union(&mut parent, left, right);
            }
        }
    }

    let mut groups = Vec::<Option<Vec<usize>>>::new();
    groups.try_reserve_exact(candidates.len()).map_err(|_| super::memory())?;
    groups.resize_with(candidates.len(), || None);
    let mut group_count = 0_usize;
    for index in 0..candidates.len() {
        let root = find(&mut parent, index);
        if groups[root].is_none() {
            groups[root] = Some(Vec::new());
            group_count += 1;
        }
        let group = groups[root].as_mut().ok_or_else(|| super::ocr("lineGroupMissing"))?;
        group.try_reserve(1).map_err(|_| super::memory())?;
        group.push(index);
    }
    let mut slots = Vec::new();
    slots.try_reserve_exact(candidates.len()).map_err(|_| super::memory())?;
    slots.extend(candidates.into_iter().map(Some));
    let mut lines = Vec::new();
    lines.try_reserve_exact(group_count).map_err(|_| super::memory())?;
    for indexes in groups.into_iter().flatten() {
        budget.checkpoint()?;
        let reference = slots[indexes[0]].as_ref().ok_or_else(|| super::ocr("lineSlotMissing"))?;
        let axis = reference.geometry.direction;
        let mut indexes = indexes;
        indexes.sort_by(|left, right| {
            projection_center(slots[*left].as_ref().unwrap().geometry, axis)
                .total_cmp(&projection_center(slots[*right].as_ref().unwrap().geometry, axis))
                .then_with(|| {
                    slots[*left]
                        .as_ref()
                        .unwrap()
                        .source_index
                        .cmp(&slots[*right].as_ref().unwrap().source_index)
                })
        });
        let mut members = Vec::new();
        members.try_reserve_exact(indexes.len()).map_err(|_| super::memory())?;
        for index in indexes {
            members.push(slots[index].take().ok_or_else(|| super::ocr("lineSlotMissing"))?);
        }
        lines.push(build_line(members, axis, language_hint, materialize_meter)?);
    }
    lines.sort_by(|left, right| {
        left.bounds
            .y
            .total_cmp(&right.bounds.y)
            .then_with(|| left.bounds.x.total_cmp(&right.bounds.x))
            .then_with(|| left.angle_degrees.total_cmp(&right.angle_degrees))
            .then_with(|| left.candidates[0].source_index.cmp(&right.candidates[0].source_index))
    });
    Ok(lines)
}

fn build_line(
    members: Vec<Candidate>,
    axis: SourcePoint,
    language_hint: Option<&str>,
    materialize_meter: &mut super::text::TextMeter,
) -> Result<MergedLine, ConversionError> {
    let mut text = String::new();
    let planned = members.iter().try_fold(0_usize, |total, member| {
        total
            .checked_add(member.text.len())
            .and_then(|value| value.checked_add(1))
            .ok_or_else(super::memory)
    })?;
    text.try_reserve_exact(planned).map_err(|_| super::memory())?;
    let mut previous_end = None;
    let mut bounds = members[0].geometry.bounds;
    let mut doubled_angle_cosine = 0.0_f32;
    let mut doubled_angle_sine = 0.0_f32;
    let mut maximum_height = 0.0_f32;
    for member in &members {
        let (start, end) = member.geometry.projection(axis);
        if should_insert_space(
            language_hint,
            text.chars().next_back(),
            member.text.chars().next(),
            member.geometry.angle_degrees,
        ) && previous_end
            .is_some_and(|previous| start - previous > 0.15 * member.geometry.thickness.max(1.0))
            && !text.chars().next_back().is_some_and(char::is_whitespace)
        {
            text.push(' ');
            materialize_meter.consume(1)?;
        }
        super::text::append(&mut text, &member.text, materialize_meter)?;
        previous_end = Some(end.max(previous_end.unwrap_or(end)));
        bounds = union_rect(bounds, member.geometry.bounds);
        let doubled_angle = member.geometry.angle_degrees.to_radians() * 2.0;
        doubled_angle_cosine += doubled_angle.cos();
        doubled_angle_sine += doubled_angle.sin();
        maximum_height = maximum_height.max(member.geometry.thickness);
    }
    Ok(MergedLine {
        candidates: members,
        text,
        bounds,
        angle_degrees: (doubled_angle_sine.atan2(doubled_angle_cosine).to_degrees() / 2.0)
            .rem_euclid(180.0),
        line_height: maximum_height,
    })
}

fn should_insert_space(
    language_hint: Option<&str>,
    left: Option<char>,
    right: Option<char>,
    angle: f32,
) -> bool {
    if (angle - 90.0).abs() <= 15.0 {
        return false;
    }
    let Some((left, right)) = left.zip(right) else { return false };
    if left.is_ascii_alphanumeric() && right.is_ascii_alphanumeric() {
        return true;
    }
    if is_cjk(left) || is_cjk(right) {
        return false;
    }
    language_hint == Some("en")
}

fn is_cjk(value: char) -> bool {
    matches!(
        value,
        '\u{2e80}'..='\u{2eff}'
            | '\u{3000}'..='\u{303f}'
            | '\u{31c0}'..='\u{31ef}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{f900}'..='\u{faff}'
            | '\u{20000}'..='\u{2fa1f}'
    )
}

fn projection_center(geometry: RegionGeometry, axis: SourcePoint) -> f32 {
    geometry.center.x * axis.x + geometry.center.y * axis.y
}

fn find(parent: &mut [usize], mut index: usize) -> usize {
    let mut root = index;
    while parent[root] != root {
        root = parent[root];
    }
    while parent[index] != index {
        let next = parent[index];
        parent[index] = root;
        index = next;
    }
    root
}

fn union(parent: &mut [usize], left: usize, right: usize) {
    let left_root = find(parent, left);
    let right_root = find(parent, right);
    if left_root == right_root {
        return;
    }
    if left_root < right_root {
        parent[right_root] = left_root;
    } else {
        parent[left_root] = right_root;
    }
}

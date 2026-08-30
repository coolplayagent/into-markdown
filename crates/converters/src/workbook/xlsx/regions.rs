use crate::workbook::cell::parse_cell_range;
use crate::workbook::error::{limit, malformed};
use crate::workbook::model::CellCoordinate;
use crate::workbook::opc::relationships::{
    decode_attr, is_spreadsheet_namespace, require_spreadsheet_namespace, validate_xml_reference,
};
use crate::workbook::schema::{OFFICE_REL_NS, OFFICE_REL_STRICT_NS};
use crate::workbook::xlsx::sheet_index::SheetLayout;
use into_markdown_core::{ConversionError, ExecutionContext};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

const REGION_PAGE_ROWS: u32 = 2_048;
const REGION_PAGE_CELLS: u64 = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SparseRun {
    pub(super) row: u32,
    pub(super) first_column: u32,
    pub(super) last_column: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::workbook) struct SparseRegion {
    pub(in crate::workbook) first_row: u32,
    pub(in crate::workbook) last_row: u32,
    pub(in crate::workbook) first_column: u32,
    pub(in crate::workbook) last_column: u32,
    pub(in crate::workbook) occupied_cells: u64,
    pub(in crate::workbook) contains_merge: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::workbook) struct MergeRange {
    pub(in crate::workbook) first_row: u32,
    pub(in crate::workbook) last_row: u32,
    pub(in crate::workbook) first_column: u32,
    pub(in crate::workbook) last_column: u32,
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::workbook) struct RegionPlan {
    pub(in crate::workbook) regions: Vec<SparseRegion>,
    pub(in crate::workbook) empty_cells_used: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::workbook) struct RegionPage {
    pub(super) first_row: u32,
    pub(super) last_row: u32,
    pub(super) first_column: u32,
    pub(super) last_column: u32,
}

pub(super) fn build_sparse_regions(
    runs: &[SparseRun],
    merges: &[MergeRange],
    tables: &[(CellCoordinate, CellCoordinate)],
    empty_cell_budget: u64,
) -> Result<RegionPlan, ConversionError> {
    let mut regions = regions_from_runs(runs)?;
    append_merge_regions(&mut regions, merges)?;
    append_table_regions(&mut regions, tables)?;
    let (regions, empty_cells_used) = coalesce_regions(regions, empty_cell_budget)?;
    // Every merge is itself an input region, and coalescing unconditionally unions
    // intersecting merge owners. A non-overlap sweep therefore proves that each
    // merge is retained by exactly one output region without an O(merges * regions)
    // ownership rescan.
    let regions = if validate_non_overlapping_regions(&regions).is_ok() {
        regions
    } else {
        let regions = consolidate_merge_owners(regions, merges)?;
        validate_merge_ownership(&regions, merges)?;
        regions
    };
    Ok(RegionPlan { regions, empty_cells_used })
}

pub(in crate::workbook) fn plan_sheet_regions(
    layout: &SheetLayout,
    tables: &[(CellCoordinate, CellCoordinate)],
    empty_cell_budget: u64,
) -> Result<RegionPlan, ConversionError> {
    let runs = layout
        .runs
        .iter()
        .map(|run| SparseRun {
            row: run.row,
            first_column: run.first_column,
            last_column: run.last_column,
        })
        .collect::<Vec<_>>();
    let merges = layout
        .merges
        .iter()
        .map(|(start, end)| MergeRange {
            first_row: start.0,
            last_row: end.0,
            first_column: start.1,
            last_column: end.1,
        })
        .collect::<Vec<_>>();
    let mut plan = build_sparse_regions(&runs, &merges, tables, empty_cell_budget)?;
    let required_end = layout
        .merges
        .iter()
        .map(|(_, end)| *end)
        .chain(tables.iter().map(|(_, end)| *end))
        .chain(layout.bounds)
        .reduce(|left, right| (left.0.max(right.0), left.1.max(right.1)));
    if let Some(declared) = layout.declared_bounds
        && required_end.is_none_or(|end| declared.0 >= end.0 && declared.1 >= end.1)
        && let Some(mut region) =
            compact_declared_region(declared, layout.populated_cells, empty_cell_budget)
    {
        region.contains_merge = !merges.is_empty();
        plan.regions = vec![region];
        plan.empty_cells_used = region_area(region).saturating_sub(layout.populated_cells);
    }
    Ok(plan)
}

pub(in crate::workbook) fn parse_table_part_ids(
    xml: &[u8],
    part: &str,
    context: &ExecutionContext,
) -> Result<Vec<String>, ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut ids = Vec::new();
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, Event::Start(event) | Event::Empty(event)))
                if event.local_name().as_ref() == b"tablePart" =>
            {
                require_spreadsheet_namespace(&namespace, part)?;
                let mut relationship_id = None;
                for attr in event.attributes().with_checks(false) {
                    let attr = attr.map_err(|error| {
                        malformed(Some(part), format!("invalid tablePart attribute: {error}"))
                    })?;
                    if attr.key.local_name().as_ref() == b"id"
                        && matches!(
                            reader.resolve_attribute(attr.key),
                            (ResolveResult::Bound(namespace), _)
                                if namespace.as_ref() == OFFICE_REL_NS
                                    || namespace.as_ref() == OFFICE_REL_STRICT_NS
                        )
                        && relationship_id.replace(decode_attr(&attr, part)?).is_some()
                    {
                        return Err(malformed(Some(part), "duplicate table relationship id"));
                    }
                }
                let relationship_id = relationship_id
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| malformed(Some(part), "table relationship id is missing"))?;
                if ids.contains(&relationship_id) {
                    return Err(malformed(Some(part), "duplicate table relationship reference"));
                }
                ids.push(relationship_id);
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::GeneralRef(reference))) => {
                validate_xml_reference(reference.as_ref(), part)?;
            }
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid worksheet XML: {error}")));
            }
            _ => {}
        }
    }
    Ok(ids)
}

pub(in crate::workbook) fn parse_table_range(
    xml: &[u8],
    part: &str,
    context: &ExecutionContext,
) -> Result<(CellCoordinate, CellCoordinate), ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut range = None;
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, Event::Start(event) | Event::Empty(event)))
                if event.local_name().as_ref() == b"table"
                    && is_spreadsheet_namespace(&namespace) =>
            {
                require_spreadsheet_namespace(&namespace, part)?;
                if range.is_some() {
                    return Err(malformed(Some(part), "duplicate table root"));
                }
                for attr in event.attributes().with_checks(false) {
                    let attr = attr.map_err(|error| {
                        malformed(Some(part), format!("invalid table attribute: {error}"))
                    })?;
                    if attr.key.as_ref() == b"ref"
                        && range.replace(parse_cell_range(&decode_attr(&attr, part)?)?).is_some()
                    {
                        return Err(malformed(Some(part), "duplicate table range"));
                    }
                }
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::GeneralRef(reference))) => {
                validate_xml_reference(reference.as_ref(), part)?;
            }
            Ok((_, Event::Eof)) => break,
            Err(error) => return Err(malformed(Some(part), format!("invalid table XML: {error}"))),
            _ => {}
        }
    }
    range.ok_or_else(|| malformed(Some(part), "table range is missing"))
}

pub(super) fn compact_declared_region(
    end: (u32, u32),
    populated_cells: u64,
    empty_cell_budget: u64,
) -> Option<SparseRegion> {
    if populated_cells == 0 {
        return None;
    }
    let area = (u64::from(end.0) + 1).checked_mul(u64::from(end.1) + 1)?;
    if area > 16_384 && area.saturating_sub(populated_cells) > empty_cell_budget {
        return None;
    }
    Some(SparseRegion {
        first_row: 0,
        last_row: end.0,
        first_column: 0,
        last_column: end.1,
        occupied_cells: populated_cells,
        contains_merge: false,
    })
}

fn region_area(region: SparseRegion) -> u64 {
    (u64::from(region.last_row - region.first_row) + 1)
        .saturating_mul(u64::from(region.last_column - region.first_column) + 1)
}

fn consolidate_merge_owners(
    mut regions: Vec<SparseRegion>,
    merges: &[MergeRange],
) -> Result<Vec<SparseRegion>, ConversionError> {
    for merge_range in merges.iter().copied() {
        validate_merge(merge_range)?;
        let merge_region = SparseRegion::from(merge_range);
        let mut owner = merge_region;
        let mut untouched = Vec::with_capacity(regions.len());
        for region in regions {
            if intersects(region, merge_region) {
                owner = union(owner, region);
            } else {
                untouched.push(region);
            }
        }
        untouched.push(owner);
        regions = untouched;
    }
    regions.sort_unstable_by_key(|region| {
        (region.first_row, region.first_column, region.last_row, region.last_column)
    });
    Ok(regions)
}

pub(in crate::workbook) fn paginate_region(
    region: SparseRegion,
    merges: &[MergeRange],
) -> Result<Vec<RegionPage>, ConversionError> {
    validate_region(region)?;
    let width = u64::from(region.last_column - region.first_column) + 1;
    let rows_by_cells = u32::try_from((REGION_PAGE_CELLS / width).max(1)).unwrap_or(u32::MAX);
    let rows_per_page = REGION_PAGE_ROWS.min(rows_by_cells);
    let relevant_merges = merges
        .iter()
        .copied()
        .filter(|merge_range| intersects(region, (*merge_range).into()))
        .map(|merge_range| {
            validate_merge(merge_range)?;
            if !contains(region, merge_range.into()) {
                return Err(malformed(None, "merged range crosses sparse region bounds"));
            }
            Ok(merge_range)
        })
        .collect::<Result<Vec<_>, ConversionError>>()?;

    let mut pages = Vec::new();
    let mut first_row = region.first_row;
    while first_row <= region.last_row {
        let mut last_row =
            first_row.saturating_add(rows_per_page.saturating_sub(1)).min(region.last_row);
        loop {
            let extended = relevant_merges
                .iter()
                .filter(|merge_range| {
                    merge_range.first_row <= last_row && merge_range.last_row > last_row
                })
                .fold(last_row, |end, merge_range| end.max(merge_range.last_row));
            if extended == last_row {
                break;
            }
            last_row = extended.min(region.last_row);
        }
        pages.push(RegionPage {
            first_row,
            last_row,
            first_column: region.first_column,
            last_column: region.last_column,
        });
        let Some(next) = last_row.checked_add(1) else { break };
        first_row = next;
    }
    Ok(pages)
}

fn regions_from_runs(runs: &[SparseRun]) -> Result<Vec<SparseRegion>, ConversionError> {
    let mut regions = Vec::<SparseRegion>::new();
    regions
        .try_reserve_exact(runs.len())
        .map_err(|_| limit("max_memory_bytes", "cannot reserve sparse worksheet runs"))?;
    let mut previous = None::<SparseRun>;
    for run in runs.iter().copied() {
        if run.first_column > run.last_column {
            return Err(malformed(None, "sparse worksheet run has reversed columns"));
        }
        if let Some(prior) = previous
            && (run.row < prior.row
                || (run.row == prior.row && run.first_column <= prior.last_column))
        {
            return Err(malformed(None, "sparse worksheet runs are duplicated or unordered"));
        }
        previous = Some(run);
        if let Some(region) = regions.last_mut()
            && region.first_row == run.row
            && region.last_column.checked_add(1) == Some(run.first_column)
        {
            region.last_column = run.last_column;
            region.occupied_cells = region
                .occupied_cells
                .checked_add(u64::from(run.last_column - run.first_column) + 1)
                .ok_or_else(|| limit("max_table_cells", "sparse run cell count overflowed"))?;
            continue;
        }
        regions.push(SparseRegion {
            first_row: run.row,
            last_row: run.row,
            first_column: run.first_column,
            last_column: run.last_column,
            occupied_cells: u64::from(run.last_column - run.first_column) + 1,
            contains_merge: false,
        });
    }
    Ok(regions)
}

fn append_merge_regions(
    regions: &mut Vec<SparseRegion>,
    merges: &[MergeRange],
) -> Result<(), ConversionError> {
    regions
        .try_reserve_exact(merges.len())
        .map_err(|_| limit("max_memory_bytes", "cannot reserve sparse merge regions"))?;
    let mut ordered = merges.to_vec();
    ordered.sort_unstable_by_key(|merge_range| {
        (
            merge_range.first_row,
            merge_range.first_column,
            merge_range.last_row,
            merge_range.last_column,
        )
    });
    validate_non_overlapping_merges(&ordered)?;
    for merge_range in ordered {
        let merge_region: SparseRegion = merge_range.into();
        regions.push(merge_region);
    }
    Ok(())
}

fn append_table_regions(
    regions: &mut Vec<SparseRegion>,
    tables: &[(CellCoordinate, CellCoordinate)],
) -> Result<(), ConversionError> {
    regions
        .try_reserve_exact(tables.len())
        .map_err(|_| limit("max_memory_bytes", "cannot reserve worksheet table regions"))?;
    for &(start, end) in tables {
        if start.0 > end.0 || start.1 > end.1 {
            return Err(malformed(None, "worksheet table range is reversed"));
        }
        let mut table_region = SparseRegion {
            first_row: start.0,
            last_row: end.0,
            first_column: start.1,
            last_column: end.1,
            occupied_cells: 0,
            contains_merge: false,
        };
        loop {
            let mut absorbed = false;
            let mut untouched = Vec::with_capacity(regions.len());
            for region in regions.drain(..) {
                if intersects(region, table_region) {
                    table_region = union(table_region, region);
                    absorbed = true;
                } else {
                    untouched.push(region);
                }
            }
            *regions = untouched;
            if !absorbed {
                break;
            }
        }
        regions.push(table_region);
    }
    Ok(())
}

fn coalesce_regions(
    mut regions: Vec<SparseRegion>,
    empty_cell_budget: u64,
) -> Result<(Vec<SparseRegion>, u64), ConversionError> {
    regions.sort_unstable_by_key(|region| {
        (region.first_row, region.first_column, region.last_row, region.last_column)
    });
    let mut remaining = empty_cell_budget;
    let mut active = BTreeMap::<u32, SparseRegion>::new();
    let mut expirations = BinaryHeap::<Reverse<(u32, u32)>>::new();
    let mut completed = Vec::new();

    for mut region in regions {
        validate_region(region)?;
        while let Some(Reverse((expires, first_column))) = expirations.peek().copied() {
            if expires >= region.first_row {
                break;
            }
            expirations.pop();
            if active
                .get(&first_column)
                .is_some_and(|candidate| candidate.last_row.saturating_add(1) == expires)
            {
                completed.push(active.remove(&first_column).expect("active region exists"));
            }
        }

        loop {
            let mut candidates = Vec::new();
            if let Some((&key, candidate)) = active.range(..=region.first_column).next_back()
                && candidate.last_column.saturating_add(1) >= region.first_column
            {
                candidates.push(key);
            }
            for (&key, candidate) in
                active.range(region.first_column..=region.last_column.saturating_add(1))
            {
                if candidate.first_column <= region.last_column.saturating_add(1)
                    && candidates.first() != Some(&key)
                {
                    candidates.push(key);
                }
            }
            if candidates.is_empty() {
                break;
            }

            let mut combined = region;
            let mut prior_area = area(region)?;
            let mut mandatory_merge = false;
            for key in &candidates {
                let candidate = active.get(key).expect("candidate remains active");
                mandatory_merge |= (region.contains_merge || candidate.contains_merge)
                    && intersects(region, *candidate);
                prior_area = prior_area.saturating_add(area(*candidate)?);
                combined = union(combined, *candidate);
            }
            let extra = area(combined)?.saturating_sub(prior_area);
            if !mandatory_merge && extra > remaining {
                for key in candidates {
                    completed.push(active.remove(&key).expect("candidate remains active"));
                }
                break;
            }
            if !mandatory_merge {
                remaining -= extra;
            }
            for key in candidates {
                let candidate = active.remove(&key).expect("candidate remains active");
                region = union(region, candidate);
            }
        }

        let key = region.first_column;
        if let Some(displaced) = active.insert(key, region) {
            completed.push(displaced);
        }
        expirations.push(Reverse((region.last_row.saturating_add(1), key)));
    }
    completed.extend(active.into_values());
    completed.sort_unstable_by_key(|region| {
        (region.first_row, region.first_column, region.last_row, region.last_column)
    });
    Ok((completed, empty_cell_budget - remaining))
}

fn validate_merge_ownership(
    regions: &[SparseRegion],
    merges: &[MergeRange],
) -> Result<(), ConversionError> {
    for merge_range in merges.iter().copied() {
        let owners =
            regions.iter().copied().filter(|region| contains(*region, merge_range.into())).count();
        if owners != 1 {
            return Err(malformed(
                None,
                "merged range does not belong to exactly one sparse region",
            ));
        }
    }
    Ok(())
}

fn validate_region(region: SparseRegion) -> Result<(), ConversionError> {
    if region.first_row > region.last_row || region.first_column > region.last_column {
        return Err(malformed(None, "sparse region has reversed bounds"));
    }
    Ok(())
}

fn validate_merge(merge_range: MergeRange) -> Result<(), ConversionError> {
    if merge_range.first_row > merge_range.last_row
        || merge_range.first_column > merge_range.last_column
    {
        return Err(malformed(None, "sparse merged range has reversed bounds"));
    }
    Ok(())
}

fn validate_non_overlapping_merges(merges: &[MergeRange]) -> Result<(), ConversionError> {
    let mut active = BTreeMap::<u32, MergeRange>::new();
    let mut expirations = BinaryHeap::<Reverse<(u32, u32)>>::new();
    for merge_range in merges.iter().copied() {
        validate_merge(merge_range)?;
        while let Some(Reverse((end_row, first_column))) = expirations.peek().copied() {
            if end_row >= merge_range.first_row {
                break;
            }
            expirations.pop();
            if active.get(&first_column).is_some_and(|value| value.last_row == end_row) {
                active.remove(&first_column);
            }
        }
        let predecessor_overlaps = active
            .range(..=merge_range.last_column)
            .next_back()
            .is_some_and(|(_, prior)| prior.last_column >= merge_range.first_column);
        if predecessor_overlaps {
            return Err(malformed(None, "overlapping sparse merged ranges"));
        }
        if active.insert(merge_range.first_column, merge_range).is_some() {
            return Err(malformed(None, "overlapping sparse merged ranges"));
        }
        expirations.push(Reverse((merge_range.last_row, merge_range.first_column)));
    }
    Ok(())
}

fn validate_non_overlapping_regions(regions: &[SparseRegion]) -> Result<(), ConversionError> {
    let mut ordered = regions.to_vec();
    ordered.sort_unstable_by_key(|region| {
        (region.first_row, region.first_column, region.last_row, region.last_column)
    });
    let mut active = BTreeMap::<u32, SparseRegion>::new();
    let mut expirations = BinaryHeap::<Reverse<(u32, u32)>>::new();
    for region in ordered {
        validate_region(region)?;
        while let Some(Reverse((end_row, first_column))) = expirations.peek().copied() {
            if end_row >= region.first_row {
                break;
            }
            expirations.pop();
            if active.get(&first_column).is_some_and(|value| value.last_row == end_row) {
                active.remove(&first_column);
            }
        }
        if active
            .range(..=region.last_column)
            .next_back()
            .is_some_and(|(_, prior)| prior.last_column >= region.first_column)
        {
            return Err(malformed(None, "sparse worksheet regions overlap"));
        }
        if active.insert(region.first_column, region).is_some() {
            return Err(malformed(None, "sparse worksheet regions overlap"));
        }
        expirations.push(Reverse((region.last_row, region.first_column)));
    }
    Ok(())
}

fn area(region: SparseRegion) -> Result<u64, ConversionError> {
    (u64::from(region.last_row - region.first_row) + 1)
        .checked_mul(u64::from(region.last_column - region.first_column) + 1)
        .ok_or_else(|| limit("max_table_cells", "sparse region area overflowed"))
}

fn union(left: SparseRegion, right: SparseRegion) -> SparseRegion {
    SparseRegion {
        first_row: left.first_row.min(right.first_row),
        last_row: left.last_row.max(right.last_row),
        first_column: left.first_column.min(right.first_column),
        last_column: left.last_column.max(right.last_column),
        occupied_cells: left.occupied_cells.saturating_add(right.occupied_cells),
        contains_merge: left.contains_merge || right.contains_merge,
    }
}

fn contains(container: SparseRegion, child: SparseRegion) -> bool {
    child.first_row >= container.first_row
        && child.last_row <= container.last_row
        && child.first_column >= container.first_column
        && child.last_column <= container.last_column
}

fn intersects(left: SparseRegion, right: SparseRegion) -> bool {
    left.first_row <= right.last_row
        && left.last_row >= right.first_row
        && left.first_column <= right.last_column
        && left.last_column >= right.first_column
}

impl From<MergeRange> for SparseRegion {
    fn from(value: MergeRange) -> Self {
        Self {
            first_row: value.first_row,
            last_row: value.last_row,
            first_column: value.first_column,
            last_column: value.last_column,
            occupied_cells: 0,
            contains_merge: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MergeRange, SparseRegion, SparseRun, build_sparse_regions, paginate_region};

    #[test]
    fn extreme_blank_space_stays_as_two_ordered_regions() {
        let plan = build_sparse_regions(
            &[
                SparseRun { row: 0, first_column: 0, last_column: 0 },
                SparseRun { row: 1_048_575, first_column: 16_383, last_column: 16_383 },
            ],
            &[],
            &[],
            4_096,
        )
        .unwrap();

        assert_eq!(plan.empty_cells_used, 0);
        assert_eq!(plan.regions.len(), 2);
        assert_eq!((plan.regions[0].first_row, plan.regions[0].first_column), (0, 0));
        assert_eq!((plan.regions[1].first_row, plan.regions[1].first_column), (1_048_575, 16_383));
    }

    #[test]
    fn adjacent_variable_width_rows_form_one_near_region() {
        let plan = build_sparse_regions(
            &[
                SparseRun { row: 0, first_column: 0, last_column: 1 },
                SparseRun { row: 0, first_column: 16_383, last_column: 16_383 },
                SparseRun { row: 1, first_column: 0, last_column: 2 },
            ],
            &[],
            &[],
            1,
        )
        .unwrap();

        assert_eq!(plan.empty_cells_used, 1);
        assert_eq!(plan.regions.len(), 2);
        assert_eq!(
            plan.regions[0],
            SparseRegion {
                first_row: 0,
                last_row: 1,
                first_column: 0,
                last_column: 2,
                occupied_cells: 5,
                contains_merge: false,
            }
        );
    }

    #[test]
    fn empty_cell_budget_controls_adjacent_row_coalescing() {
        let runs = [
            SparseRun { row: 0, first_column: 0, last_column: 0 },
            SparseRun { row: 1, first_column: 0, last_column: 1 },
        ];
        let without_slack = build_sparse_regions(&runs, &[], &[], 0).unwrap();
        let with_slack = build_sparse_regions(&runs, &[], &[], 1).unwrap();

        assert_eq!(without_slack.regions.len(), 2);
        assert_eq!(without_slack.empty_cells_used, 0);
        assert_eq!(with_slack.regions.len(), 1);
        assert_eq!(with_slack.empty_cells_used, 1);
    }

    #[test]
    fn pagination_extends_a_page_instead_of_splitting_a_merge() {
        let region = SparseRegion {
            first_row: 0,
            last_row: 4_096,
            first_column: 0,
            last_column: 0,
            occupied_cells: 4_097,
            contains_merge: true,
        };
        let plain = paginate_region(region, &[]).unwrap();
        assert_eq!(
            plain.iter().map(|page| page.last_row - page.first_row + 1).collect::<Vec<_>>(),
            [2_048, 2_048, 1]
        );

        let pages = paginate_region(
            region,
            &[MergeRange { first_row: 4_095, last_row: 4_096, first_column: 0, last_column: 0 }],
        )
        .unwrap();
        assert_eq!(
            pages.iter().map(|page| page.last_row - page.first_row + 1).collect::<Vec<_>>(),
            [2_048, 2_049]
        );
    }

    #[test]
    fn merge_is_owned_wholly_by_one_region() {
        let merge_range = MergeRange { first_row: 0, last_row: 1, first_column: 0, last_column: 1 };
        let plan = build_sparse_regions(
            &[
                SparseRun { row: 0, first_column: 0, last_column: 0 },
                SparseRun { row: 1, first_column: 1, last_column: 1 },
            ],
            &[merge_range],
            &[],
            0,
        )
        .unwrap();
        assert_eq!(plan.regions.len(), 1);
        assert!(plan.regions[0].contains_merge);
        assert_eq!(
            (
                plan.regions[0].first_row,
                plan.regions[0].last_row,
                plan.regions[0].first_column,
                plan.regions[0].last_column,
            ),
            (0, 1, 0, 1)
        );
    }
}

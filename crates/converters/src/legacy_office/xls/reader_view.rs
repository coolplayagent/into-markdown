//! Offset-preserving reader adjustments after BIFF structure authentication.

use super::preflight::{Preflight, PreflightFlag, biff_record};
use super::{BOF, BOF4, ConversionError, EOF, ErrorPolicy, WORKBOOK, malformed, read_u16};
use crate::workbook::LegacyXlsHints;
use into_markdown_core::{ExecutionContext, ResourceReservation};
use std::collections::{BTreeMap, BTreeSet};

const FORMAT: u16 = 0x041e;
const XF: u16 = 0x00e0;

pub(super) fn prepare_wrapper(
    workbook: &[u8],
    preflight: &Preflight,
    container_required: bool,
    remap: &BTreeMap<u16, u16>,
    context: &ExecutionContext,
) -> Result<Option<(Vec<u8>, ResourceReservation)>, ConversionError> {
    let required = container_required
        || preflight.has(PreflightFlag::DimensionMetadata)
        || preflight.has(PreflightFlag::FormulaCacheMetadata)
        || preflight.has(PreflightFlag::NestedCharts)
        || !remap.is_empty()
        || !preflight.hints.formula_expressions.is_empty()
        || preflight.logical_end != workbook.len();
    if !required {
        return Ok(None);
    }
    let workbook = &workbook[..preflight.logical_end];
    let layout = super::cfb_wrapper_layout(workbook.len())?;
    let memory = context.reserve_memory(
        u64::try_from(layout.output_bytes.saturating_add(layout.total_sectors * 4))
            .unwrap_or(u64::MAX),
    )?;
    let mut wrapper = super::build_cfb_wrapper(
        workbook,
        preflight.has(PreflightFlag::DimensionMetadata),
        preflight.has(PreflightFlag::FormulaCacheMetadata),
        preflight.biff_version,
        layout,
    )?;
    if preflight.has(PreflightFlag::NestedCharts)
        || !remap.is_empty()
        || !preflight.hints.formula_expressions.is_empty()
    {
        patch_reader_view(
            &mut wrapper[super::CFB_SECTOR_BYTES..super::CFB_SECTOR_BYTES + workbook.len()],
            workbook,
            remap,
        )?;
    }
    Ok(Some((wrapper, memory)))
}

pub(super) fn format_id_remap(
    workbook: &[u8],
    hints: &LegacyXlsHints,
    error_policy: ErrorPolicy,
) -> Result<BTreeMap<u16, u16>, ConversionError> {
    // Calamine's BIFF8 FORMAT reader accepts the MS-XLS custom-format ranges only.
    let unsupported = hints
        .format_codes
        .keys()
        .copied()
        .filter(|index| !matches!(index, 5..=8 | 23..=26 | 41..=44 | 63..=66 | 164..=382));
    let mut remap = unsupported.take(220).map(|index| (index, 0)).collect::<BTreeMap<_, _>>();
    if remap.len() > 219 {
        return Err(ConversionError::Unsupported {
            detail: "too many non-canonical Format identifiers for available custom slots".into(),
        });
    }
    if remap.is_empty() {
        return Ok(remap);
    }
    if error_policy == ErrorPolicy::Strict {
        return Err(malformed(WORKBOOK, "non-canonical BIFF Format index"));
    }
    let mut used = hints.format_codes.keys().copied().collect::<BTreeSet<_>>();
    let mut cursor = 0;
    while cursor < workbook.len() {
        let (kind, body, end) = biff_record(workbook, cursor, WORKBOOK)?;
        if kind == EOF {
            break;
        }
        if kind == XF {
            used.insert(read_u16(body, 2, WORKBOOK)?);
        }
        cursor = end;
    }
    let mut available = (164..=382).filter(|index| !used.contains(index));
    for replacement in remap.values_mut() {
        *replacement = available.next().ok_or_else(|| ConversionError::Unsupported {
            detail: "no unused BIFF custom-format index is available for compatibility".into(),
        })?;
    }
    Ok(remap)
}

pub(super) fn patch_reader_view(
    output: &mut [u8],
    workbook: &[u8],
    remap: &BTreeMap<u16, u16>,
) -> Result<(), ConversionError> {
    let mut cursor = 0;
    let mut depth = 0_usize;
    while cursor < workbook.len() {
        let (kind, body, end) = biff_record(workbook, cursor, WORKBOOK)?;
        if matches!(kind, BOF | BOF4) {
            depth = depth
                .checked_add(1)
                .ok_or_else(|| malformed(WORKBOOK, "reader-view substream depth overflowed"))?;
        }
        if depth > 1 {
            // Nested chart caches are not worksheet cells. Hide even the chart BOF/EOF,
            // retaining framing so the reader reaches the enclosing worksheet's EOF.
            super::wrapper::put_u16(output, cursor, 0xffff)?;
        } else if kind == super::FORMULA {
            // Formula identity/text comes from the original inventory. Calamine eagerly
            // parses formula tokens on open; give that unused parser an empty token view
            // without changing the cached result, record framing, or any stream offset.
            super::wrapper::put_u16(output, cursor + 4 + 20, 0)?;
        } else if matches!(kind, FORMAT | XF) && !remap.is_empty() {
            let field = if kind == FORMAT { 0 } else { 2 };
            let index = read_u16(body, field, WORKBOOK)?;
            if let Some(replacement) = remap.get(&index) {
                super::wrapper::put_u16(output, cursor + 4 + field, *replacement)?;
            }
        }
        if kind == EOF {
            depth = depth
                .checked_sub(1)
                .ok_or_else(|| malformed(WORKBOOK, "reader-view EOF has no substream"))?;
        }
        cursor = end;
    }
    Ok(())
}

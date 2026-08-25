use super::budget::{LegacyBudget, malformed};
use super::builder::{PROVIDER_ID, locator};
use super::normalize_xls_output;
use crate::msg::ole::Storage;
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConversionOptions, ConverterOutput,
    Diagnostic, DiagnosticSeverity, ExecutionContext, NodeId, Provenance, ProvenanceKind,
};

const WORKBOOK: &str = "Workbook";
const BOF: u16 = 0x0809;
const BIFF8: u16 = 0x0600;
const FILE_PASS: u16 = 0x002f;
const DIMENSIONS: u16 = 0x0200;
const SUP_BOOK: u16 = 0x01ae;
const EXTERN_SHEET: u16 = 0x0017;

pub(super) fn convert(
    bytes: &[u8],
    root: Storage<'_>,
    budget: &mut LegacyBudget<'_>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let (part, workbook) = root
        .stream(WORKBOOK)
        .map(|stream| (WORKBOOK, stream))
        .or_else(|| root.stream("Book").map(|stream| ("Book", stream)))
        .ok_or_else(|| malformed("CFB directory", "XLS has no Workbook stream"))?;
    let preflight = preflight(workbook, part, budget)?;

    let mut output = crate::workbook::convert_legacy_xls(bytes, options, context)?;
    normalize_xls_output(&mut output);
    output.document.metadata.properties.insert("legacyOffice.xls.biff".into(), "8".into());

    if preflight.external_bindings {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.externalBindingsSkipped".into(),
            severity: DiagnosticSeverity::Warning,
            message: "external workbook bindings were retained as inert formula text and were not resolved"
                .into(),
            locator: Some(locator(part)),
        });
    }
    if root.storage("_VBA_PROJECT_CUR").is_some() || root.stream("_VBA_PROJECT_CUR").is_some() {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.macrosSkipped".into(),
            severity: DiagnosticSeverity::Warning,
            message: "VBA project data was not executed or exposed as active content".into(),
            locator: Some(locator("_VBA_PROJECT_CUR")),
        });
    }
    retain_safe_images(workbook, part, &mut output, budget)?;
    Ok(output)
}

#[derive(Default)]
struct Preflight {
    external_bindings: bool,
}

fn preflight(
    bytes: &[u8],
    part: &str,
    budget: &mut LegacyBudget<'_>,
) -> Result<Preflight, ConversionError> {
    let mut cursor = 0usize;
    let mut saw_biff8 = false;
    let mut result = Preflight::default();
    while cursor < bytes.len() {
        budget.work(1, part)?;
        let header = bytes
            .get(cursor..cursor.saturating_add(4))
            .ok_or_else(|| malformed(part, "truncated BIFF record header"))?;
        let kind = u16::from_le_bytes([header[0], header[1]]);
        let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let body_start = cursor
            .checked_add(4)
            .ok_or_else(|| malformed(part, "BIFF record offset overflowed"))?;
        let end = body_start
            .checked_add(length)
            .ok_or_else(|| malformed(part, "BIFF record length overflowed"))?;
        let body = bytes
            .get(body_start..end)
            .ok_or_else(|| malformed(part, "truncated BIFF record body"))?;
        match kind {
            BOF => {
                let version = read_u16(body, 0, part)?;
                if version != BIFF8 {
                    return Err(ConversionError::Unsupported {
                        detail: format!(
                            "XLS BIFF version 0x{version:04x} predates Office 97-2003 BIFF8"
                        ),
                    });
                }
                saw_biff8 = true;
            }
            FILE_PASS => return Err(ConversionError::Encrypted),
            DIMENSIONS => {
                if body.len() < 12 {
                    return Err(malformed(part, "truncated BIFF8 Dimensions record"));
                }
                let first_row = u64::from(read_u32(body, 0, part)?);
                let last_row = u64::from(read_u32(body, 4, part)?);
                let first_column = u64::from(read_u16(body, 8, part)?);
                let last_column = u64::from(read_u16(body, 10, part)?);
                if last_row < first_row || last_column < first_column {
                    return Err(malformed(part, "BIFF8 Dimensions range is reversed"));
                }
                budget.table_shape(
                    usize::try_from(last_row - first_row).unwrap_or(usize::MAX),
                    usize::try_from(last_column - first_column).unwrap_or(usize::MAX),
                )?;
            }
            SUP_BOOK | EXTERN_SHEET => result.external_bindings = true,
            _ => {}
        }
        cursor = end;
    }
    if !saw_biff8 {
        return Err(malformed(part, "Workbook stream has no BIFF8 BOF record"));
    }
    Ok(result)
}

fn retain_safe_images(
    bytes: &[u8],
    part: &str,
    output: &mut ConverterOutput,
    budget: &mut LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    let mut cursor = 0usize;
    let mut count = 0usize;
    while let Some((start, end, media_type)) = super::doc::find_image(&bytes[cursor..]) {
        let start = cursor + start;
        let end = cursor + end;
        budget.raster(&bytes[start..end], media_type, "Workbook/image")?;
        budget.asset(end - start, "Workbook/image")?;
        count += 1;
        let extension = if media_type == "image/png" { "png" } else { "jpg" };
        let id = AssetId(format!("legacy-xls-asset-{count}"));
        output.assets.push(Asset {
            id: id.clone(),
            filename: Some(format!("workbook-image-{count}.{extension}")),
            media_type: media_type.into(),
            bytes: bytes[start..end].to_vec(),
            external_uri: None,
        });
        output.document.blocks.push(BlockNode {
            id: NodeId(format!("legacy-xls-image-{count}")),
            block: Block::Image { asset: id, alt: None },
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: PROVIDER_ID.into(),
                locator: locator(part),
                confidence: None,
            },
        });
        cursor = end;
    }
    if count > 0 {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.imagePlacementRecovered".into(),
            severity: DiagnosticSeverity::Warning,
            message: "safe embedded image payloads were retained in workbook stream order because drawing anchors were incomplete"
                .into(),
            locator: Some(locator(part)),
        });
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize, part: &str) -> Result<u16, ConversionError> {
    let raw = bytes.get(offset..offset + 2).ok_or_else(|| malformed(part, "truncated BIFF u16"))?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn read_u32(bytes: &[u8], offset: usize, part: &str) -> Result<u32, ConversionError> {
    let raw = bytes.get(offset..offset + 4).ok_or_else(|| malformed(part, "truncated BIFF u32"))?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits};

    fn budget<'a>(
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> LegacyBudget<'a> {
        LegacyBudget::new(64, options, context).unwrap()
    }

    #[test]
    fn rejects_pre_biff8_and_filepass() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut old = vec![0x09, 0x08, 4, 0, 0x00, 0x05, 0, 0];
        assert!(matches!(
            preflight(&old, WORKBOOK, &mut budget(&options, &context)),
            Err(ConversionError::Unsupported { .. })
        ));
        old[4..6].copy_from_slice(&BIFF8.to_le_bytes());
        old.extend_from_slice(&[0x2f, 0, 0, 0]);
        assert!(matches!(
            preflight(&old, WORKBOOK, &mut budget(&options, &context)),
            Err(ConversionError::Encrypted)
        ));
    }

    #[test]
    fn dimensions_use_table_resource_limits() {
        let limits = ResourceLimits { max_table_rows: 10, ..ResourceLimits::default() };
        let options = ConversionOptions { limits, ..ConversionOptions::default() };
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut bytes = vec![0x09, 0x08, 4, 0, 0, 6, 0, 0];
        bytes.extend_from_slice(&[0x00, 0x02, 12, 0]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        assert!(matches!(
            preflight(&bytes, WORKBOOK, &mut budget(&options, &context)),
            Err(ConversionError::ResourceLimit { limit: "max_table_rows", .. })
        ));
    }
}

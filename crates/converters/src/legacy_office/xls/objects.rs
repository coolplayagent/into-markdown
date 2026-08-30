use super::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConverterOutput, Diagnostic,
    DiagnosticSeverity, LegacyBudget, NodeId, PROVIDER_ID, Provenance, ProvenanceKind, locator,
};

pub(super) fn retain_safe_images(
    bytes: &[u8],
    part: &str,
    output: &mut ConverterOutput,
    budget: &mut LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    let mut cursor = 0usize;
    let mut count = 0usize;
    while let Some((start, end, media_type)) = super::super::doc::find_image(&bytes[cursor..]) {
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

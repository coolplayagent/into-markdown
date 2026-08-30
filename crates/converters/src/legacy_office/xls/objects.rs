use super::preflight::biff_record;
use super::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConverterOutput, Diagnostic,
    DiagnosticSeverity, ErrorPolicy, LegacyBudget, MSO_DRAWING, MSO_DRAWING_GROUP, NodeId,
    PROVIDER_ID, Provenance, ProvenanceKind, locator, malformed, read_u16, read_u32,
};

const ESCHER_CONTAINER_VERSION: u16 = 0x000f;
const ESCHER_BLIP_JPEG: u16 = 0xf01d;
const ESCHER_BLIP_PNG: u16 = 0xf01e;

pub(super) fn retain_safe_images(
    bytes: &[u8],
    part: &str,
    output: &mut ConverterOutput,
    budget: &mut LegacyBudget<'_>,
    error_policy: ErrorPolicy,
) -> Result<(), ConversionError> {
    let mut cursor = 0_usize;
    let mut images = Vec::<(&[u8], &'static str)>::new();
    let mut media_failure = None;
    while cursor < bytes.len() {
        budget.work(1, part)?;
        let (kind, body, end) = biff_record(bytes, cursor, part)?;
        if matches!(kind, MSO_DRAWING | MSO_DRAWING_GROUP)
            && let Err(error) = collect_blips(body, 0, budget, &mut images)
        {
            if error_policy == ErrorPolicy::Strict {
                return Err(error);
            }
            media_failure.get_or_insert_with(|| error.to_string());
        }
        cursor = end;
    }

    for (index, (image, media_type)) in images.into_iter().enumerate() {
        let image_part = format!("{part}/drawing/blip-{}", index + 1);
        if let Err(error) = budget.raster(image, media_type, &image_part) {
            if error_policy == ErrorPolicy::Strict {
                return Err(error);
            }
            media_failure.get_or_insert_with(|| error.to_string());
            continue;
        }
        budget.asset(image.len(), &image_part)?;
        let extension = if media_type == "image/png" { "png" } else { "jpg" };
        let id = AssetId(format!("legacy-xls-asset-{}", index + 1));
        output.assets.push(Asset {
            id: id.clone(),
            filename: Some(format!("workbook-image-{}.{}", index + 1, extension)),
            media_type: media_type.into(),
            bytes: image.to_vec(),
            external_uri: None,
        });
        output.document.blocks.push(BlockNode {
            id: NodeId(format!("legacy-xls-image-{}", index + 1)),
            block: Block::Image { asset: id, alt: None },
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: PROVIDER_ID.into(),
                locator: locator(part),
                confidence: None,
            },
        });
    }
    if !output.assets.is_empty() {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.imagePlacementRecovered".into(),
            severity: DiagnosticSeverity::Warning,
            message: "authenticated drawing BLIP payloads were retained in workbook stream order because drawing anchors were incomplete"
                .into(),
            locator: Some(locator(part)),
        });
    }
    if let Some(detail) = media_failure {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.embeddedMediaPlaceholder".into(),
            severity: DiagnosticSeverity::Warning,
            message: format!(
                "an authenticated drawing payload could not be safely retained and remains an inert placeholder: {detail}"
            ),
            locator: Some(locator(part)),
        });
    }
    Ok(())
}

fn collect_blips<'a>(
    bytes: &'a [u8],
    depth: u16,
    budget: &mut LegacyBudget<'_>,
    output: &mut Vec<(&'a [u8], &'static str)>,
) -> Result<(), ConversionError> {
    budget.depth(depth, "Workbook/drawing")?;
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        budget.work(1, "Workbook/drawing")?;
        if bytes.len() - cursor < 8 {
            return Err(malformed("Workbook/drawing", "truncated Escher record header"));
        }
        let options = read_u16(bytes, cursor, "Workbook/drawing")?;
        let kind = read_u16(bytes, cursor + 2, "Workbook/drawing")?;
        let length = usize::try_from(read_u32(bytes, cursor + 4, "Workbook/drawing")?)
            .map_err(|_| malformed("Workbook/drawing", "Escher record length overflowed"))?;
        let body_start = cursor
            .checked_add(8)
            .ok_or_else(|| malformed("Workbook/drawing", "Escher body offset overflowed"))?;
        let end = body_start
            .checked_add(length)
            .ok_or_else(|| malformed("Workbook/drawing", "Escher record length overflowed"))?;
        let body = bytes
            .get(body_start..end)
            .ok_or_else(|| malformed("Workbook/drawing", "truncated Escher record body"))?;
        if options & 0x000f == ESCHER_CONTAINER_VERSION {
            collect_blips(body, depth.saturating_add(1), budget, output)?;
        } else if matches!(kind, ESCHER_BLIP_JPEG | ESCHER_BLIP_PNG) {
            let (signature, media_type) = if kind == ESCHER_BLIP_PNG {
                (b"\x89PNG\r\n\x1a\n".as_slice(), "image/png")
            } else {
                (b"\xff\xd8\xff".as_slice(), "image/jpeg")
            };
            let start = body
                .windows(signature.len())
                .position(|window| window == signature)
                .ok_or_else(|| malformed("Workbook/drawing", "BLIP signature is missing"))?;
            output.push((&body[start..], media_type));
        }
        cursor = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy_office::xls::wrapper::push_biff_record;
    use image::{ExtendedColorType, ImageEncoder as _, codecs::png::PngEncoder};
    use into_markdown_core::{ConversionOptions, Document, ExecutionContext, ExecutionOptions};

    fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(&[0, 0, 0, 0], 1, 1, ExtendedColorType::Rgba8)
            .unwrap();
        bytes
    }

    fn drawing_record(image: &[u8]) -> Vec<u8> {
        let mut escher = Vec::new();
        escher.extend_from_slice(&0_u16.to_le_bytes());
        escher.extend_from_slice(&ESCHER_BLIP_PNG.to_le_bytes());
        escher.extend_from_slice(&u32::try_from(image.len()).unwrap().to_le_bytes());
        escher.extend_from_slice(image);
        let mut workbook = Vec::new();
        push_biff_record(&mut workbook, MSO_DRAWING, &escher).unwrap();
        workbook
    }

    fn retain(bytes: &[u8], policy: ErrorPolicy) -> Result<ConverterOutput, ConversionError> {
        let options = ConversionOptions { error_policy: policy, ..ConversionOptions::default() };
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut budget = LegacyBudget::new(bytes.len(), &options, &context).unwrap();
        let mut output = ConverterOutput::new(Document::default(), Vec::new(), Vec::new());
        retain_safe_images(bytes, "Workbook", &mut output, &mut budget, policy)?;
        Ok(output)
    }

    #[test]
    fn image_signatures_require_authenticated_drawing_and_blip_records() {
        let image = png();
        let mut unrelated = Vec::new();
        push_biff_record(&mut unrelated, 0x1337, &image).unwrap();
        let ignored = retain(&unrelated, ErrorPolicy::Strict).unwrap();
        assert!(ignored.assets.is_empty());

        let retained = retain(&drawing_record(&image), ErrorPolicy::Strict).unwrap();
        assert_eq!(retained.assets.len(), 1);
        assert_eq!(retained.assets[0].bytes, image);
    }

    #[test]
    fn malformed_authenticated_blip_is_a_best_effort_placeholder() {
        let malformed = drawing_record(b"\x89PNG\r\n\x1a\n");
        assert!(retain(&malformed, ErrorPolicy::Strict).is_err());
        let recovered = retain(&malformed, ErrorPolicy::BestEffort).unwrap();
        assert!(recovered.assets.is_empty());
        assert!(
            recovered
                .diagnostics
                .iter()
                .any(|item| item.code == "legacyOffice.xls.embeddedMediaPlaceholder")
        );
    }
}

use into_markdown_core::{
    Block, BlockNode, ConversionError, ConverterOutput, Inline, InputFormat, SourceLocator,
};
use into_markdown_legacy_office::NormalizedFormat;

const PROVIDER_ID: &str = "builtin.converter.legacy-office";

pub(super) fn remap(
    output: &mut ConverterOutput,
    source: InputFormat,
    normalized: NormalizedFormat,
    version: &str,
    artifact_sha256: &str,
    target: &str,
) -> Result<(), ConversionError> {
    for block in &mut output.document.blocks {
        remap_block(block, source)?;
    }
    for diagnostic in &mut output.diagnostics {
        if let Some(locator) = &mut diagnostic.locator {
            remap_locator(locator, source)?;
        }
    }
    let properties = &mut output.document.metadata.properties;
    insert(properties, "legacyOffice.runtime.product", "LibreOffice")?;
    insert(properties, "legacyOffice.runtime.version", version)?;
    insert(properties, "legacyOffice.runtime.artifactSha256", artifact_sha256)?;
    insert(properties, "legacyOffice.runtime.target", target)?;
    insert(properties, "legacyOffice.sourceFormat", source.as_str())?;
    insert(properties, "legacyOffice.normalizedFormat", normalized.input_format().as_str())?;
    Ok(())
}

fn remap_block(block: &mut BlockNode, source: InputFormat) -> Result<(), ConversionError> {
    remap_provenance(&mut block.provenance, source)?;
    match &mut block.block {
        Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. }
        | Block::Footnote { blocks, .. } => {
            for child in blocks {
                remap_block(child, source)?;
            }
        }
        Block::List { items, .. } => {
            for item in items {
                for child in &mut item.blocks {
                    remap_block(child, source)?;
                }
            }
        }
        Block::Table { rows, .. } => {
            for row in rows {
                for cell in &mut row.cells {
                    for child in &mut cell.blocks {
                        remap_block(child, source)?;
                    }
                }
            }
        }
        Block::Paragraph(inlines)
        | Block::Heading { content: inlines, .. }
        | Block::TimedSegment { content: inlines, .. } => remap_inlines(inlines, source)?,
        _ => {}
    }
    Ok(())
}

fn remap_inlines(inlines: &mut [Inline], source: InputFormat) -> Result<(), ConversionError> {
    for inline in inlines {
        match inline {
            Inline::SourceText { provenance, .. } | Inline::OcrText { provenance, .. } => {
                remap_provenance(provenance, source)?;
            }
            Inline::Link { content, .. } => remap_inlines(content, source)?,
            _ => {}
        }
    }
    Ok(())
}

fn remap_provenance(
    provenance: &mut into_markdown_core::Provenance,
    source: InputFormat,
) -> Result<(), ConversionError> {
    provenance.provider = format!("{PROVIDER_ID}>{}", provenance.provider);
    remap_locator(&mut provenance.locator, source)
}

fn remap_locator(locator: &mut SourceLocator, source: InputFormat) -> Result<(), ConversionError> {
    let prefix = format!("legacy-office/{}/", source.as_str());
    locator.part = Some(match locator.part.take() {
        Some(part) if safe_part(&part) => format!("{prefix}{part}"),
        Some(_) => return Err(internal("normalized converter returned an unsafe source part")),
        None => prefix.trim_end_matches('/').to_owned(),
    });
    // The runtime rewrites the compound document into a new ZIP package. Its
    // byte coordinates cannot truthfully address the original OLE bytes.
    locator.byte_start = None;
    locator.byte_end = None;
    Ok(())
}

fn safe_part(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 4_096
        && part.is_ascii()
        && !part.starts_with('/')
        && !part.contains(['\\', '\0'])
        && part.split('/').all(|component| !matches!(component, "" | "." | ".."))
}

fn insert(
    properties: &mut std::collections::BTreeMap<String, String>,
    key: &str,
    value: &str,
) -> Result<(), ConversionError> {
    if properties.insert(key.into(), value.into()).is_some() {
        return Err(internal("normalized converter forged legacy Office runtime metadata"));
    }
    Ok(())
}

fn internal(detail: &'static str) -> ConversionError {
    ConversionError::Internal { detail: detail.into() }
}

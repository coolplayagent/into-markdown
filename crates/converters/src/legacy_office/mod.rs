//! Bounded, offline conversion for Office 97-2003 compound documents.
//!
//! The converter shares the audited CFB reader with MSG, then dispatches to
//! format-specific parsers. It never launches a process, evaluates macros or
//! formulae, follows external links, or performs network access.

mod budget;
mod builder;
mod doc;
mod ppt;
mod xls;

use crate::msg::ole::CompoundFile;
use budget::{LegacyBudget, malformed};
use builder::PROVIDER_ID;
use into_markdown_core::{
    Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput,
    Diagnostic, DiagnosticSeverity, ExecutionContext, FormatCandidate, InputFormat, ProbeOutcome,
    ProvenanceKind, ResolvedInput, Services, SourceLocator,
};

const FORMATS: &[InputFormat] = &[InputFormat::Doc, InputFormat::Ppt, InputFormat::Xls];
const CFB_MAGIC: &[u8; 8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";

/// Native converter for DOC, PPT/PPS/POT, and XLS compound documents.
#[derive(Debug, Default)]
pub struct LegacyOfficeConverter;

impl Converter for LegacyOfficeConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        230
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        FORMATS
    }

    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            if !FORMATS.contains(&candidate.format) || !input.bytes.starts_with(CFB_MAGIC) {
                return Ok(ProbeOutcome::NotApplicable);
            }
            Ok(ProbeOutcome::Match { confidence: 1.0 })
        })
    }

    fn planned_output_bytes(
        &self,
        _: &ResolvedInput,
        _: &FormatCandidate,
        _: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        Ok(context.available_memory_bytes())
    }

    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                convert_native(&input.bytes, candidate.format, options, context)
            }))
            .unwrap_or_else(|_| {
                Err(malformed("legacy Office", "native parser rejected invalid structure"))
            })
        })
    }
}

fn convert_native(
    bytes: &[u8],
    requested: InputFormat,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    if !FORMATS.contains(&requested) {
        return Err(ConversionError::Unsupported {
            detail: "legacy Office converter accepts only DOC, PPT/PPS/POT, or XLS".into(),
        });
    }
    let mut budget = LegacyBudget::new(bytes.len(), options, context)?;
    let compound = CompoundFile::open(bytes, &mut budget)?;
    let root = compound.root();
    let detected = detect_family(root)?;
    if detected != requested {
        return Err(ConversionError::Unsupported {
            detail: format!(
                "explicit legacy Office format {requested:?} conflicts with detected {detected:?}"
            ),
        });
    }
    match detected {
        InputFormat::Doc => doc::convert(root, &mut budget),
        InputFormat::Ppt => ppt::convert(root, &mut budget),
        InputFormat::Xls => xls::convert(bytes, root, &mut budget, options, context),
        _ => unreachable!("family detector returns only legacy Office formats"),
    }
}

pub(super) fn normalize_xls_output(output: &mut ConverterOutput) {
    output.document.metadata.properties.insert("legacyOffice.family".into(), "xls".into());
    output
        .document
        .metadata
        .properties
        .insert("legacyOffice.parser".into(), "into-markdown-native".into());
    rewrite_provenance(&mut output.document.blocks);
    output.diagnostics.push(Diagnostic {
        code: "legacyOffice.xls.inertObjectsSkipped".into(),
        severity: DiagnosticSeverity::Warning,
        message:
            "macros, external workbook bindings, and executable embedded objects were not executed"
                .into(),
        locator: Some(SourceLocator { part: Some("Workbook".into()), ..SourceLocator::default() }),
    });
}

fn rewrite_provenance(nodes: &mut [BlockNode]) {
    for node in nodes {
        node.provenance.kind = ProvenanceKind::NativeParser;
        node.provenance.provider = PROVIDER_ID.into();
        match &mut node.block {
            Block::List { items, .. } => {
                for item in items {
                    rewrite_provenance(&mut item.blocks);
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &mut row.cells {
                        rewrite_provenance(&mut cell.blocks);
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => rewrite_provenance(blocks),
            _ => {}
        }
    }
}

fn detect_family(root: crate::msg::ole::Storage<'_>) -> Result<InputFormat, ConversionError> {
    let word = root.stream("WordDocument").is_some();
    let powerpoint = root.stream("PowerPoint Document").is_some();
    let workbook = root.stream("Workbook").is_some() || root.stream("Book").is_some();
    let count = u8::from(word) + u8::from(powerpoint) + u8::from(workbook);
    if count != 1 {
        return Err(malformed(
            "CFB directory",
            "compound document must identify exactly one DOC, PPT, or XLS family",
        ));
    }
    Ok(if word {
        InputFormat::Doc
    } else if powerpoint {
        InputFormat::Ppt
    } else {
        InputFormat::Xls
    })
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits};

    const DOC: &[u8] = include_bytes!("../../../../tools/macos-release/fixtures/normal.doc");
    const PPT: &[u8] = include_bytes!("../../../../tools/macos-release/fixtures/normal.ppt");
    const XLS: &[u8] = include_bytes!("../../../../tools/macos-release/fixtures/normal.xls");

    fn convert(bytes: &[u8], format: InputFormat) -> ConverterOutput {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        convert_native(bytes, format, &options, &context).unwrap()
    }

    fn encoded(bytes: &[u8], format: InputFormat) -> Vec<u8> {
        let output = convert(bytes, format);
        serde_json::to_vec(&(output.document, output.assets, output.diagnostics)).unwrap()
    }

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn sector_offset(bytes: &[u8], sector: u32) -> usize {
        let sector_size = 1usize << u16::from_le_bytes(bytes[30..32].try_into().unwrap());
        (usize::try_from(sector).unwrap() + 1) * sector_size
    }

    fn directory_entry(bytes: &[u8], name: &str) -> usize {
        let encoded = name.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
        bytes
            .windows(encoded.len())
            .enumerate()
            .find(|(offset, window)| offset % 128 == 0 && *window == encoded)
            .map(|(offset, _)| offset)
            .unwrap()
    }

    fn word_stream_offset(bytes: &[u8]) -> usize {
        bytes
            .windows(4)
            .position(|window| {
                window[..2] == [0xec, 0xa5]
                    && u16::from_le_bytes(window[2..4].try_into().unwrap()) >= 0x00c1
            })
            .unwrap()
    }

    #[test]
    fn repository_office_corpus_converts_without_optional_services() {
        let doc = convert(DOC, InputFormat::Doc);
        assert_eq!(doc.document.blocks.len(), 1);
        assert!(doc.assets.is_empty());

        let ppt = convert(PPT, InputFormat::Ppt);
        assert_eq!(
            ppt.document
                .blocks
                .iter()
                .filter(|node| matches!(node.block, Block::Slide { .. }))
                .count(),
            2
        );
        assert_eq!(ppt.assets.len(), 1);
        assert_eq!(ppt.assets[0].media_type, "image/png");
        assert!(ppt.diagnostics.iter().all(|item| !item.code.is_empty()));

        let xls = convert(XLS, InputFormat::Xls);
        assert!(matches!(xls.document.blocks[0].block, Block::Sheet { .. }));
        assert!(xls.document.blocks.iter().all(|node| {
            node.provenance.provider == PROVIDER_ID
                && node.provenance.kind == ProvenanceKind::NativeParser
        }));
    }

    #[test]
    fn serial_and_concurrent_outputs_are_byte_deterministic() {
        for (bytes, format) in
            [(DOC, InputFormat::Doc), (PPT, InputFormat::Ppt), (XLS, InputFormat::Xls)]
        {
            let expected = encoded(bytes, format);
            for _ in 0..4 {
                assert_eq!(encoded(bytes, format), expected);
            }
            std::thread::scope(|scope| {
                let handles =
                    (0..4).map(|_| scope.spawn(|| encoded(bytes, format))).collect::<Vec<_>>();
                for handle in handles {
                    assert_eq!(handle.join().unwrap(), expected);
                }
            });
        }
    }

    #[test]
    fn format_confusion_and_cfb_corruption_have_stable_errors() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        assert!(matches!(
            convert_native(DOC, InputFormat::Ppt, &options, &context),
            Err(ConversionError::Unsupported { .. })
        ));

        let mut truncated = DOC[..DOC.len() - 1].to_vec();
        assert!(matches!(
            convert_native(&truncated, InputFormat::Doc, &options, &context),
            Err(ConversionError::Malformed { .. })
        ));
        truncated[0] = 0;
        assert!(matches!(
            convert_native(&truncated, InputFormat::Doc, &options, &context),
            Err(ConversionError::Malformed { .. })
        ));
    }

    #[test]
    fn cfb_cycles_overlaps_and_entry_limits_fail_closed() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());

        let mut cycle = DOC.to_vec();
        let word_entry = directory_entry(&cycle, "WordDocument");
        let word_sector = read_u32(&cycle, word_entry + 116);
        let minifat_sector = read_u32(&cycle, 60);
        let minifat_entry =
            sector_offset(&cycle, minifat_sector) + usize::try_from(word_sector).unwrap() * 4;
        cycle[minifat_entry..minifat_entry + 4].copy_from_slice(&word_sector.to_le_bytes());
        assert!(matches!(
            convert_native(&cycle, InputFormat::Doc, &options, &context),
            Err(ConversionError::Malformed { .. })
        ));

        let mut overlap = DOC.to_vec();
        let word_offset = word_stream_offset(&overlap);
        let flags =
            u16::from_le_bytes(overlap[word_offset + 10..word_offset + 12].try_into().unwrap());
        let table_entry =
            directory_entry(&overlap, if flags & 0x0200 == 0 { "0Table" } else { "1Table" });
        overlap[table_entry + 116..table_entry + 120].copy_from_slice(&word_sector.to_le_bytes());
        assert!(matches!(
            convert_native(&overlap, InputFormat::Doc, &options, &context),
            Err(ConversionError::Malformed { .. })
        ));

        let limits = ResourceLimits { max_archive_entries: 1, ..ResourceLimits::default() };
        let limited = ConversionOptions { limits, ..ConversionOptions::default() };
        let limited_context =
            ExecutionContext::new(ExecutionOptions::default(), limited.limits.clone());
        assert!(matches!(
            convert_native(DOC, InputFormat::Doc, &limited, &limited_context),
            Err(ConversionError::ResourceLimit { limit: "max_archive_entries", .. })
        ));
    }

    #[test]
    fn doc_encryption_and_pre_office_97_are_stable_errors() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut encrypted = DOC.to_vec();
        let word = word_stream_offset(&encrypted);
        let flags =
            u16::from_le_bytes(encrypted[word + 10..word + 12].try_into().unwrap()) | 0x0100;
        encrypted[word + 10..word + 12].copy_from_slice(&flags.to_le_bytes());
        assert!(matches!(
            convert_native(&encrypted, InputFormat::Doc, &options, &context),
            Err(ConversionError::Encrypted)
        ));

        let mut old = DOC.to_vec();
        old[word + 2..word + 4].copy_from_slice(&0x00c0u16.to_le_bytes());
        assert!(matches!(
            convert_native(&old, InputFormat::Doc, &options, &context),
            Err(ConversionError::Unsupported { .. })
        ));
    }
}

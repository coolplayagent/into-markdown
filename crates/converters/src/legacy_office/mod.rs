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

use crate::msg::ole::{CompoundCompatibility, CompoundFile, CompoundRecovery};
use budget::{LegacyBudget, malformed};
use builder::PROVIDER_ID;
use into_markdown_core::{
    Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput,
    Diagnostic, DiagnosticSeverity, ErrorPolicy, ExecutionContext, FormatCandidate, InputFormat,
    ProbeOutcome, ProvenanceKind, ResolvedInput, Services, SourceLocator,
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
            if !FORMATS.contains(&candidate.format) {
                return Ok(ProbeOutcome::NotApplicable);
            }
            let compound = input.bytes.starts_with(CFB_MAGIC);
            let raw_xls =
                candidate.format == InputFormat::Xls && xls::looks_like_raw_biff(&input.bytes);
            if !compound && !raw_xls {
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
    if requested == InputFormat::Xls && xls::looks_like_raw_biff(bytes) {
        return xls::convert_raw(bytes, &mut budget, options, context);
    }
    let compatibility =
        if requested == InputFormat::Xls && options.error_policy == ErrorPolicy::BestEffort {
            CompoundCompatibility::LegacyOfficeBestEffort
        } else {
            CompoundCompatibility::Strict
        };
    let compound = CompoundFile::open_with_compatibility(bytes, &mut budget, compatibility)?;
    let compound_recoveries = compound.recoveries().collect::<Vec<_>>();
    let container_view_required = !compound_recoveries.is_empty();
    let root = compound.root();
    let detected = detect_family(root)?;
    if detected != requested {
        return Err(ConversionError::Unsupported {
            detail: format!(
                "explicit legacy Office format {requested:?} conflicts with detected {detected:?}"
            ),
        });
    }
    let mut output = match detected {
        InputFormat::Doc => doc::convert(root, &mut budget),
        InputFormat::Ppt => ppt::convert(root, &mut budget),
        InputFormat::Xls => {
            xls::convert(bytes, root, &mut budget, options, context, container_view_required)
        }
        _ => unreachable!("family detector returns only legacy Office formats"),
    }?;
    for recovery in compound_recoveries {
        output.diagnostics.push(compound_recovery_diagnostic(recovery));
    }
    Ok(output)
}

fn compound_recovery_diagnostic(recovery: CompoundRecovery) -> Diagnostic {
    let severity = if recovery == CompoundRecovery::StorageStreamMetadata {
        DiagnosticSeverity::Warning
    } else {
        DiagnosticSeverity::Info
    };
    let (code, message, part) = match recovery {
        CompoundRecovery::TrailingFileBytes => (
            "legacyOffice.cfb.trailingBytesIgnored",
            "unaddressable bytes after the final complete CFB sector were ignored",
            "cfb/header",
        ),
        CompoundRecovery::FatSectorMarker => (
            "legacyOffice.cfb.fatMarkerRecovered",
            "a FAT sector marker disagreed with the bounded DIFAT and was recovered",
            "cfb/fat",
        ),
        CompoundRecovery::UnreachableFatTarget => (
            "legacyOffice.cfb.unreachableFatTargetIgnored",
            "an out-of-bounds FAT target belonging to no reachable chain was ignored",
            "cfb/fat",
        ),
        CompoundRecovery::DirectoryNameTerminator => (
            "legacyOffice.cfb.directoryNameRecovered",
            "a directory name used a non-canonical but unambiguous NUL terminator",
            "cfb/directory",
        ),
        CompoundRecovery::RootStorageName => (
            "legacyOffice.cfb.rootNameRecovered",
            "the type-5 root storage used a non-canonical display name",
            "cfb/directory",
        ),
        CompoundRecovery::StorageStreamMetadata => (
            "legacyOffice.cfb.storageMetadataIgnored",
            "stream-only metadata on a storage directory entry was ignored",
            "cfb/directory",
        ),
        CompoundRecovery::StreamChainTail => (
            "legacyOffice.cfb.streamTailIgnored",
            "a stale allocation pointer after the declared end of a complete stream was ignored",
            "cfb/stream",
        ),
        CompoundRecovery::PartialStreamSector => (
            "legacyOffice.cfb.partialStreamSectorRecovered",
            "the available prefix of a terminal partial sector satisfied the declared stream size",
            "cfb/stream",
        ),
    };
    Diagnostic {
        code: code.into(),
        severity,
        message: message.into(),
        locator: Some(SourceLocator { part: Some(part.into()), ..Default::default() }),
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

    fn options(policy: ErrorPolicy) -> ConversionOptions {
        ConversionOptions { error_policy: policy, ..ConversionOptions::default() }
    }

    fn convert_with_options(
        bytes: &[u8],
        format: InputFormat,
        options: &ConversionOptions,
    ) -> Result<ConverterOutput, ConversionError> {
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        convert_native(bytes, format, options, &context)
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
    fn xls_best_effort_recovers_only_redundant_cfb_metadata() {
        let strict = options(ErrorPolicy::Strict);
        let best_effort = options(ErrorPolicy::BestEffort);

        let mut storage_metadata = XLS.to_vec();
        let comp_obj = directory_entry(&storage_metadata, "\u{1}CompObj");
        storage_metadata[comp_obj + 66] = 1;
        assert!(matches!(
            convert_with_options(&storage_metadata, InputFormat::Xls, &strict),
            Err(ConversionError::Malformed { .. })
        ));
        let recovered =
            convert_with_options(&storage_metadata, InputFormat::Xls, &best_effort).unwrap();
        assert!(recovered.diagnostics.iter().any(|item| {
            item.code == "legacyOffice.cfb.storageMetadataIgnored"
                && item.severity == DiagnosticSeverity::Warning
        }));
        assert!(matches!(
            convert_with_options(&storage_metadata, InputFormat::Doc, &best_effort),
            Err(ConversionError::Malformed { .. })
        ));
        assert!(matches!(
            convert_with_options(&storage_metadata, InputFormat::Doc, &strict),
            Err(ConversionError::Malformed { .. })
        ));

        let mut name_terminator = XLS.to_vec();
        let comp_obj = directory_entry(&name_terminator, "\u{1}CompObj");
        let declared =
            u16::from_le_bytes(name_terminator[comp_obj + 64..comp_obj + 66].try_into().unwrap());
        name_terminator[comp_obj + 64..comp_obj + 66]
            .copy_from_slice(&(declared + 2).to_le_bytes());
        assert!(matches!(
            convert_with_options(&name_terminator, InputFormat::Xls, &strict),
            Err(ConversionError::Malformed { .. })
        ));
        let recovered =
            convert_with_options(&name_terminator, InputFormat::Xls, &best_effort).unwrap();
        assert!(recovered.diagnostics.iter().any(|item| {
            item.code == "legacyOffice.cfb.directoryNameRecovered"
                && item.severity == DiagnosticSeverity::Info
        }));

        let mut trailing_bytes = XLS.to_vec();
        trailing_bytes.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
        assert!(matches!(
            convert_with_options(&trailing_bytes, InputFormat::Xls, &strict),
            Err(ConversionError::Malformed { .. })
        ));
        let recovered =
            convert_with_options(&trailing_bytes, InputFormat::Xls, &best_effort).unwrap();
        assert!(recovered.diagnostics.iter().any(|item| {
            item.code == "legacyOffice.cfb.trailingBytesIgnored"
                && item.severity == DiagnosticSeverity::Info
        }));

        let mut fat_marker = XLS.to_vec();
        let fat_sector = read_u32(&fat_marker, 76);
        let marker =
            sector_offset(&fat_marker, fat_sector) + usize::try_from(fat_sector).unwrap() * 4;
        fat_marker[marker..marker + 4].copy_from_slice(&0xffff_fffe_u32.to_le_bytes());
        assert!(matches!(
            convert_with_options(&fat_marker, InputFormat::Xls, &strict),
            Err(ConversionError::Malformed { .. })
        ));
        let recovered = convert_with_options(&fat_marker, InputFormat::Xls, &best_effort).unwrap();
        assert!(
            recovered
                .diagnostics
                .iter()
                .any(|item| item.code == "legacyOffice.cfb.fatMarkerRecovered")
        );

        let mut stale_fat = XLS.to_vec();
        let fat_sector = read_u32(&stale_fat, 76);
        let unused_marker = sector_offset(&stale_fat, fat_sector) + 4;
        let out_of_bounds = u32::try_from(stale_fat.len() / 512 + 10).unwrap();
        stale_fat[unused_marker..unused_marker + 4].copy_from_slice(&out_of_bounds.to_le_bytes());
        assert!(matches!(
            convert_with_options(&stale_fat, InputFormat::Xls, &strict),
            Err(ConversionError::Malformed { .. })
        ));
        let recovered = convert_with_options(&stale_fat, InputFormat::Xls, &best_effort).unwrap();
        assert!(
            recovered
                .diagnostics
                .iter()
                .any(|item| item.code == "legacyOffice.cfb.unreachableFatTargetIgnored")
        );

        let mut root_name = XLS.to_vec();
        let root = sector_offset(&root_name, read_u32(&root_name, 48));
        root_name[root..root + 64].fill(0);
        root_name[root..root + 2].copy_from_slice(&('R' as u16).to_le_bytes());
        root_name[root + 64..root + 66].copy_from_slice(&2_u16.to_le_bytes());
        assert!(matches!(
            convert_with_options(&root_name, InputFormat::Xls, &strict),
            Err(ConversionError::Malformed { .. })
        ));
        let recovered = convert_with_options(&root_name, InputFormat::Xls, &best_effort).unwrap();
        assert!(
            recovered
                .diagnostics
                .iter()
                .any(|item| item.code == "legacyOffice.cfb.rootNameRecovered")
        );
    }

    #[test]
    fn xls_best_effort_rejects_disguised_and_ambiguous_workbook_names() {
        let best_effort = options(ErrorPolicy::BestEffort);
        let mut disguised_workbook = XLS.to_vec();
        let workbook = directory_entry(&disguised_workbook, "Workbook");
        disguised_workbook[workbook + 18..workbook + 26]
            .copy_from_slice(&[b'E', 0, b'v', 0, b'i', 0, b'l', 0]);
        disguised_workbook[workbook + 64..workbook + 66].copy_from_slice(&26_u16.to_le_bytes());
        assert!(matches!(
            convert_with_options(&disguised_workbook, InputFormat::Xls, &best_effort),
            Err(ConversionError::Malformed { .. })
        ));

        let mut ambiguous_alias = XLS.to_vec();
        let comp_obj = directory_entry(&ambiguous_alias, "\u{1}CompObj");
        ambiguous_alias[comp_obj..comp_obj + 64].fill(0);
        for (index, unit) in "Book".encode_utf16().chain([0]).enumerate() {
            ambiguous_alias[comp_obj + index * 2..comp_obj + index * 2 + 2]
                .copy_from_slice(&unit.to_le_bytes());
        }
        ambiguous_alias[comp_obj + 64..comp_obj + 66].copy_from_slice(&10_u16.to_le_bytes());
        assert!(matches!(
            convert_with_options(&ambiguous_alias, InputFormat::Xls, &best_effort),
            Err(ConversionError::Malformed { .. })
        ));
    }

    #[test]
    fn xls_best_effort_still_rejects_mini_chain_cycles_and_overlaps() {
        let best_effort = options(ErrorPolicy::BestEffort);
        let mut cycle = XLS.to_vec();
        let workbook_entry = directory_entry(&cycle, "Workbook");
        let workbook_sector = read_u32(&cycle, workbook_entry + 116);
        let minifat_sector = read_u32(&cycle, 60);
        let minifat_entry =
            sector_offset(&cycle, minifat_sector) + usize::try_from(workbook_sector).unwrap() * 4;
        cycle[minifat_entry..minifat_entry + 4].copy_from_slice(&workbook_sector.to_le_bytes());
        assert!(matches!(
            convert_with_options(&cycle, InputFormat::Xls, &best_effort),
            Err(ConversionError::Malformed { .. })
        ));

        let mut overlap = XLS.to_vec();
        let comp_obj = directory_entry(&overlap, "\u{1}CompObj");
        overlap[comp_obj + 116..comp_obj + 120].copy_from_slice(&workbook_sector.to_le_bytes());
        assert!(matches!(
            convert_with_options(&overlap, InputFormat::Xls, &best_effort),
            Err(ConversionError::Malformed { .. })
        ));
    }

    #[test]
    fn doc_and_ppt_keep_strict_cfb_validation_in_best_effort() {
        let best_effort = options(ErrorPolicy::BestEffort);
        for (fixture, format) in [(DOC, InputFormat::Doc), (PPT, InputFormat::Ppt)] {
            let mut storage_metadata = fixture.to_vec();
            let comp_obj = directory_entry(&storage_metadata, "\u{1}CompObj");
            storage_metadata[comp_obj + 66] = 1;
            assert!(matches!(
                convert_with_options(&storage_metadata, format, &best_effort),
                Err(ConversionError::Malformed { .. })
            ));

            let mut trailing_bytes = fixture.to_vec();
            trailing_bytes.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
            assert!(matches!(
                convert_with_options(&trailing_bytes, format, &best_effort),
                Err(ConversionError::Malformed { .. })
            ));
        }
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

use super::*;
use into_markdown::{
    Asset, AssetId, Block, BlockNode, Diagnostic, DiagnosticSeverity, Document, DocumentMetadata,
    Inline, NodeId, Provenance, ProvenanceKind, SourceLocator,
};
use std::collections::BTreeMap;
use std::io::{Cursor, Seek};

fn context() -> ExecutionContext {
    ExecutionContext::new(
        into_markdown::ExecutionOptions::default(),
        into_markdown::ResourceLimits::default(),
    )
}

fn fixture() -> ConversionResult {
    let provenance = Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: "test".into(),
        locator: SourceLocator { page: Some(1), ..SourceLocator::default() },
        confidence: Some(1.0),
    };
    let document = Document {
        metadata: DocumentMetadata {
            title: Some("A \"title\"".into()),
            authors: vec!["Alice".into()],
            properties: BTreeMap::from([("language".into(), "中文".into())]),
        },
        blocks: vec![BlockNode {
            id: NodeId("p1".into()),
            block: Block::Paragraph(vec![Inline::Text {
                value: "hello\nworld".into(),
                marks: vec![],
            }]),
            provenance: provenance.clone(),
        }],
        ..Document::default()
    };
    ConversionResult::new(
        document,
        "hello\nworld 😀\n".into(),
        vec![
            Asset {
                id: AssetId("z".into()),
                filename: Some("first.png".into()),
                media_type: "image/png".into(),
                bytes: vec![0, 1, 2, 3, 4],
                external_uri: None,
            },
            Asset {
                id: AssetId("a".into()),
                filename: Some("duplicate.png".into()),
                media_type: "image/png".into(),
                bytes: vec![0, 1, 2, 3, 4],
                external_uri: None,
            },
        ],
        vec![Diagnostic {
            code: "test.warning".into(),
            severity: DiagnosticSeverity::Warning,
            message: "visible warning".into(),
            locator: None,
        }],
        vec![provenance],
    )
}

#[test]
fn small_outputs_match_legacy_encoder_byte_for_byte() {
    let result = fixture();
    for emit in [EmitKind::Markdown, EmitKind::IrJson, EmitKind::ResultJson, EmitKind::Bundle] {
        let mut spool =
            StructuredSpool::from_result(&result, context(), emit, AssetModeArg::Extract).unwrap();
        let mut streamed = Cursor::new(Vec::new());
        spool.serialize(emit, &mut streamed).unwrap();
        let legacy = super::super::serialization::encode_result(&result, emit).unwrap();
        assert_eq!(streamed.into_inner(), legacy, "{emit:?}");
        assert_eq!(spool.serialization_calls, 1);
    }
}

#[test]
fn split_utf8_and_controls_use_the_same_json_string_wire() {
    let context = context();
    let mut spool = JsonStringSpool::new(&context, "json-string").unwrap();
    for chunk in [b"a\n\"".as_slice(), &[0xf0], &[0x9f, 0x98], &[0x80, b'\t']] {
        spool.write(chunk).unwrap();
    }
    spool.finish().unwrap();
    let mut encoded = Vec::new();
    copy_spool(&context, &spool.file, &mut encoded).unwrap();
    assert_eq!(encoded, serde_json::to_vec("a\n\"😀\t").unwrap());
}

#[test]
fn duplicate_assets_share_one_payload_spool() {
    let result = fixture();
    let context = context();
    let spool = StructuredSpool::from_result(
        &result,
        context.clone(),
        EmitKind::ResultJson,
        AssetModeArg::Extract,
    )
    .unwrap();
    assert_eq!(spool.payload_records.len(), 1);
    assert_eq!(spool.asset_records.len(), 2);
    assert_eq!(spool.payload_records[0].file.as_file().unwrap().metadata().unwrap().len(), 5);
    assert!(context.reserved_temporary_bytes() < 16 * 1024);
}

fn spool_len(spool: Option<&TemporaryFile>) -> u64 {
    spool.map_or(0, |file| file.as_file().unwrap().metadata().unwrap().len())
}

fn represented_temporary_bytes(spool: &StructuredSpool) -> u64 {
    spool_len(spool.ir.as_ref())
        + spool_len(spool.markdown.as_ref())
        + spool.markdown_json.as_ref().map_or(0, |json| spool_len(Some(&json.file)))
        + spool_len(spool.diagnostics.as_ref())
        + spool_len(spool.provenance.as_ref())
        + spool
            .payload_records
            .iter()
            .map(|payload| payload.file.as_file().unwrap().metadata().unwrap().len())
            .sum::<u64>()
}

#[test]
fn emit_and_asset_mode_materialize_only_the_frozen_representations() {
    let result = fixture();
    for emit in [EmitKind::Markdown, EmitKind::IrJson, EmitKind::ResultJson, EmitKind::Bundle] {
        for asset_mode in [AssetModeArg::Omit, AssetModeArg::Embed, AssetModeArg::Extract] {
            let context = context();
            let spool =
                StructuredSpool::from_result(&result, context.clone(), emit, asset_mode).unwrap();
            let raw_markdown = emit == EmitKind::Markdown || emit == EmitKind::Bundle;
            let escaped_markdown = emit == EmitKind::ResultJson;
            let semantic_ir = emit != EmitKind::Markdown;
            let inventories = matches!(emit, EmitKind::ResultJson | EmitKind::Bundle);
            let assets = inventories || asset_mode == AssetModeArg::Extract;

            assert_eq!(spool.markdown.is_some(), raw_markdown, "{emit:?}/{asset_mode:?}");
            assert_eq!(spool.markdown_json.is_some(), escaped_markdown, "{emit:?}/{asset_mode:?}");
            assert_eq!(spool.ir.is_some(), semantic_ir, "{emit:?}/{asset_mode:?}");
            assert_eq!(
                spool.diagnostics.is_some() && spool.provenance.is_some(),
                inventories,
                "{emit:?}/{asset_mode:?}"
            );
            assert_eq!(spool.capabilities.markdown, raw_markdown || escaped_markdown);
            assert_eq!(spool.capabilities.semantic_events, semantic_ir);
            assert_eq!(spool.capabilities.assets, assets);
            assert_eq!(!spool.payload_records.is_empty(), assets);
            assert_eq!(
                context.reserved_temporary_bytes(),
                represented_temporary_bytes(&spool),
                "unrepresented temporary bytes for {emit:?}/{asset_mode:?}"
            );
        }
    }
}

#[test]
fn absent_representations_and_emit_mismatch_fail_closed_without_writes() {
    let markdown_context = context();
    let mut markdown = StructuredSpool::from_result(
        &fixture(),
        markdown_context,
        EmitKind::Markdown,
        AssetModeArg::Omit,
    )
    .unwrap();
    let semantic_error = ArtifactSink::write_document_event(
        &mut markdown,
        &DocumentStreamEvent::Metadata(&DocumentMetadata::default()),
    )
    .unwrap_err();
    assert_eq!(semantic_error.code().as_str(), "internal");

    let mut destination = Cursor::new(Vec::new());
    let mismatch = markdown.serialize(EmitKind::ResultJson, &mut destination).unwrap_err();
    assert_eq!(mismatch.code(), "internal");
    assert!(destination.into_inner().is_empty());
    assert_eq!(markdown.serialization_calls, 0);

    let incomplete_context = context();
    let mut incomplete = StructuredSpool::from_result(
        &fixture(),
        incomplete_context,
        EmitKind::Markdown,
        AssetModeArg::Omit,
    )
    .unwrap();
    drop(incomplete.markdown.take());
    let mut untouched = Cursor::new(Vec::new());
    let missing = incomplete.serialize(EmitKind::Markdown, &mut untouched).unwrap_err();
    assert_eq!(missing.code(), "internal");
    assert!(untouched.into_inner().is_empty());
    assert_eq!(incomplete.serialization_calls, 0);

    let ir_context = context();
    let mut ir =
        StructuredSpool::from_result(&fixture(), ir_context, EmitKind::IrJson, AssetModeArg::Omit)
            .unwrap();
    let markdown_error = ArtifactSink::write_markdown(&mut ir, b"unrequested").unwrap_err();
    assert_eq!(markdown_error.code().as_str(), "internal");
    let asset = AssetStreamInfo {
        id: AssetId("unrequested".into()),
        filename: Some("unrequested.bin".into()),
        media_type: "application/octet-stream".into(),
        size: 0,
        external_uri: None,
        content_sha256: None,
    };
    let asset_error = ArtifactSink::begin_asset(&mut ir, &asset).unwrap_err();
    assert_eq!(asset_error.code().as_str(), "internal");
}

fn deadline_context() -> ExecutionContext {
    ExecutionContext::new(
        into_markdown::ExecutionOptions {
            timeout: Some(std::time::Duration::from_millis(20)),
            ..into_markdown::ExecutionOptions::default()
        },
        into_markdown::ResourceLimits::default(),
    )
}

fn assert_timeout_policy(error: ConversionError) {
    assert_eq!(error.code().as_str(), "timeout");
    let cli = CliError::from(error);
    assert_eq!(cli.code(), "timeout");
    assert_eq!(cli.exit_code(), ExitClass::Policy.code());
}

#[test]
fn deadline_is_preserved_in_semantic_markdown_and_asset_callbacks() {
    let semantic_context = deadline_context();
    let mut semantic =
        StructuredSpool::new(semantic_context.clone(), EmitKind::IrJson, AssetModeArg::Omit)
            .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    let error = ArtifactSink::write_document_event(
        &mut semantic,
        &DocumentStreamEvent::Metadata(&DocumentMetadata::default()),
    )
    .unwrap_err();
    assert_timeout_policy(error);
    drop(semantic);
    assert_eq!(semantic_context.reserved_temporary_bytes(), 0);
    assert_eq!(semantic_context.reserved_memory_bytes(), 0);

    let markdown_context = deadline_context();
    let mut markdown =
        StructuredSpool::new(markdown_context.clone(), EmitKind::Markdown, AssetModeArg::Omit)
            .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_timeout_policy(ArtifactSink::write_markdown(&mut markdown, b"late").unwrap_err());
    drop(markdown);
    assert_eq!(markdown_context.reserved_temporary_bytes(), 0);
    assert_eq!(markdown_context.reserved_memory_bytes(), 0);

    let asset_context = deadline_context();
    let mut assets =
        StructuredSpool::new(asset_context.clone(), EmitKind::Markdown, AssetModeArg::Extract)
            .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    let asset = AssetStreamInfo {
        id: AssetId("late".into()),
        filename: Some("late.bin".into()),
        media_type: "application/octet-stream".into(),
        size: 0,
        external_uri: None,
        content_sha256: None,
    };
    assert_timeout_policy(ArtifactSink::begin_asset(&mut assets, &asset).unwrap_err());
    drop(assets);
    assert_eq!(asset_context.reserved_temporary_bytes(), 0);
    assert_eq!(asset_context.reserved_memory_bytes(), 0);
}

struct BrokenPipeWriter {
    remaining: usize,
}

struct FailAfterWriter {
    destination: Cursor<Vec<u8>>,
    remaining: usize,
    injected: bool,
}

impl Write for FailAfterWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.remaining == 0 && !self.injected {
            self.injected = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "injected full destination",
            ));
        }
        let written =
            if self.remaining == 0 { bytes.len() } else { bytes.len().min(self.remaining) };
        if self.remaining != 0 {
            self.remaining -= written;
        }
        self.destination.write(&bytes[..written])
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.destination.flush()
    }
}

impl Seek for FailAfterWriter {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.destination.seek(position)
    }
}

struct CancellingWriter {
    destination: Cursor<Vec<u8>>,
    cancellation: into_markdown::CancellationToken,
    cancel_after: usize,
    written: usize,
}

struct MeteredTemporary {
    destination: TemporaryFile,
    context: ExecutionContext,
    peak_memory: u64,
    peak_temporary: u64,
}

impl MeteredTemporary {
    fn new(context: &ExecutionContext, prefix: &str) -> Self {
        Self {
            destination: context.temporary_file(prefix).unwrap(),
            context: context.clone(),
            peak_memory: 0,
            peak_temporary: 0,
        }
    }

    fn sample(&mut self) {
        self.peak_memory = self.peak_memory.max(self.context.reserved_memory_bytes());
        self.peak_temporary = self.peak_temporary.max(self.context.reserved_temporary_bytes());
    }
}

impl Write for MeteredTemporary {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.sample();
        let written = self.destination.write(bytes);
        self.sample();
        written
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.destination.flush().map_err(std::io::Error::other)
    }
}

impl Seek for MeteredTemporary {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.destination.seek(position)
    }
}

impl Write for CancellingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.destination.write(bytes)?;
        self.written = self.written.saturating_add(written);
        if self.written >= self.cancel_after {
            self.cancellation.cancel();
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.destination.flush()
    }
}

impl Seek for CancellingWriter {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.destination.seek(position)
    }
}

impl Write for BrokenPipeWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "closed"));
        }
        let written = bytes.len().min(self.remaining);
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn stdout_pipe_close_is_typed_and_spools_are_released() {
    let context = context();
    let mut spool = StructuredSpool::from_result(
        &fixture(),
        context.clone(),
        EmitKind::ResultJson,
        AssetModeArg::Extract,
    )
    .unwrap();
    let mut primary = context.temporary_file("primary").unwrap();
    spool.serialize(EmitKind::ResultJson, &mut primary).unwrap();
    let mut writer = BrokenPipeWriter { remaining: 7 };
    let error = copy_spool(&context, &primary, &mut writer).unwrap_err();
    assert!(error.is_broken_pipe());
    drop(primary);
    drop(spool);
    assert_eq!(context.reserved_temporary_bytes(), 0);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn serialization_failure_and_mid_stream_cancellation_release_every_spool() {
    let failed_context = context();
    let mut failed_spool = StructuredSpool::from_result(
        &fixture(),
        failed_context.clone(),
        EmitKind::Bundle,
        AssetModeArg::Extract,
    )
    .unwrap();
    let mut failed =
        FailAfterWriter { destination: Cursor::new(Vec::new()), remaining: 31, injected: false };
    let error = failed_spool.serialize(EmitKind::Bundle, &mut failed).unwrap_err();
    assert_eq!(error.code(), "io");
    assert_eq!(failed_spool.serialization_calls, 1);
    drop(failed_spool);
    assert_eq!(failed_context.reserved_temporary_bytes(), 0);
    assert_eq!(failed_context.reserved_memory_bytes(), 0);

    let cancellation = into_markdown::CancellationToken::new();
    let cancelled_context = ExecutionContext::new(
        into_markdown::ExecutionOptions {
            cancellation: cancellation.clone(),
            ..into_markdown::ExecutionOptions::default()
        },
        into_markdown::ResourceLimits::default(),
    );
    let mut cancelled_spool = StructuredSpool::from_result(
        &fixture(),
        cancelled_context.clone(),
        EmitKind::ResultJson,
        AssetModeArg::Extract,
    )
    .unwrap();
    let mut destination = CancellingWriter {
        destination: Cursor::new(Vec::new()),
        cancellation,
        cancel_after: 1,
        written: 0,
    };
    let error = cancelled_spool.serialize(EmitKind::ResultJson, &mut destination).unwrap_err();
    assert_eq!(error.code(), "cancelled");
    assert_eq!(cancelled_spool.serialization_calls, 1);
    drop(cancelled_spool);
    assert_eq!(cancelled_context.reserved_temporary_bytes(), 0);
    assert_eq!(cancelled_context.reserved_memory_bytes(), 0);
}

#[test]
fn multi_megabyte_ir_markdown_and_asset_stay_file_backed() {
    let mut result = fixture();
    let large_text = "0123456789abcdef".repeat(128 * 1024);
    result.markdown = format!("{large_text}\n{large_text}\n");
    result.document.blocks[0].block =
        Block::Paragraph(vec![Inline::Text { value: large_text, marks: vec![] }]);
    let asset_payload = vec![0x5a; 4 * 1024 * 1024];
    result.assets[0].bytes.clone_from(&asset_payload);
    result.assets[1].bytes = asset_payload;

    for (emit, asset_mode, expects_payload) in [
        (EmitKind::Markdown, AssetModeArg::Omit, false),
        (EmitKind::Markdown, AssetModeArg::Embed, false),
        (EmitKind::Markdown, AssetModeArg::Extract, true),
        (EmitKind::IrJson, AssetModeArg::Omit, false),
        (EmitKind::IrJson, AssetModeArg::Extract, true),
        (EmitKind::ResultJson, AssetModeArg::Omit, true),
        (EmitKind::Bundle, AssetModeArg::Omit, true),
    ] {
        let context = context();
        let mut spool =
            StructuredSpool::from_result(&result, context.clone(), emit, asset_mode).unwrap();
        assert_eq!(spool.payload_records.len(), usize::from(expects_payload));
        assert_eq!(context.reserved_temporary_bytes(), represented_temporary_bytes(&spool));
        assert!(context.reserved_memory_bytes() < 2 * 1024 * 1024);
        let before_serialization = context.reserved_temporary_bytes();
        let mut primary = MeteredTemporary::new(&context, "large-primary");
        spool.serialize(emit, &mut primary).unwrap();
        assert_eq!(spool.serialization_calls, 1);
        assert!(context.reserved_temporary_bytes() > before_serialization);
        assert!(primary.peak_memory < 4 * 1024 * 1024, "{emit:?} memory peak");
        assert!(primary.peak_temporary < 40 * 1024 * 1024, "{emit:?} temporary peak");
        drop(primary);
        drop(spool);
        assert_eq!(context.reserved_temporary_bytes(), 0);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn temporary_limit_failure_and_cancellation_release_every_spool() {
    let result = fixture();
    let measured_context = context();
    let measured = StructuredSpool::from_result(
        &result,
        measured_context.clone(),
        EmitKind::ResultJson,
        AssetModeArg::Extract,
    )
    .unwrap();
    let exact = measured_context.reserved_temporary_bytes();
    let exact_memory = measured_context.reserved_memory_bytes();
    drop(measured);
    assert_eq!(measured_context.reserved_temporary_bytes(), 0);

    let exact_limits = into_markdown::ResourceLimits {
        max_temporary_bytes: exact,
        ..into_markdown::ResourceLimits::default()
    };
    let exact_context =
        ExecutionContext::new(into_markdown::ExecutionOptions::default(), exact_limits);
    let exact_spool = StructuredSpool::from_result(
        &result,
        exact_context.clone(),
        EmitKind::ResultJson,
        AssetModeArg::Extract,
    )
    .unwrap();
    assert_eq!(exact_context.reserved_temporary_bytes(), exact);
    drop(exact_spool);
    assert_eq!(exact_context.reserved_temporary_bytes(), 0);

    let limits = into_markdown::ResourceLimits {
        max_temporary_bytes: exact - 1,
        ..into_markdown::ResourceLimits::default()
    };
    let low = ExecutionContext::new(into_markdown::ExecutionOptions::default(), limits);
    let error = StructuredSpool::from_result(
        &result,
        low.clone(),
        EmitKind::ResultJson,
        AssetModeArg::Extract,
    )
    .err()
    .unwrap();
    assert_eq!(error.code(), "resourceLimit");
    assert_eq!(low.reserved_temporary_bytes(), 0);
    assert_eq!(low.reserved_memory_bytes(), 0);

    let exact_memory_limits = into_markdown::ResourceLimits {
        max_memory_bytes: exact_memory,
        ..into_markdown::ResourceLimits::default()
    };
    let exact_memory_context =
        ExecutionContext::new(into_markdown::ExecutionOptions::default(), exact_memory_limits);
    let exact_memory_spool = StructuredSpool::from_result(
        &result,
        exact_memory_context.clone(),
        EmitKind::ResultJson,
        AssetModeArg::Extract,
    )
    .unwrap();
    assert_eq!(exact_memory_context.reserved_memory_bytes(), exact_memory);
    drop(exact_memory_spool);
    assert_eq!(exact_memory_context.reserved_memory_bytes(), 0);

    let memory_limits = into_markdown::ResourceLimits {
        max_memory_bytes: exact_memory - 1,
        ..into_markdown::ResourceLimits::default()
    };
    let low_memory =
        ExecutionContext::new(into_markdown::ExecutionOptions::default(), memory_limits);
    let error = StructuredSpool::from_result(
        &result,
        low_memory.clone(),
        EmitKind::ResultJson,
        AssetModeArg::Extract,
    )
    .err()
    .unwrap();
    assert_eq!(error.code(), "resourceLimit");
    assert_eq!(low_memory.reserved_temporary_bytes(), 0);
    assert_eq!(low_memory.reserved_memory_bytes(), 0);

    let cancellation = into_markdown::CancellationToken::new();
    let cancelled = ExecutionContext::new(
        into_markdown::ExecutionOptions {
            cancellation: cancellation.clone(),
            ..into_markdown::ExecutionOptions::default()
        },
        into_markdown::ResourceLimits::default(),
    );
    cancellation.cancel();
    let error = StructuredSpool::from_result(
        &result,
        cancelled.clone(),
        EmitKind::ResultJson,
        AssetModeArg::Extract,
    )
    .err()
    .unwrap();
    assert_eq!(error.code(), "cancelled");
    assert_eq!(cancelled.reserved_temporary_bytes(), 0);
    assert_eq!(cancelled.reserved_memory_bytes(), 0);
}

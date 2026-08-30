use super::*;
use into_markdown::{
    Asset, AssetId, Block, BlockNode, Diagnostic, DiagnosticSeverity, DocumentMetadata, Inline,
    NodeId, Provenance, ProvenanceKind, SourceLocator,
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
        let mut spool = StructuredSpool::from_result(&result, context()).unwrap();
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
    let spool = StructuredSpool::from_result(&result, context.clone()).unwrap();
    assert_eq!(spool.payload_records.len(), 1);
    assert_eq!(spool.asset_records.len(), 2);
    assert_eq!(spool.payloads.as_file().unwrap().metadata().unwrap().len(), 5);
    assert!(context.reserved_temporary_bytes() < 16 * 1024);
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
    let mut spool = StructuredSpool::from_result(&fixture(), context.clone()).unwrap();
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
    let mut failed_spool =
        StructuredSpool::from_result(&fixture(), failed_context.clone()).unwrap();
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
    let mut cancelled_spool =
        StructuredSpool::from_result(&fixture(), cancelled_context.clone()).unwrap();
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

    for emit in [EmitKind::ResultJson, EmitKind::Bundle] {
        let context = context();
        let mut spool = StructuredSpool::from_result(&result, context.clone()).unwrap();
        assert_eq!(spool.payload_records.len(), 1);
        assert!(context.reserved_temporary_bytes() > 10 * 1024 * 1024);
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
    let measured = StructuredSpool::from_result(&result, measured_context.clone()).unwrap();
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
    let exact_spool = StructuredSpool::from_result(&result, exact_context.clone()).unwrap();
    assert_eq!(exact_context.reserved_temporary_bytes(), exact);
    drop(exact_spool);
    assert_eq!(exact_context.reserved_temporary_bytes(), 0);

    let limits = into_markdown::ResourceLimits {
        max_temporary_bytes: exact - 1,
        ..into_markdown::ResourceLimits::default()
    };
    let low = ExecutionContext::new(into_markdown::ExecutionOptions::default(), limits);
    let error = StructuredSpool::from_result(&result, low.clone()).err().unwrap();
    assert_eq!(error.code(), "resourceLimit");
    assert_eq!(low.reserved_temporary_bytes(), 0);
    assert_eq!(low.reserved_memory_bytes(), 0);

    let exact_memory_limits = into_markdown::ResourceLimits {
        max_memory_bytes: exact_memory,
        ..into_markdown::ResourceLimits::default()
    };
    let exact_memory_context =
        ExecutionContext::new(into_markdown::ExecutionOptions::default(), exact_memory_limits);
    let exact_memory_spool =
        StructuredSpool::from_result(&result, exact_memory_context.clone()).unwrap();
    assert_eq!(exact_memory_context.reserved_memory_bytes(), exact_memory);
    drop(exact_memory_spool);
    assert_eq!(exact_memory_context.reserved_memory_bytes(), 0);

    let memory_limits = into_markdown::ResourceLimits {
        max_memory_bytes: exact_memory - 1,
        ..into_markdown::ResourceLimits::default()
    };
    let low_memory =
        ExecutionContext::new(into_markdown::ExecutionOptions::default(), memory_limits);
    let error = StructuredSpool::from_result(&result, low_memory.clone()).err().unwrap();
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
    let error = StructuredSpool::from_result(&result, cancelled.clone()).err().unwrap();
    assert_eq!(error.code(), "cancelled");
    assert_eq!(cancelled.reserved_temporary_bytes(), 0);
    assert_eq!(cancelled.reserved_memory_bytes(), 0);
}

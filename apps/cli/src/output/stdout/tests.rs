use super::*;
use crate::args::{AssetModeArg, ConflictPolicy};
use crate::output::assets::stage_assets;
use into_markdown::{
    Asset, AssetId, Block, BlockNode, ConversionResult, Document, NodeId, Provenance,
    ProvenanceKind, SourceLocator,
};

struct FailingWriter {
    kind: std::io::ErrorKind,
    remaining: usize,
}

impl Write for FailingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::new(self.kind, "injected stdout failure"));
        }
        let written = bytes.len().min(self.remaining);
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn result() -> ConversionResult {
    let asset = Asset {
        id: AssetId("image".into()),
        filename: Some("image.png".into()),
        media_type: "image/png".into(),
        bytes: vec![1, 2, 3],
        external_uri: None,
    };
    ConversionResult::new(
        Document {
            blocks: vec![BlockNode {
                id: NodeId("image-node".into()),
                block: Block::Image { asset: asset.id.clone(), alt: Some("image".into()) },
                provenance: Provenance {
                    kind: ProvenanceKind::NativeParser,
                    provider: "test".into(),
                    locator: SourceLocator::default(),
                    confidence: None,
                },
            }],
            ..Document::default()
        },
        "![image](assets/image.png)\n".into(),
        vec![asset],
        vec![],
        vec![],
    )
}

fn context() -> ExecutionContext {
    ExecutionContext::new(
        into_markdown::ExecutionOptions::default(),
        into_markdown::ResourceLimits::default(),
    )
}

#[test]
fn broken_pipe_commits_fully_staged_assets_and_releases_copy_memory() {
    let context = context();
    let temporary = tempfile::tempdir().unwrap();
    let assets = temporary.path().join("assets");
    let result = result();
    let staged =
        stage_assets(&result, &assets, AssetModeArg::Extract, ConflictPolicy::Error, &context)
            .unwrap();
    let mut primary = context.temporary_file("stdout-primary").unwrap();
    primary.write_all_checked(b"complete primary").unwrap();
    let mut stdout = FailingWriter { kind: std::io::ErrorKind::BrokenPipe, remaining: 1 };

    let error = publish(&primary, &mut stdout, Some(staged), &context).unwrap_err();

    assert!(error.is_broken_pipe());
    let written = std::fs::read_dir(&assets).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(written.len(), 1);
    assert_eq!(std::fs::read(written[0].path()).unwrap(), [1, 2, 3]);
    assert_eq!(context.reserved_memory_bytes(), 0);
    drop(primary);
    assert_eq!(context.reserved_temporary_bytes(), 0);
}

#[test]
fn non_pipe_failure_aborts_assets_and_releases_copy_memory() {
    let context = context();
    let temporary = tempfile::tempdir().unwrap();
    let assets = temporary.path().join("assets");
    let result = result();
    let staged =
        stage_assets(&result, &assets, AssetModeArg::Extract, ConflictPolicy::Error, &context)
            .unwrap();
    let mut primary = context.temporary_file("stdout-primary").unwrap();
    primary.write_all_checked(b"complete primary").unwrap();
    let mut stdout = FailingWriter { kind: std::io::ErrorKind::StorageFull, remaining: 1 };

    let error = publish(&primary, &mut stdout, Some(staged), &context).unwrap_err();

    assert_eq!(error.code(), "io");
    assert!(!assets.exists() || std::fs::read_dir(&assets).unwrap().next().is_none());
    assert_eq!(context.reserved_memory_bytes(), 0);
    drop(primary);
    assert_eq!(context.reserved_temporary_bytes(), 0);
}

#[test]
fn cancellation_before_stdout_copy_aborts_assets() {
    let cancellation = into_markdown::CancellationToken::new();
    let context = ExecutionContext::new(
        into_markdown::ExecutionOptions {
            cancellation: cancellation.clone(),
            ..into_markdown::ExecutionOptions::default()
        },
        into_markdown::ResourceLimits::default(),
    );
    let temporary = tempfile::tempdir().unwrap();
    let assets = temporary.path().join("assets");
    let result = result();
    let staged =
        stage_assets(&result, &assets, AssetModeArg::Extract, ConflictPolicy::Error, &context)
            .unwrap();
    let mut primary = context.temporary_file("stdout-primary").unwrap();
    primary.write_all_checked(b"complete primary").unwrap();
    cancellation.cancel();

    let error = publish(&primary, &mut Vec::new(), Some(staged), &context).unwrap_err();

    assert_eq!(error.code(), "cancelled");
    assert!(!assets.exists() || std::fs::read_dir(&assets).unwrap().next().is_none());
    assert_eq!(context.reserved_memory_bytes(), 0);
    drop(primary);
    assert_eq!(context.reserved_temporary_bytes(), 0);
}

//! Real process-v1 guest used by plugin-manager installation/dispatch tests.

use into_markdown_core::{ConversionResult, Document, Provenance, ProvenanceKind, SourceLocator};
use into_markdown_process_plugin::worker::{self, WorkerError};

fn main() -> std::io::Result<()> {
    worker::serve("fixture.manager-process", 1024 * 1024, |request, _, _| {
        if request.source != b"ok" {
            return Err(WorkerError::new("fixtureInput", "expected ok"));
        }
        Ok(ConversionResult::new(
            Document::default(),
            "manager-process-ok".into(),
            Vec::new(),
            Vec::new(),
            vec![Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: "fixture.manager-process".into(),
                locator: SourceLocator::default(),
                confidence: Some(1.0),
            }],
        ))
    })
}

use super::*;

pub(super) fn restored_result(
    document: into_markdown::Document,
    markdown: String,
    assets: Vec<into_markdown::Asset>,
    artifact: OwnedDiagnosticsArtifact,
    provenance: Vec<into_markdown::Provenance>,
) -> into_markdown::ConversionResult {
    let mut result = into_markdown::ConversionResult::new(
        document,
        markdown,
        assets,
        artifact.diagnostics,
        provenance,
    );
    result.set_ocr_runtime_usage(artifact.ocr_runtime);
    result
}

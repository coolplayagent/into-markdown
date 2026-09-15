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

/// External references have no local bytes to include in a portable bundle.
/// Keep the other artifacts available without fetching remote resources.
pub(super) fn bundle_unavailable(result: &into_markdown::ConversionResult) -> bool {
    result.assets.iter().any(|asset| asset.bytes.is_empty() && asset.external_uri.is_some())
        && result.assets.iter().all(|asset| !asset.bytes.is_empty() || asset.external_uri.is_some())
}

pub(super) fn publication_diagnostics(
    result: &into_markdown::ConversionResult,
) -> std::borrow::Cow<'_, [into_markdown::Diagnostic]> {
    const CODE: &str = "webBundleExternalAssets";
    if !bundle_unavailable(result) || result.diagnostics.iter().any(|item| item.code == CODE) {
        return std::borrow::Cow::Borrowed(&result.diagnostics);
    }
    let mut diagnostics = result.diagnostics.clone();
    diagnostics.push(into_markdown::Diagnostic {
        code: CODE.into(),
        severity: into_markdown::DiagnosticSeverity::Warning,
        message: "Portable bundle unavailable because source images use external URLs. Markdown, document data, and available local assets remain downloadable; external image links are preserved.".into(),
        locator: None,
    });
    std::borrow::Cow::Owned(diagnostics)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ArtifactManifest<'a> {
    schema_version: u32,
    bundle_unavailable: bool,
    entries: &'a [ArtifactReference],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DiagnosticsArtifact<'a> {
    schema_version: u32,
    diagnostics: std::borrow::Cow<'a, [into_markdown::Diagnostic]>,
    outcome: &'a str,
    ocr_runtime: Option<into_markdown::OcrRuntimeUsageDto>,
}

impl<'a> DiagnosticsArtifact<'a> {
    pub(super) fn from_result(result: &'a into_markdown::ConversionResult) -> Self {
        Self {
            schema_version: 1,
            diagnostics: publication_diagnostics(result),
            outcome: match result.outcome() {
                into_markdown::ConversionOutcome::Complete if !bundle_unavailable(result) => {
                    "complete"
                }
                into_markdown::ConversionOutcome::Complete => "degraded",
                into_markdown::ConversionOutcome::Degraded => "degraded",
            },
            ocr_runtime: result.ocr_runtime_usage(),
        }
    }
}

impl<'a> ArtifactManifest<'a> {
    pub(super) fn new(
        result: &into_markdown::ConversionResult,
        entries: &'a [ArtifactReference],
    ) -> Self {
        Self { schema_version: 1, bundle_unavailable: bundle_unavailable(result), entries }
    }
}

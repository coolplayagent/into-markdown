use super::*;

pub(super) fn encode_result(value: &ResultDto) -> RawResultDto {
    RawResultDto {
        schema_version: value.schema_version,
        markdown: value.markdown.clone(),
        document: value.document.clone(),
        assets: value.assets.iter().map(RawAssetDto::from).collect(),
        diagnostics: value.diagnostics.iter().map(RawDiagnosticDto::from).collect(),
        provenance: value.provenance.iter().map(RawProvenanceDto::from).collect(),
    }
}

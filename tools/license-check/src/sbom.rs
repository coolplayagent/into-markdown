//! Deterministic NOTICE and SBOM-input rendering from reviewed components.

use crate::schema::{Component, GeneratedFile, SbomComponent, SbomInput};
use sha2::{Digest, Sha256};

pub(crate) fn generated_file(path: &str, contents: String) -> GeneratedFile {
    GeneratedFile {
        path: path.to_owned(),
        bytes: contents.len() as u64,
        sha256: hex(&contents),
        contents,
    }
}

pub(crate) fn render_notices(project_notice: &str, components: &[&Component]) -> String {
    let mut output = String::from(
        "# Third-party notices\n\nThis file is generated from the repository component authority.\n",
    );
    for component in components {
        output.push_str("\n## ");
        output.push_str(&component.id);
        output.push_str("\n\nVersion: ");
        output.push_str(component.version.as_deref().unwrap_or_default());
        output.push_str("\n\nSource: ");
        output.push_str(component.source.as_deref().unwrap_or_default());
        output.push_str("\n\nLicense: ");
        output.push_str(component.license.as_deref().unwrap_or_default());
        output.push_str("\n\nObligations: ");
        output.push_str(component.obligations.as_deref().unwrap_or_default());
        output.push('\n');
    }
    output.push_str("\n## Project NOTICE\n\n");
    output.push_str(project_notice.trim_end());
    output.push('\n');
    output
}

pub(crate) fn render_sbom(target: &str, components: &[&Component]) -> SbomInput {
    SbomInput {
        schema_version: 1,
        target: target.to_owned(),
        components: components
            .iter()
            .map(|component| SbomComponent {
                id: component.id.clone(),
                kind: component.kind.clone(),
                version: component.version.clone().unwrap_or_default(),
                source: component.source.clone().unwrap_or_default(),
                license: component.license.clone().unwrap_or_default(),
            })
            .collect(),
    }
}

pub(crate) fn pretty_json<T: serde::Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value)
        .map(|mut json| {
            json.push('\n');
            json
        })
        .map_err(|error| format!("cannot render deterministic JSON: {error}"))
}

pub(crate) fn hex(contents: &str) -> String {
    format!("{:x}", Sha256::digest(contents.as_bytes()))
}

//! Deterministic SPDX 2.3, source-manifest, and notice rendering.

use crate::schema::{
    ArchiveFile, BuildTool, Component, GeneratedFile, IntegrityEvidence, ReleaseArtifact,
    ReleaseSetRequest, SourceArtifact, SourceComponent, SourceFile, SourceManifest,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

const CREATED: &str = "2026-01-01T00:00:00Z";

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

#[allow(clippy::too_many_arguments)] // The renderer keeps every release identity input explicit.
pub(crate) fn render_sources(
    repository: &Path,
    target: &str,
    artifact: ReleaseArtifact,
    version: &str,
    source_revision: &str,
    components: &[&Component],
    non_distributed: &[SourceComponent],
    build_tools: &[BuildTool],
    artifact_file: Option<(&str, u64, &str)>,
    files: &[ArchiveFile],
    errors: &mut Vec<String>,
) -> SourceManifest {
    let mut sources = source_components(repository, target, components, files, errors);
    sources.extend_from_slice(non_distributed);
    sources.sort_by(|left, right| left.id.cmp(&right.id));
    SourceManifest {
        schema_version: 1,
        target: target.to_owned(),
        artifact: artifact.id().to_owned(),
        version: version.to_owned(),
        source_revision: source_revision.to_owned(),
        artifact_file: artifact_file.map(|(file_name, bytes, sha256)| SourceArtifact {
            file_name: file_name.to_owned(),
            bytes,
            sha256: sha256.to_owned(),
        }),
        components: sources,
        build_tools: build_tools.to_vec(),
    }
}

fn source_components(
    repository: &Path,
    target: &str,
    components: &[&Component],
    files: &[ArchiveFile],
    errors: &mut Vec<String>,
) -> Vec<SourceComponent> {
    components
        .iter()
        .map(|component| {
            let mut integrity = component.integrity.clone();
            integrity.extend(crate::native::integrity(repository, target, &component.id, errors));
            SourceComponent {
                id: component.id.clone(),
                kind: component.kind.clone(),
                version: component.version.clone().unwrap_or_default(),
                source: component.source.clone().unwrap_or_default(),
                license: component.license.clone().unwrap_or_default(),
                scope: "runtime".to_owned(),
                distributed: true,
                integrity,
                authority: component.authority.clone(),
                files: owned_files(&component.id, files),
            }
        })
        .collect()
}

fn owned_files(component_id: &str, files: &[ArchiveFile]) -> Vec<SourceFile> {
    files
        .iter()
        .filter(|file| {
            file.component_id.as_deref() == Some(component_id)
                || file.embedded_components.iter().any(|item| item == component_id)
        })
        .map(|file| SourceFile {
            path: file.path.clone(),
            bytes: file.bytes,
            sha1: file.sha1.clone().unwrap_or_default(),
            sha256: file.sha256.clone(),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)] // The renderer keeps every release identity input explicit.
pub(crate) fn render_component_sbom(
    repository: &Path,
    target: &str,
    artifact: ReleaseArtifact,
    version: &str,
    components: &[&Component],
    non_distributed: &[SourceComponent],
    build_tools: &[BuildTool],
    errors: &mut Vec<String>,
) -> Value {
    let sources = source_components(repository, target, components, &[], errors);
    let root_id = "SPDXRef-Package-Root";
    let mut packages = vec![root_package(artifact.id(), version, None, false)];
    let mut relationships = vec![relationship("SPDXRef-DOCUMENT", "DESCRIBES", root_id)];
    for component in &sources {
        let id = component_id(&component.id);
        packages.push(component_package(component, &id));
        relationships.push(relationship(root_id, "DEPENDS_ON", &id));
    }
    append_build_dependencies(
        root_id,
        non_distributed,
        build_tools,
        &mut packages,
        &mut relationships,
    );
    document(
        &format!("{}-{version}-{target}", artifact.id()),
        &format!(
            "https://sbom.into-markdown.invalid/component/{target}/{}/{version}",
            artifact.id()
        ),
        &packages,
        Vec::new(),
        &relationships,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_artifact_sbom(
    repository: &Path,
    target: &str,
    artifact: ReleaseArtifact,
    version: &str,
    file_name: &str,
    bytes: u64,
    sha256: &str,
    components: &[&Component],
    non_distributed: &[SourceComponent],
    build_tools: &[BuildTool],
    files: &[ArchiveFile],
    errors: &mut Vec<String>,
) -> Value {
    let sources = source_components(repository, target, components, files, errors);
    let root_id = "SPDXRef-Package-Root";
    let mut root = root_package(artifact.id(), version, Some((file_name, bytes, sha256)), true);
    root["packageFileName"] = json!(file_name);
    root["packageVerificationCode"] = json!({
        "packageVerificationCodeValue": verification_code(
            files.iter().filter_map(|file| file.sha1.as_deref())
        )
    });
    let mut packages = vec![root];
    let mut relationships = vec![relationship("SPDXRef-DOCUMENT", "DESCRIBES", root_id)];
    for component in &sources {
        let id = component_id(&component.id);
        let mut package = component_package(component, &id);
        package["filesAnalyzed"] = json!(true);
        package["packageVerificationCode"] = json!({
            "packageVerificationCodeValue": verification_code(
                component.files.iter().map(|file| file.sha1.as_str())
            )
        });
        packages.push(package);
        relationships.push(relationship(root_id, "DEPENDS_ON", &id));
    }
    append_build_dependencies(
        root_id,
        non_distributed,
        build_tools,
        &mut packages,
        &mut relationships,
    );
    let mut spdx_files = Vec::new();
    for file in files {
        let id = file_id(&file.path);
        spdx_files.push(json!({
            "fileName": format!("./{}", file.path),
            "SPDXID": id,
            "checksums": [
                {"algorithm":"SHA1","checksumValue":file.sha1.as_deref().unwrap_or_default()},
                {"algorithm":"SHA256","checksumValue":file.sha256}
            ],
            "licenseConcluded":"NOASSERTION",
            "copyrightText":"NOASSERTION"
        }));
        relationships.push(relationship(root_id, "CONTAINS", &id));
        if let Some(owner) = &file.component_id {
            relationships.push(relationship(&component_id(owner), "CONTAINS", &id));
        }
        for owner in &file.embedded_components {
            relationships.push(relationship(&component_id(owner), "CONTAINS", &id));
        }
    }
    document(
        &format!("{file_name}-{sha256}"),
        &format!("https://sbom.into-markdown.invalid/artifact/{target}/{sha256}"),
        &packages,
        spdx_files,
        &relationships,
    )
}

fn append_build_dependencies(
    root_id: &str,
    non_distributed: &[SourceComponent],
    build_tools: &[BuildTool],
    packages: &mut Vec<Value>,
    relationships: &mut Vec<Value>,
) {
    for component in non_distributed {
        let id = component_id(&component.id);
        packages.push(component_package(component, &id));
        let kind =
            if component.scope == "test" { "TEST_DEPENDENCY_OF" } else { "BUILD_DEPENDENCY_OF" };
        relationships.push(relationship(&id, kind, root_id));
    }
    for tool in build_tools {
        let id = component_id(&format!("build-tool:{}", tool.id));
        packages.push(build_tool_package(tool, &id));
        relationships.push(relationship(&id, "BUILD_DEPENDENCY_OF", root_id));
    }
}

pub(crate) fn render_release_set(request: &ReleaseSetRequest) -> (Value, Value) {
    let root_id = "SPDXRef-Package-ReleaseSet";
    let mut packages =
        vec![root_package("into-markdown-complete-offline", &request.version, None, false)];
    packages[0]["SPDXID"] = json!(root_id);
    let mut relationships = vec![relationship("SPDXRef-DOCUMENT", "DESCRIBES", root_id)];
    let mut core = Vec::new();
    let mut complete = Vec::new();
    let mut plugin_artifacts = Vec::new();
    let mut core_instances = BTreeSet::new();
    let mut complete_instances = BTreeSet::new();
    let mut component_union = BTreeSet::new();
    for artifact in &request.artifacts {
        let id = component_id(&format!("artifact-{}", artifact.artifact.id()));
        let mut package = root_package(
            artifact.artifact.id(),
            &request.version,
            Some((&artifact.file_name, artifact.bytes, &artifact.sha256)),
            false,
        );
        package["SPDXID"] = json!(id);
        package["packageFileName"] = json!(artifact.file_name);
        packages.push(package);
        relationships.push(relationship(root_id, "CONTAINS", &id));
        complete.push(artifact.file_name.clone());
        if artifact.artifact == ReleaseArtifact::Core {
            core.push(artifact.file_name.clone());
            core_instances.extend(
                artifact
                    .components
                    .iter()
                    .map(|component| format!("{}::{component}", artifact.artifact.id())),
            );
        } else {
            plugin_artifacts.push(artifact.file_name.clone());
        }
        complete_instances.extend(
            artifact
                .components
                .iter()
                .map(|component| format!("{}::{component}", artifact.artifact.id())),
        );
        component_union.extend(artifact.components.iter().cloned());
    }
    let plugin_instances: Vec<_> =
        complete_instances.difference(&core_instances).cloned().collect();
    let manifest = json!({
        "schema_version": 1,
        "target": request.target,
        "version": request.version,
        "source_revision": request.source_revision,
        "profiles": {"core": core, "complete-offline": complete},
        "component_instances": {
            "core": core_instances,
            "complete-offline": complete_instances,
        },
        "complete_offline_minus_core": {
            "artifacts": plugin_artifacts,
            "component_instances": plugin_instances,
        },
        "artifacts": request.artifacts,
        "components": component_union,
    });
    let sbom = document(
        &format!("into-markdown-release-set-{}-{}", request.version, request.target),
        &format!(
            "https://sbom.into-markdown.invalid/release-set/{}/{}",
            request.target, request.version
        ),
        &packages,
        Vec::new(),
        &relationships,
    );
    (manifest, sbom)
}

fn root_package(
    name: &str,
    version: &str,
    checksum: Option<(&str, u64, &str)>,
    files_analyzed: bool,
) -> Value {
    let mut value = json!({
        "name": name,
        "SPDXID": "SPDXRef-Package-Root",
        "versionInfo": version,
        "downloadLocation": "NOASSERTION",
        "filesAnalyzed": files_analyzed,
        "licenseConcluded": "NOASSERTION",
        "licenseDeclared": "Apache-2.0",
        "copyrightText": "Copyright Into Markdown contributors"
    });
    if let Some((file_name, bytes, sha256)) = checksum {
        value["checksums"] = json!([{"algorithm":"SHA256","checksumValue":sha256}]);
        value["summary"] = json!(format!("Final artifact {file_name}, {bytes} bytes"));
    }
    value
}

fn component_package(component: &SourceComponent, id: &str) -> Value {
    let checksums: Vec<_> = component.integrity.iter().filter_map(spdx_checksum).collect();
    let license = if component.license.contains("LicenseRef-") {
        "NOASSERTION"
    } else {
        component.license.as_str()
    };
    let mut value = json!({
        "name": component.id,
        "SPDXID": id,
        "versionInfo": component.version,
        "downloadLocation": component.source,
        "filesAnalyzed": false,
        "licenseConcluded": license,
        "licenseDeclared": license,
        "copyrightText": "NOASSERTION",
        "sourceInfo": component.authority,
    });
    if !checksums.is_empty() {
        value["checksums"] = Value::Array(checksums);
    }
    value
}

fn build_tool_package(tool: &BuildTool, id: &str) -> Value {
    json!({
        "name": tool.id,
        "SPDXID": id,
        "versionInfo": tool.version,
        "downloadLocation": tool.source,
        "filesAnalyzed": false,
        "licenseConcluded": "NOASSERTION",
        "licenseDeclared": "NOASSERTION",
        "copyrightText": "NOASSERTION",
        "sourceInfo": format!("{} scope; distributed={}", tool.scope, tool.distributed),
    })
}

fn verification_code<'a>(checksums: impl Iterator<Item = &'a str>) -> String {
    let mut checksums: Vec<_> = checksums.collect();
    checksums.sort_unstable();
    sha1_hex(checksums.concat().as_bytes())
}

fn sha1_hex(input: &[u8]) -> String {
    let bit_length = (input.len() as u64).wrapping_mul(8);
    let mut message = input.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    let mut state = [0x6745_2301_u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0];
    for chunk in message.chunks_exact(64) {
        let mut words = [0_u32; 80];
        for (index, word) in words[..16].iter_mut().enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes(chunk[offset..offset + 4].try_into().unwrap_or([0; 4]));
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let [mut work0, mut work1, mut work2, mut work3, mut work4] = state;
        for (index, word) in words.iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((work1 & work2) | ((!work1) & work3), 0x5a82_7999),
                20..=39 => (work1 ^ work2 ^ work3, 0x6ed9_eba1),
                40..=59 => ((work1 & work2) | (work1 & work3) | (work2 & work3), 0x8f1b_bcdc),
                _ => (work1 ^ work2 ^ work3, 0xca62_c1d6),
            };
            let next = work0
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(work4)
                .wrapping_add(constant)
                .wrapping_add(*word);
            work4 = work3;
            work3 = work2;
            work2 = work1.rotate_left(30);
            work1 = work0;
            work0 = next;
        }
        for (slot, value) in state.iter_mut().zip([work0, work1, work2, work3, work4]) {
            *slot = slot.wrapping_add(value);
        }
    }
    state.iter().fold(String::with_capacity(40), |mut output, value| {
        write!(output, "{value:08x}").expect("writing to a string cannot fail");
        output
    })
}

fn spdx_checksum(evidence: &IntegrityEvidence) -> Option<Value> {
    let algorithm = match evidence.algorithm.as_str() {
        "SHA-256" if valid_hex(&evidence.digest, 64) => "SHA256",
        "SHA-512" if valid_hex(&evidence.digest, 128) => "SHA512",
        _ => return None,
    };
    Some(json!({"algorithm":algorithm,"checksumValue":evidence.digest}))
}

fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn relationship(from: &str, kind: &str, to: &str) -> Value {
    json!({"spdxElementId":from,"relationshipType":kind,"relatedSpdxElement":to})
}

fn document(
    name: &str,
    namespace: &str,
    packages: &[Value],
    files: Vec<Value>,
    relationships: &[Value],
) -> Value {
    let mut value = json!({
        "spdxVersion":"SPDX-2.3",
        "dataLicense":"CC0-1.0",
        "SPDXID":"SPDXRef-DOCUMENT",
        "name":name,
        "documentNamespace":namespace,
        "creationInfo":{"created":CREATED,"creators":["Tool: into-markdown-release-projection"]},
        "packages":packages,
        "relationships":relationships,
    });
    if !files.is_empty() {
        value["files"] = Value::Array(files);
    }
    value
}

fn component_id(value: &str) -> String {
    format!("SPDXRef-Package-{}", &format!("{:x}", Sha256::digest(value.as_bytes()))[..24])
}

fn file_id(value: &str) -> String {
    format!("SPDXRef-File-{}", &format!("{:x}", Sha256::digest(value.as_bytes()))[..24])
}

pub(crate) fn pretty_json<T: Serialize>(value: &T) -> Result<String, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_and_package_verification_code_match_standard_vectors() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            verification_code(["a9993e364706816aba3e25717850c26c9cd0d89d"].into_iter()),
            "9ef2bdeea2b1bae79b9ddb930427d0b2c880bdac"
        );
    }
}

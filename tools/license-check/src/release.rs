//! Platform-neutral release projection and archive-verification API.

use crate::sbom::{
    generated_file, pretty_json, render_artifact_sbom, render_component_sbom, render_notices,
    render_release_set, render_sources,
};
use crate::schema::{
    ArchiveFile, ArchiveFileKind, ArchiveProjection, ArtifactMetadata, ArtifactProjection,
    BuildTool, BuildToolExecutable, BuildToolInventory, Component, Inventory, Policy,
    ReleaseArtifact, ReleaseInputs, ReleaseRequest, ReleaseSetMetadata, ReleaseSetRequest,
    SCHEMA_VERSION, SUPPORTED_TARGETS, SourceDependencyInventory,
};
use crate::{ffmpeg, materials, models_fixtures, native, npm, rust};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component as PathComponent, Path};

const LICENSE_PATH: &str = "LICENSE";
const NOTICE_PATH: &str = "NOTICE";
const THIRD_PARTY_PATH: &str = "THIRD_PARTY_NOTICES.md";
const SBOM_PATH: &str = "SBOM.spdx.json";
const SOURCES_PATH: &str = "SOURCES.json";
const CORE_CATALOG_PATH: &str = "core-catalog.json";

pub(crate) fn audit_repository_contract(repository: &Path, errors: &mut Vec<String>) {
    rust::validate_bazel_bridge(repository, errors);
    ffmpeg::audit_repository(repository, errors);
    let mut notices = BTreeSet::new();
    for target in SUPPORTED_TARGETS {
        let path = repository
            .join("tools/license-check/fixtures")
            .join(format!("release-request-{target}.json"));
        let request = read(path, errors);
        match generate_release_inputs_unchecked(repository, &request) {
            Ok(inputs) if inputs.target == target => {
                notices.insert(inputs.third_party_notices.contents);
            }
            Ok(_) => errors.push(format!("release request fixture target disagrees with {target}")),
            Err(mut fixture_errors) => errors.append(&mut fixture_errors),
        }
    }
    if notices.len() != 1 {
        errors.push(
            "platform projections produce different component license conclusions".to_owned(),
        );
    }
}

/// Generate deterministic NOTICE and SBOM inputs for the exact component projection.
///
/// # Errors
///
/// Returns every sorted authority, schema, policy, or rendering violation. Input is never repaired.
pub fn generate_release_inputs(
    repository: &Path,
    request_json: &str,
) -> Result<ReleaseInputs, Vec<String>> {
    crate::audit(repository, true)?;
    generate_release_inputs_unchecked(repository, request_json)
}

pub(crate) fn generate_release_inputs_unchecked(
    repository: &Path,
    request_json: &str,
) -> Result<ReleaseInputs, Vec<String>> {
    let mut errors = Vec::new();
    let request: Option<ReleaseRequest> = parse("release request", request_json, &mut errors);
    let authorities = load_authorities(
        repository,
        request.as_ref().map_or(ReleaseArtifact::Core, |item| item.artifact),
        &mut errors,
    );
    let (Some(request), Some((inventory, policy))) = (request, authorities) else {
        return Err(sorted(errors));
    };
    validate_header(request.schema_version, &request.target, &mut errors);
    validate_source_revision(&request.source_revision, &mut errors);
    let selected =
        select_components(request.artifact, &request.components, &inventory, &policy, &mut errors);
    if !errors.is_empty() {
        return Err(sorted(errors));
    }

    let project_notice = read(repository.join(NOTICE_PATH), &mut errors);
    let third_party = render_notices(&project_notice, &selected);
    let build_tools = load_build_tools(repository, &request.target, &mut errors);
    let non_distributed = source_dependencies(repository, request.artifact, &mut errors);
    let sbom = render_component_sbom(
        repository,
        &request.target,
        request.artifact,
        &request.version,
        &selected,
        &non_distributed,
        &build_tools,
        &mut errors,
    );
    let sources = render_sources(
        repository,
        &request.target,
        request.artifact,
        &request.version,
        &request.source_revision,
        &selected,
        &non_distributed,
        &build_tools,
        None,
        &[],
        &mut errors,
    );
    let sbom_json = match pretty_json(&sbom) {
        Ok(json) => json,
        Err(error) => {
            errors.push(error);
            String::new()
        }
    };
    let sources_json = match pretty_json(&sources) {
        Ok(json) => json,
        Err(error) => {
            errors.push(error);
            String::new()
        }
    };
    let catalog_json = match into_markdown_converters::core_catalog_authority()
        .map_err(|error| format!("cannot generate core catalog authority: {error}"))
        .and_then(|authority| pretty_json(&authority))
    {
        Ok(json) => json,
        Err(error) => {
            errors.push(error);
            String::new()
        }
    };
    if !errors.is_empty() {
        return Err(sorted(errors));
    }
    Ok(ReleaseInputs {
        schema_version: SCHEMA_VERSION,
        target: request.target,
        component_ids: selected.iter().map(|component| component.id.clone()).collect(),
        notice: generated_file(NOTICE_PATH, project_notice),
        third_party_notices: generated_file(THIRD_PARTY_PATH, third_party),
        sbom: generated_file(SBOM_PATH, sbom_json),
        sources: generated_file(SOURCES_PATH, sources_json),
        core_catalog: generated_file(CORE_CATALOG_PATH, catalog_json),
    })
}

/// Verify an archive manifest without creating, extracting, or downloading the archive.
///
/// # Errors
///
/// Returns every sorted projection, ownership, hash, declaration, or build-evidence violation.
pub fn verify_archive_projection(
    repository: &Path,
    projection_json: &str,
) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let projection: Option<ArchiveProjection> =
        parse("archive projection", projection_json, &mut errors);
    let Some(projection) = projection else { return Err(sorted(errors)) };
    validate_header(projection.schema_version, &projection.target, &mut errors);
    validate_source_revision(&projection.source_revision, &mut errors);
    if projection.version.trim().is_empty() {
        errors.push("archive projection version is empty".to_owned());
    }

    let request = ReleaseRequest {
        schema_version: projection.schema_version,
        target: projection.target.clone(),
        artifact: ReleaseArtifact::Core,
        version: projection.version.clone(),
        source_revision: projection.source_revision.clone(),
        components: projection.components.clone(),
    };
    let request_json = match serde_json::to_string(&request) {
        Ok(json) => json,
        Err(error) => {
            errors.push(format!("cannot serialize internal release request: {error}"));
            String::new()
        }
    };
    let inputs = match generate_release_inputs(repository, &request_json) {
        Ok(inputs) => Some(inputs),
        Err(mut input_errors) => {
            errors.append(&mut input_errors);
            None
        }
    };

    if let Some((inventory, policy)) =
        load_authorities(repository, ReleaseArtifact::Core, &mut errors)
    {
        let selected = select_components(
            ReleaseArtifact::Core,
            &projection.components,
            &inventory,
            &policy,
            &mut errors,
        );
        materials::validate(repository, &projection, &selected, &mut errors);
    }

    validate_files(repository, &projection, inputs.as_ref(), &mut errors);
    ffmpeg::validate(repository, &projection, &mut errors);
    native::validate(
        repository,
        &projection.target,
        &projection.components,
        &projection.files,
        &projection.native_transformations,
        &mut errors,
    );
    models_fixtures::validate(repository, &projection.components, &projection.files, &mut errors);
    if errors.is_empty() { Ok(()) } else { Err(sorted(errors)) }
}

/// Generate publishable metadata for a final signed artifact projection.
///
/// # Errors
///
/// Returns every schema, component, ownership, source, or digest violation.
pub fn finalize_artifact_metadata(
    repository: &Path,
    projection_json: &str,
) -> Result<ArtifactMetadata, Vec<String>> {
    crate::audit(repository, true)?;
    let mut errors = Vec::new();
    let projection: Option<ArtifactProjection> =
        parse("artifact projection", projection_json, &mut errors);
    let Some(projection) = projection else { return Err(sorted(errors)) };
    validate_header(projection.schema_version, &projection.target, &mut errors);
    validate_source_revision(&projection.source_revision, &mut errors);
    if projection.version.trim().is_empty()
        || projection.bytes == 0
        || !is_sha256(&projection.sha256)
        || !safe_file_name(&projection.file_name)
    {
        errors.push("artifact identity lacks a safe name, version, size, or SHA-256".to_owned());
    }
    let Some((inventory, policy)) = load_authorities(repository, projection.artifact, &mut errors)
    else {
        return Err(sorted(errors));
    };
    let selected = select_components(
        projection.artifact,
        &projection.components,
        &inventory,
        &policy,
        &mut errors,
    );
    let expected: BTreeSet<_> = selected.iter().map(|item| item.id.as_str()).collect();
    let actual: BTreeSet<_> = projection.components.iter().map(String::as_str).collect();
    if actual.len() != projection.components.len() || actual != expected {
        errors.push("artifact component set differs from its authoritative closure".to_owned());
    }
    validate_artifact_files(&projection.files, &expected, &mut errors);
    let mut build_tools = load_build_tools(repository, &projection.target, &mut errors);
    bind_build_tool_executions(&mut build_tools, &projection.build_tools, &mut errors);
    let non_distributed = source_dependencies(repository, projection.artifact, &mut errors);
    let sources = render_sources(
        repository,
        &projection.target,
        projection.artifact,
        &projection.version,
        &projection.source_revision,
        &selected,
        &non_distributed,
        &build_tools,
        Some((&projection.file_name, projection.bytes, &projection.sha256)),
        &projection.files,
        &mut errors,
    );
    let project_notice = read(repository.join(NOTICE_PATH), &mut errors);
    let notices = render_notices(&project_notice, &selected);
    let sbom = render_artifact_sbom(
        repository,
        &projection.target,
        projection.artifact,
        &projection.version,
        &projection.file_name,
        projection.bytes,
        &projection.sha256,
        &selected,
        &non_distributed,
        &build_tools,
        &projection.files,
        &mut errors,
    );
    let sbom_json = pretty_json(&sbom).unwrap_or_else(|error| {
        errors.push(error);
        String::new()
    });
    let sources_json = pretty_json(&sources).unwrap_or_else(|error| {
        errors.push(error);
        String::new()
    });
    if !errors.is_empty() {
        return Err(sorted(errors));
    }
    Ok(ArtifactMetadata {
        schema_version: SCHEMA_VERSION,
        target: projection.target,
        artifact: projection.artifact.id().to_owned(),
        file_name: projection.file_name.clone(),
        sha256: projection.sha256,
        sbom: generated_file(&format!("{}.spdx.json", projection.file_name), sbom_json),
        sources: generated_file(&format!("{}.sources.json", projection.file_name), sources_json),
        third_party_notices: generated_file(
            &format!("{}.THIRD_PARTY_NOTICES.md", projection.file_name),
            notices,
        ),
    })
}

/// Generate the auditable Core and complete-offline release-set projection.
///
/// # Errors
///
/// Returns every target, artifact-set, checksum, or profile violation.
pub fn aggregate_release_set(request_json: &str) -> Result<ReleaseSetMetadata, Vec<String>> {
    let mut errors = Vec::new();
    let request: Option<ReleaseSetRequest> =
        parse("release-set request", request_json, &mut errors);
    let Some(mut request) = request else { return Err(sorted(errors)) };
    validate_header(request.schema_version, &request.target, &mut errors);
    validate_source_revision(&request.source_revision, &mut errors);
    let expected = BTreeSet::from([
        ReleaseArtifact::Core,
        ReleaseArtifact::OcrPlugin,
        ReleaseArtifact::MediaPlugin,
    ]);
    let actual: BTreeSet<_> = request.artifacts.iter().map(|item| item.artifact).collect();
    if actual != expected || request.artifacts.len() != expected.len() {
        errors.push(
            "release set must contain Core and each complete capability plugin once".to_owned(),
        );
    }
    if request.version.trim().is_empty() {
        errors.push("release set version is empty".to_owned());
    }
    let mut names = BTreeSet::new();
    for artifact in &request.artifacts {
        if !names.insert(artifact.file_name.as_str())
            || !safe_file_name(&artifact.file_name)
            || artifact.bytes == 0
            || artifact.components.is_empty()
            || [
                artifact.sha256.as_str(),
                artifact.sbom_sha256.as_str(),
                artifact.sources_sha256.as_str(),
                artifact.notices_sha256.as_str(),
            ]
            .into_iter()
            .any(|hash| !is_sha256(hash))
        {
            errors.push(format!(
                "release-set artifact {} has incomplete identity or sidecars",
                artifact.file_name
            ));
        }
    }
    if !errors.is_empty() {
        return Err(sorted(errors));
    }
    request.artifacts.sort_by_key(|item| item.artifact);
    let (manifest, sbom) = render_release_set(&request);
    let base = format!("into-markdown-{}-release-set", request.target);
    Ok(ReleaseSetMetadata {
        schema_version: SCHEMA_VERSION,
        target: request.target,
        release_set: generated_file(
            &format!("{base}.json"),
            pretty_json(&manifest).map_err(|error| vec![error])?,
        ),
        sbom: generated_file(
            &format!("{base}.spdx.json"),
            pretty_json(&sbom).map_err(|error| vec![error])?,
        ),
    })
}

fn validate_artifact_files(
    files: &[ArchiveFile],
    selected: &BTreeSet<&str>,
    errors: &mut Vec<String>,
) {
    if files.is_empty() {
        errors.push("artifact projection contains no members".to_owned());
        return;
    }
    let mut paths = BTreeSet::new();
    let mut owners = BTreeSet::new();
    for file in files {
        if !paths.insert(file.path.as_str())
            || !safe_path(&file.path)
            || !is_sha256(&file.sha256)
            || file.sha1.as_deref().is_none_or(|value| !is_sha1(value))
        {
            errors.push(format!(
                "artifact member {} has unsafe, duplicate, or incomplete digest identity",
                file.path
            ));
        }
        if file.bytes == 0 && !allowed_empty_vendor_source(file) {
            errors.push(format!("artifact member {} has an empty size", file.path));
        }
        for owner in file.component_id.iter().chain(file.embedded_components.iter()) {
            if !selected.contains(owner.as_str()) {
                errors.push(format!("artifact member {} has unknown owner {owner}", file.path));
            }
            owners.insert(owner.as_str());
        }
        if file.kind == ArchiveFileKind::Component && file.component_id.is_none() {
            errors.push(format!("artifact component member {} is orphaned", file.path));
        }
    }
    for component in selected.difference(&owners) {
        errors.push(format!("artifact component {component} owns no member"));
    }
}

fn allowed_empty_vendor_source(file: &ArchiveFile) -> bool {
    file.kind == ArchiveFileKind::Project
        && file.component_id.is_none()
        && file.embedded_components.is_empty()
        && file.path.starts_with("lib/into-markdown-rust/vendor/")
        && file.path.len() > "lib/into-markdown-rust/vendor/".len()
}

#[cfg(test)]
mod empty_vendor_source_tests {
    use super::{ArchiveFile, ArchiveFileKind, allowed_empty_vendor_source};

    fn file(path: &str, kind: ArchiveFileKind) -> ArchiveFile {
        ArchiveFile {
            path: path.into(),
            bytes: 0,
            sha1: Some("0".repeat(40)),
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            kind,
            component_id: None,
            embedded_components: vec![],
        }
    }

    #[test]
    fn only_project_files_inside_the_offline_vendor_tree_may_be_empty() {
        assert!(allowed_empty_vendor_source(&file(
            "lib/into-markdown-rust/vendor/vcpkg-0.2.15/test-data/empty.dll",
            ArchiveFileKind::Project,
        )));
        assert!(!allowed_empty_vendor_source(&file(
            "lib/into-markdown-rust/Cargo.toml",
            ArchiveFileKind::Project,
        )));
        assert!(!allowed_empty_vendor_source(&file(
            "lib/into-markdown-rust/vendor/vcpkg-0.2.15/test-data/empty.dll",
            ArchiveFileKind::Component,
        )));
    }
}

fn load_build_tools(repository: &Path, target: &str, errors: &mut Vec<String>) -> Vec<BuildTool> {
    let contents = read(repository.join("third_party/licenses/build-tools.json"), errors);
    let Some(inventory): Option<BuildToolInventory> =
        parse("build-tool authority", &contents, errors)
    else {
        return Vec::new();
    };
    if inventory.schema_version != SCHEMA_VERSION {
        errors.push("unsupported build-tool authority schema_version".to_owned());
    }
    let mut ids = BTreeSet::new();
    let mut result = Vec::new();
    for tool in inventory.tools {
        if !ids.insert(tool.id.clone())
            || !safe_id(&tool.id)
            || tool.version.trim().is_empty()
            || !tool.source.starts_with("https://")
            || tool.license.trim().is_empty()
            || tool.scope.trim().is_empty()
            || tool.integrity.is_empty()
            || tool.integrity.iter().any(|item| {
                item.algorithm != "SHA-256" || !is_sha256(&item.digest) || item.subject.is_empty()
            })
            || tool.targets.iter().any(|item| !SUPPORTED_TARGETS.contains(&item.as_str()))
        {
            errors.push(format!("build tool {} has incomplete authority", tool.id));
        }
        for evidence in &tool.integrity {
            let subject = repository.join(&evidence.subject);
            if !safe_path(&evidence.subject) {
                errors.push(format!(
                    "build tool {} has unsafe integrity subject {}",
                    tool.id, evidence.subject
                ));
                continue;
            }
            match fs::read(&subject) {
                Ok(contents) => {
                    let observed = format!("{:x}", Sha256::digest(contents));
                    if observed != evidence.digest {
                        errors.push(format!(
                            "build tool {} authority digest differs for {}",
                            tool.id, evidence.subject
                        ));
                    }
                }
                Err(error) => errors.push(format!(
                    "cannot read build tool {} authority {}: {error}",
                    tool.id,
                    subject.display()
                )),
            }
        }
        if tool.targets.iter().any(|item| item == target) {
            result.push(tool);
        }
    }
    if result.is_empty() {
        errors.push(format!("release target {target} has no build-tool authority"));
    }
    result.sort_by(|left, right| left.id.cmp(&right.id));
    result
}

fn source_dependencies(
    repository: &Path,
    artifact: ReleaseArtifact,
    errors: &mut Vec<String>,
) -> Vec<crate::schema::SourceComponent> {
    let mut result = Vec::new();
    if artifact == ReleaseArtifact::Core {
        let contents = read(repository.join("third_party/licenses/npm-inventory.json"), errors);
        result.extend(npm::source_dependencies(&contents, errors));
    }
    let contents =
        read(repository.join("third_party/licenses/non-distributed-sources.json"), errors);
    let Some(inventory): Option<SourceDependencyInventory> =
        parse("non-distributed source authority", &contents, errors)
    else {
        return result;
    };
    if inventory.schema_version != SCHEMA_VERSION {
        errors.push("unsupported non-distributed source authority schema_version".to_owned());
    }
    result.extend(inventory.components);
    let mut ids = BTreeSet::new();
    for component in &result {
        if !ids.insert(component.id.as_str())
            || !safe_id(&component.id)
            || component.kind.trim().is_empty()
            || component.version.trim().is_empty()
            || !component.source.starts_with("https://")
            || component.license.trim().is_empty()
            || !matches!(component.scope.as_str(), "build" | "test")
            || component.distributed
            || component.integrity.is_empty()
            || component.integrity.iter().any(|item| {
                !match item.algorithm.as_str() {
                    "SHA-256" => is_sha256(&item.digest),
                    "SRI-SHA-512" => item.digest.starts_with("sha512-"),
                    _ => false,
                } || item.subject.trim().is_empty()
            })
            || component.authority.trim().is_empty()
            || !component.files.is_empty()
        {
            errors
                .push(format!("non-distributed source {} has incomplete authority", component.id));
        }
    }
    result
}

fn bind_build_tool_executions(
    tools: &mut [BuildTool],
    executions: &[BuildToolExecutable],
    errors: &mut Vec<String>,
) {
    let mut by_id: BTreeMap<_, _> = tools.iter_mut().map(|tool| (tool.id.clone(), tool)).collect();
    let mut identities = BTreeSet::new();
    for execution in executions {
        if !identities.insert((execution.authority_id.as_str(), execution.name.as_str()))
            || !safe_file_name(&execution.name)
            || execution.version.trim().is_empty()
            || execution.bytes == 0
            || !is_sha256(&execution.sha256)
        {
            errors.push(format!(
                "build-tool execution {}:{} has incomplete identity",
                execution.authority_id, execution.name
            ));
            continue;
        }
        let Some(tool) = by_id.get_mut(&execution.authority_id) else {
            errors.push(format!(
                "build-tool execution {} has no target authority",
                execution.authority_id
            ));
            continue;
        };
        if matches!(tool.id.as_str(), "rust-toolchain" | "bazel" | "node" | "pnpm")
            && !execution.version.contains(&tool.version)
        {
            errors.push(format!(
                "build-tool execution {} version differs from authority {}",
                execution.authority_id, tool.version
            ));
        }
        tool.executables.push(execution.clone());
    }
    for (id, tool) in by_id {
        if tool.executables.is_empty() {
            errors.push(format!("build tool {id} has no observed executable"));
        }
        tool.executables.sort_by(|left, right| left.name.cmp(&right.name));
    }
}

fn validate_source_revision(revision: &str, errors: &mut Vec<String>) {
    if !(40..=64).contains(&revision.len())
        || revision.bytes().any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
    {
        errors.push("source_revision must be a lowercase Git object ID".to_owned());
    }
}

fn load_authorities(
    repository: &Path,
    artifact: ReleaseArtifact,
    errors: &mut Vec<String>,
) -> Option<(Inventory, Policy)> {
    let inventory_text = read(repository.join("third_party/licenses/inventory.json"), errors);
    let policy_text = read(repository.join("third_party/licenses/policy.json"), errors);
    let inventory: Option<Inventory> = parse("component inventory", &inventory_text, errors);
    let policy = parse("license policy", &policy_text, errors);
    match (inventory, policy) {
        (Some(mut inventory), Some(policy)) => {
            for component in &mut inventory.components {
                component.required_in_core = component.included_in_release;
            }
            enrich_inventory_evidence(repository, &mut inventory, errors);
            let cargo_lock = read(repository.join("Cargo.lock"), errors);
            let rust_approvals =
                read(repository.join("third_party/licenses/rust-lock.tsv"), errors);
            let cargo_normal_runtime =
                read(repository.join("third_party/licenses/cargo-normal-runtime.json"), errors);
            let npm_inventory =
                read(repository.join("third_party/licenses/npm-inventory.json"), errors);
            inventory.components.extend(rust::load(
                repository,
                &cargo_lock,
                &rust_approvals,
                &cargo_normal_runtime,
                artifact.cargo_root(),
                errors,
            ));
            if artifact == ReleaseArtifact::Core {
                inventory.components.extend(npm::load(&npm_inventory, errors));
            }
            Some((inventory, policy))
        }
        _ => None,
    }
}

fn enrich_inventory_evidence(
    repository: &Path,
    inventory: &mut Inventory,
    errors: &mut Vec<String>,
) {
    for component in &mut inventory.components {
        component.authority = format!("third_party/licenses/inventory.json#{}", component.id);
        let evidence_path = match component.id.as_str() {
            "onnxruntime-cpu" => Some("third_party/onnxruntime/manifest.json"),
            "pdfium" => Some("third_party/pdfium/manifest.json"),
            "ffmpeg" => Some("third_party/ffmpeg/source.json"),
            "ppocrv6-tiny-recognizer-onnx-model" | "ppocrv6-tiny-recognizer-character-table" => {
                Some("models/ppocrv6-tiny-recognizer-authority.json")
            }
            "ppocrv6-tiny-detector-onnx-model" => {
                Some("models/ppocrv6-tiny-detector-onnx-authority.json")
            }
            "whisper-small"
            | "silero-vad-half-onnx-model"
            | "3dspeaker-eres2net-base-onnx-model" => Some("models/manifest.json"),
            _ => None,
        };
        if let Some(evidence_path) = evidence_path {
            let _ = read(repository.join(evidence_path), errors);
            component.authority.push_str(" + ");
            component.authority.push_str(evidence_path);
        }
    }
}

fn select_components<'a>(
    artifact: ReleaseArtifact,
    requested: &[String],
    inventory: &'a Inventory,
    policy: &Policy,
    errors: &mut Vec<String>,
) -> Vec<&'a Component> {
    if inventory.schema_version != SCHEMA_VERSION || policy.schema_version != SCHEMA_VERSION {
        errors.push("unsupported component authority schema_version".to_owned());
    }
    let mut by_id = BTreeMap::new();
    for component in &inventory.components {
        if by_id.insert(component.id.as_str(), component).is_some() {
            errors.push(format!("duplicate authority component {}", component.id));
        }
    }
    validate_catalog_runtime_authority(inventory, &by_id, errors);
    let mut caller_ids = BTreeSet::new();
    for id in requested {
        if !caller_ids.insert(id.as_str()) {
            errors.push(format!("duplicate projected component {id}"));
        }
    }
    let mut selected = Vec::new();
    let mut requested: BTreeSet<&str> = inventory
        .components
        .iter()
        .filter(|component| {
            component.required_in_core
                && (artifact == ReleaseArtifact::Core || component.id.starts_with("cargo:"))
        })
        .map(|component| component.id.as_str())
        .chain(requested.iter().map(String::as_str))
        .collect();
    if artifact == ReleaseArtifact::OcrPlugin
        && requested.iter().any(|id| OCR_RUNTIME_COMPONENTS.contains(id))
    {
        requested.extend(OCR_RUNTIME_COMPONENTS);
    }
    for id in requested {
        if !safe_id(id) {
            errors.push(format!("unsafe component ID {id:?}"));
            continue;
        }
        let Some(component) = by_id.get(id).copied() else {
            errors.push(format!("unknown projected component {id}"));
            continue;
        };
        validate_component(component, policy, errors);
        selected.push(component);
    }
    selected.sort_by_key(|component| component.id.as_str());
    selected
}

const OCR_RUNTIME_COMPONENTS: [&str; 4] = [
    "onnxruntime-cpu",
    "ppocrv6-tiny-detector-onnx-model",
    "ppocrv6-tiny-recognizer-onnx-model",
    "ppocrv6-tiny-recognizer-character-table",
];

fn validate_catalog_runtime_authority(
    _inventory: &Inventory,
    by_id: &BTreeMap<&str, &Component>,
    errors: &mut Vec<String>,
) {
    let catalog: BTreeSet<_> = into_markdown_converters::core_capabilities()
        .iter()
        .filter_map(|capability| capability.runtime.map(|runtime| runtime.component))
        .collect();
    let expected = BTreeSet::from(["official.media.whisper", "official.ocr.ppocrv6", "pdfium"]);
    if catalog != expected {
        errors.push(format!(
            "core runtime catalog differs from license projection authority: {catalog:?}"
        ));
    }
    for id in OCR_RUNTIME_COMPONENTS.into_iter().chain([
        "3dspeaker-eres2net-base-onnx-model",
        "ffmpeg",
        "pdfium",
        "silero-vad-half-onnx-model",
        "whisper-small",
    ]) {
        if by_id
            .get(id)
            .is_none_or(|component| component.status != "reviewed" || !component.release_eligible)
        {
            errors.push(format!(
                "core or official capability plugin component {id} lacks reviewed release authority"
            ));
        }
    }
}

fn validate_component(component: &Component, policy: &Policy, errors: &mut Vec<String>) {
    if component.status != "reviewed" {
        errors.push(format!("projected component {} is not reviewed", component.id));
    }
    if !component.release_eligible {
        errors.push(format!("projected component {} is not release-eligible", component.id));
    }
    if component.manual_only {
        errors.push(format!("manual-only component {} cannot be released", component.id));
    }
    if matches!(component.kind.as_str(), "fixture-input" | "model-source" | "build-input") {
        errors.push(format!(
            "source/manual component {} cannot be projected as release content",
            component.id
        ));
    }
    for (field, value) in [
        ("kind", Some(component.kind.as_str())),
        ("version", component.version.as_deref()),
        ("source", component.source.as_deref()),
        ("license", component.license.as_deref()),
        ("obligations", component.obligations.as_deref()),
    ] {
        if value.is_none_or(|value| value.trim().is_empty()) {
            errors.push(format!("projected component {} lacks {field}", component.id));
        }
    }
    if let Some(source) = component.source.as_deref()
        && (!source.starts_with("https://") || source.contains('#') || source.contains('?'))
    {
        errors.push(format!("projected component {} has non-canonical source", component.id));
    }
    if let Some(license) = component.license.as_deref() {
        let terms: Vec<_> = license.split(" AND ").collect();
        if terms.is_empty()
            || terms.iter().any(|term| policy.denied.iter().any(|denied| denied == term))
            || terms.iter().any(|term| !policy.allowed.iter().any(|allowed| allowed == term))
        {
            errors.push(format!(
                "projected component {} has unknown or incompatible license {license}",
                component.id
            ));
        }
    }
}

fn validate_files(
    repository: &Path,
    projection: &ArchiveProjection,
    inputs: Option<&ReleaseInputs>,
    errors: &mut Vec<String>,
) {
    let mut paths = BTreeSet::new();
    let selected: BTreeSet<_> = projection.components.iter().map(String::as_str).collect();
    let mut owners = BTreeSet::new();
    for file in &projection.files {
        validate_file(file, &selected, errors);
        if !paths.insert(file.path.as_str()) {
            errors.push(format!("duplicate archive path {}", file.path));
        }
        if let Some(owner) = file.component_id.as_deref() {
            owners.insert(owner);
        }
        for owner in &file.embedded_components {
            owners.insert(owner);
        }
    }
    for component in selected.difference(&owners) {
        errors.push(format!("projected component {component} owns no archive file"));
    }
    let Some(inputs) = inputs else {
        for path in [
            LICENSE_PATH,
            NOTICE_PATH,
            THIRD_PARTY_PATH,
            SBOM_PATH,
            SOURCES_PATH,
            CORE_CATALOG_PATH,
        ] {
            if !projection.files.iter().any(|file| file.path == path) {
                errors.push(format!("archive is missing required declaration {path}"));
            }
        }
        return;
    };
    let expected_components: BTreeSet<_> =
        inputs.component_ids.iter().map(String::as_str).collect();
    if selected != expected_components {
        errors.push(
            "archive component projection omits or adds authoritative runtime components"
                .to_owned(),
        );
    }
    let license = read_bytes(repository.join(LICENSE_PATH), errors);
    let expected = [
        (LICENSE_PATH, license.len() as u64, digest(&license), ArchiveFileKind::Declaration),
        (
            NOTICE_PATH,
            inputs.notice.bytes,
            inputs.notice.sha256.clone(),
            ArchiveFileKind::Declaration,
        ),
        (
            THIRD_PARTY_PATH,
            inputs.third_party_notices.bytes,
            inputs.third_party_notices.sha256.clone(),
            ArchiveFileKind::Generated,
        ),
        (SBOM_PATH, inputs.sbom.bytes, inputs.sbom.sha256.clone(), ArchiveFileKind::Generated),
        (
            SOURCES_PATH,
            inputs.sources.bytes,
            inputs.sources.sha256.clone(),
            ArchiveFileKind::Generated,
        ),
        (
            CORE_CATALOG_PATH,
            inputs.core_catalog.bytes,
            inputs.core_catalog.sha256.clone(),
            ArchiveFileKind::Generated,
        ),
    ];
    for (path, bytes, sha256, kind) in expected {
        match projection.files.iter().find(|file| file.path == path) {
            Some(file)
                if file.bytes == bytes
                    && file.sha256 == sha256
                    && file.kind == kind
                    && file.component_id.is_none() => {}
            Some(_) => {
                errors.push(format!("archive declaration {path} does not match generated input"));
            }
            None => errors.push(format!("archive is missing required declaration {path}")),
        }
    }
}

fn validate_file(file: &ArchiveFile, selected: &BTreeSet<&str>, errors: &mut Vec<String>) {
    if !safe_path(&file.path) {
        errors.push(format!("unsafe archive path {:?}", file.path));
    }
    if !is_sha256(&file.sha256) {
        errors.push(format!("archive file {} lacks a fixed SHA-256", file.path));
    }
    let kind_path_is_valid = match file.kind {
        ArchiveFileKind::Project => {
            matches!(
                file.path.as_str(),
                "bin/into-md"
                    | "bin/into-md.exe"
                    | "bin/installed-smoke"
                    | "bin/installed-smoke.exe"
                    | "bin/archive-check"
                    | "bin/archive-check.exe"
                    | "bin/into-md-installer"
                    | "bin/into-md-installer.exe"
                    | "bin/onnxruntime-worker"
                    | "bin/onnxruntime-worker.exe"
                    | "share/into-markdown/plugins/official-publisher.json"
                    | "Install.ps1"
                    | "Uninstall.ps1"
                    | "install"
                    | "uninstall"
            ) || agent_skill_path(&file.path)
                || file.path.starts_with("lib/into-markdown-rust/")
                || (file.path.starts_with("bin/models/")
                    && file.path.ends_with("/install-state.json"))
                || file.path.starts_with("share/into-markdown/smoke/fixtures/")
        }
        ArchiveFileKind::Declaration => matches!(file.path.as_str(), LICENSE_PATH | NOTICE_PATH),
        ArchiveFileKind::Generated => {
            matches!(
                file.path.as_str(),
                THIRD_PARTY_PATH | SBOM_PATH | SOURCES_PATH | CORE_CATALOG_PATH
            )
        }
        ArchiveFileKind::LicenseMaterial => projection_material_path(&file.path),
        ArchiveFileKind::Component => true,
    };
    if !kind_path_is_valid {
        errors.push(format!(
            "archive file {} is outside the closed path set for {:?}",
            file.path, file.kind
        ));
    }
    match (file.kind, file.component_id.as_deref()) {
        (ArchiveFileKind::Component, Some(id)) if selected.contains(id) => {}
        (ArchiveFileKind::Component, Some(id)) => {
            errors.push(format!("archive file {} has unknown or unselected owner {id}", file.path));
        }
        (ArchiveFileKind::Component, None) => {
            errors.push(format!("archive component file {} is orphaned", file.path));
        }
        (
            ArchiveFileKind::Project
            | ArchiveFileKind::Declaration
            | ArchiveFileKind::Generated
            | ArchiveFileKind::LicenseMaterial,
            None,
        ) => {}
        (_, Some(id)) => errors
            .push(format!("archive non-component file {} has component owner {id}", file.path)),
    }
    if file.kind == ArchiveFileKind::Component
        && file.component_id.as_deref().is_some_and(embedded_only)
    {
        errors.push(format!(
            "embedded component cannot hide a standalone archive file {}",
            file.path
        ));
    }
    let mut embedded = BTreeSet::new();
    for id in &file.embedded_components {
        if file.kind != ArchiveFileKind::Project {
            errors.push(format!(
                "archive non-project file {} declares embedded components",
                file.path
            ));
        }
        if !embedded.insert(id.as_str()) {
            errors.push(format!("archive file {} duplicates embedded component {id}", file.path));
        }
        if !selected.contains(id.as_str()) {
            errors.push(format!(
                "archive file {} embeds unknown or unselected component {id}",
                file.path
            ));
        }
    }
}

fn agent_skill_path(path: &str) -> bool {
    matches!(
        path,
        "share/into-markdown/skills/into-markdown/LICENSE"
            | "share/into-markdown/skills/into-markdown/SKILL.md"
            | "share/into-markdown/skills/into-markdown/agents/openai.yaml"
            | "share/into-markdown/skills/into-markdown/references/cli-workflows.md"
    )
}

fn embedded_only(id: &str) -> bool {
    id.starts_with("cargo:")
        || id.starts_with("npm:")
        || matches!(
            id,
            "opencc-transcript-character-table"
                | "imageproc-contour-adaptation"
                | "clipper2-rust"
                | "calamine"
        )
}

fn projection_material_path(path: &str) -> bool {
    path.starts_with("share/into-markdown/licenses/")
        || path.starts_with("share/into-markdown/source/")
        || path.starts_with("share/into-markdown/relink/")
}

fn validate_header(version: u64, target: &str, errors: &mut Vec<String>) {
    if version != SCHEMA_VERSION {
        errors.push("unsupported release projection schema_version".to_owned());
    }
    if !SUPPORTED_TARGETS.contains(&target) {
        errors.push(format!("unsupported release target {target}"));
    }
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@' | b'/' | b'+')
        })
}

fn safe_path(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('\\')
        && !value.contains(':')
        && Path::new(value)
            .components()
            .all(|component| matches!(component, PathComponent::Normal(_)))
        && value.is_ascii()
}

fn safe_file_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_sha1(value: &str) -> bool {
    value.len() == 40
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parse<T: for<'de> serde::Deserialize<'de>>(
    name: &str,
    contents: &str,
    errors: &mut Vec<String>,
) -> Option<T> {
    serde_json::from_str(contents)
        .map_err(|error| errors.push(format!("invalid {name}: {error}")))
        .ok()
}

fn read(path: impl AsRef<Path>, errors: &mut Vec<String>) -> String {
    fs::read_to_string(path.as_ref())
        .map_err(|error| errors.push(format!("cannot read {}: {error}", path.as_ref().display())))
        .unwrap_or_default()
}

fn read_bytes(path: impl AsRef<Path>, errors: &mut Vec<String>) -> Vec<u8> {
    fs::read(path.as_ref())
        .map_err(|error| errors.push(format!("cannot read {}: {error}", path.as_ref().display())))
        .unwrap_or_default()
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sorted(mut errors: Vec<String>) -> Vec<String> {
    errors.sort();
    errors.dedup();
    errors
}

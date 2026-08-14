//! Platform-neutral release projection and archive-verification API.

use crate::sbom::{generated_file, pretty_json, render_notices, render_sbom};
use crate::schema::{
    ArchiveFile, ArchiveFileKind, ArchiveProjection, Component, Inventory, Policy, ReleaseInputs,
    ReleaseRequest, SCHEMA_VERSION, SUPPORTED_TARGETS,
};
use crate::{ffmpeg, materials, models_fixtures, native, npm, rust};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component as PathComponent, Path};

const LICENSE_PATH: &str = "LICENSE";
const NOTICE_PATH: &str = "NOTICE";
const THIRD_PARTY_PATH: &str = "THIRD_PARTY_NOTICES.md";
const SBOM_PATH: &str = "sbom-input.json";
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

fn generate_release_inputs_unchecked(
    repository: &Path,
    request_json: &str,
) -> Result<ReleaseInputs, Vec<String>> {
    let mut errors = Vec::new();
    let request: Option<ReleaseRequest> = parse("release request", request_json, &mut errors);
    let authorities = load_authorities(repository, &mut errors);
    let (Some(request), Some((inventory, policy))) = (request, authorities) else {
        return Err(sorted(errors));
    };
    validate_header(request.schema_version, &request.target, &mut errors);
    let selected = select_components(&request.components, &inventory, &policy, &mut errors);
    if !errors.is_empty() {
        return Err(sorted(errors));
    }

    let project_notice = read(repository.join(NOTICE_PATH), &mut errors);
    let third_party = render_notices(&project_notice, &selected);
    let sbom = render_sbom(repository, &request.target, &selected, &mut errors);
    let sbom_json = match pretty_json(&sbom) {
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
        sbom_input: generated_file(SBOM_PATH, sbom_json),
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

    let request = ReleaseRequest {
        schema_version: projection.schema_version,
        target: projection.target.clone(),
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

    if let Some((inventory, policy)) = load_authorities(repository, &mut errors) {
        let selected = select_components(&projection.components, &inventory, &policy, &mut errors);
        materials::validate(repository, &projection, &selected, &mut errors);
    }

    validate_files(repository, &projection, inputs.as_ref(), &mut errors);
    ffmpeg::validate(repository, &projection, &mut errors);
    native::validate(
        repository,
        &projection.target,
        &projection.components,
        &projection.files,
        &mut errors,
    );
    models_fixtures::validate(repository, &projection.components, &projection.files, &mut errors);
    if errors.is_empty() { Ok(()) } else { Err(sorted(errors)) }
}

fn load_authorities(repository: &Path, errors: &mut Vec<String>) -> Option<(Inventory, Policy)> {
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
                errors,
            ));
            inventory.components.extend(npm::load(&npm_inventory, errors));
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
        .filter(|component| component.required_in_core)
        .map(|component| component.id.as_str())
        .chain(requested.iter().map(String::as_str))
        .collect();
    if requested.iter().any(|id| OCR_RUNTIME_COMPONENTS.contains(id)) {
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
    inventory: &Inventory,
    by_id: &BTreeMap<&str, &Component>,
    errors: &mut Vec<String>,
) {
    let catalog: BTreeSet<_> = into_markdown_converters::core_capabilities()
        .iter()
        .filter_map(|capability| capability.runtime.map(|runtime| runtime.component))
        .collect();
    let expected = BTreeSet::from(["legacy-office", "onnxruntime", "pdfium"]);
    if catalog != expected {
        errors.push(format!(
            "core runtime catalog differs from license projection authority: {catalog:?}"
        ));
    }
    for id in OCR_RUNTIME_COMPONENTS.into_iter().chain(["pdfium"]) {
        if by_id
            .get(id)
            .is_none_or(|component| component.status != "reviewed" || !component.release_eligible)
        {
            errors.push(format!(
                "core runtime catalog component {id} lacks reviewed release authority"
            ));
        }
    }
    if inventory.components.iter().any(|component| component.id == "legacy-office") {
        errors.push(
            "project-owned legacy-office worker must not be disguised as third-party inventory"
                .to_owned(),
        );
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
        for path in [LICENSE_PATH, NOTICE_PATH, THIRD_PARTY_PATH, SBOM_PATH, CORE_CATALOG_PATH] {
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
        (
            SBOM_PATH,
            inputs.sbom_input.bytes,
            inputs.sbom_input.sha256.clone(),
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
    if file.bytes == 0 || !is_sha256(&file.sha256) {
        errors.push(format!("archive file {} lacks fixed size or SHA-256", file.path));
    }
    let kind_path_is_valid = match file.kind {
        ArchiveFileKind::Project => {
            matches!(file.path.as_str(), "bin/into-md" | "bin/into-md.exe")
                || file.path.starts_with("lib/into-markdown-rust/")
                || file.path.starts_with("share/into-markdown/smoke/fixtures/")
        }
        ArchiveFileKind::Declaration => matches!(file.path.as_str(), LICENSE_PATH | NOTICE_PATH),
        ArchiveFileKind::Generated => {
            matches!(file.path.as_str(), THIRD_PARTY_PATH | SBOM_PATH | CORE_CATALOG_PATH)
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

fn embedded_only(id: &str) -> bool {
    id.starts_with("cargo:")
        || id.starts_with("npm:")
        || matches!(id, "imageproc-contour-adaptation" | "clipper2-rust" | "calamine")
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

fn is_sha256(value: &str) -> bool {
    value.len() == 64
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

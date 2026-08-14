//! Platform-neutral release projection and archive-verification API.

use crate::sbom::{generated_file, pretty_json, render_notices, render_sbom};
use crate::schema::{
    ArchiveFile, ArchiveFileKind, ArchiveProjection, Component, Inventory, Policy, ReleaseInputs,
    ReleaseRequest, SCHEMA_VERSION, SUPPORTED_TARGETS,
};
use crate::{models_fixtures, native, npm, rust};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component as PathComponent, Path};

const LICENSE_PATH: &str = "LICENSE";
const NOTICE_PATH: &str = "NOTICE";
const THIRD_PARTY_PATH: &str = "THIRD_PARTY_NOTICES.md";
const SBOM_PATH: &str = "sbom-input.json";

pub(crate) fn audit_repository_contract(repository: &Path, errors: &mut Vec<String>) {
    let mut notices = BTreeSet::new();
    for target in SUPPORTED_TARGETS {
        let path = repository
            .join("tools/license-check/fixtures")
            .join(format!("release-request-{target}.json"));
        let request = read(path, errors);
        match generate_release_inputs(repository, &request) {
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
    let sbom = render_sbom(&request.target, &selected);
    let sbom_json = match pretty_json(&sbom) {
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

    validate_files(repository, &projection, inputs.as_ref(), &mut errors);
    validate_ffmpeg(&projection, &mut errors);
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
            let cargo_lock = read(repository.join("Cargo.lock"), errors);
            let rust_approvals =
                read(repository.join("third_party/licenses/rust-lock.tsv"), errors);
            let npm_inventory =
                read(repository.join("third_party/licenses/npm-inventory.json"), errors);
            inventory.components.extend(rust::load(&cargo_lock, &rust_approvals, errors));
            inventory.components.extend(npm::load(&npm_inventory, errors));
            Some((inventory, policy))
        }
        _ => None,
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
    let mut ids = BTreeSet::new();
    let mut selected = Vec::new();
    for id in requested {
        if !safe_id(id) {
            errors.push(format!("unsafe component ID {id:?}"));
            continue;
        }
        if !ids.insert(id.as_str()) {
            errors.push(format!("duplicate projected component {id}"));
            continue;
        }
        let Some(component) = by_id.get(id.as_str()).copied() else {
            errors.push(format!("unknown projected component {id}"));
            continue;
        };
        validate_component(component, policy, errors);
        selected.push(component);
    }
    selected.sort_by_key(|component| component.id.as_str());
    selected
}

fn validate_component(component: &Component, policy: &Policy, errors: &mut Vec<String>) {
    if component.status != "reviewed" {
        errors.push(format!("projected component {} is not reviewed", component.id));
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
        for path in [LICENSE_PATH, NOTICE_PATH, THIRD_PARTY_PATH, SBOM_PATH] {
            if !projection.files.iter().any(|file| file.path == path) {
                errors.push(format!("archive is missing required declaration {path}"));
            }
        }
        return;
    };
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
    if file.kind == ArchiveFileKind::Project && requires_component_classification(&file.path) {
        errors.push(format!(
            "archive binary/model/font {} cannot be classified as project-owned",
            file.path
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
            ArchiveFileKind::Project | ArchiveFileKind::Declaration | ArchiveFileKind::Generated,
            None,
        ) => {}
        (_, Some(id)) => errors
            .push(format!("archive non-component file {} has component owner {id}", file.path)),
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

fn requires_component_classification(path: &str) -> bool {
    if path.starts_with("bin/") {
        return !matches!(path, "bin/into-md" | "bin/into-md.exe");
    }
    let lower = path.to_ascii_lowercase();
    lower.starts_with("lib/")
        || [".dll", ".dylib", ".onnx", ".otf", ".so", ".ttf", ".wasm", ".woff", ".woff2"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
        || lower.contains(".so.")
}

fn validate_ffmpeg(projection: &ArchiveProjection, errors: &mut Vec<String>) {
    let selected = projection.components.iter().any(|id| id == "ffmpeg");
    let Some(evidence) = projection.ffmpeg_evidence.as_ref() else {
        if selected {
            errors.push("FFmpeg is present without LGPL-compatible build evidence".to_owned());
        }
        return;
    };
    if !selected {
        errors.push("FFmpeg build evidence is orphaned".to_owned());
        return;
    }
    let binary = projection.files.iter().find(|file| file.path == evidence.executable_path);
    if evidence.schema_version != SCHEMA_VERSION
        || evidence.ffmpeg_version != "8.1.2"
        || evidence.target != projection.target
        || binary.is_none_or(|file| {
            file.kind != ArchiveFileKind::Component
                || file.component_id.as_deref() != Some("ffmpeg")
                || file.bytes != evidence.executable_bytes
                || file.sha256 != evidence.executable_sha256
        })
    {
        errors.push("FFmpeg build evidence is not bound to the projected binary".to_owned());
    }
    let flags: BTreeSet<_> = evidence.configure.iter().map(String::as_str).collect();
    for required in [
        "--disable-everything",
        "--disable-gpl",
        "--disable-version3",
        "--disable-nonfree",
        "--disable-network",
        "--disable-autodetect",
        "--disable-shared",
        "--enable-static",
    ] {
        if !flags.contains(required) {
            errors.push(format!("FFmpeg build evidence lacks {required}"));
        }
    }
    if evidence.configure.iter().any(|flag| {
        flag == "--enable-gpl"
            || flag == "--enable-version3"
            || flag == "--enable-nonfree"
            || flag.starts_with("--enable-lib")
    }) {
        errors.push("FFmpeg build evidence enables incompatible or external components".to_owned());
    }
    let actual_dependencies: BTreeSet<_> =
        evidence.dependencies.iter().map(String::as_str).collect();
    let expected_dependencies: BTreeSet<_> = match projection.target.as_str() {
        "aarch64-apple-darwin" => [
            "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation",
            "/System/Library/Frameworks/CoreMedia.framework/Versions/A/CoreMedia",
            "/System/Library/Frameworks/CoreVideo.framework/Versions/A/CoreVideo",
            "/usr/lib/libSystem.B.dylib",
        ]
        .into_iter()
        .collect(),
        "aarch64-unknown-linux-gnu" | "x86_64-unknown-linux-gnu" => {
            ["libc.so.6", "libm.so.6", "libpthread.so.0"].into_iter().collect()
        }
        "x86_64-pc-windows-msvc" => {
            ["ADVAPI32.dll", "KERNEL32.dll", "OLE32.dll", "USER32.dll"].into_iter().collect()
        }
        _ => BTreeSet::new(),
    };
    if actual_dependencies != expected_dependencies {
        errors.push("FFmpeg build evidence has unreviewed dynamic dependencies".to_owned());
    }
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
                || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@' | b'/')
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

//! `FFmpeg` build-authority and LGPL-compatible policy verification.

use crate::schema::{ArchiveFileKind, ArchiveProjection, FfmpegEvidence, SCHEMA_VERSION};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildAuthority {
    schema_version: u64,
    ffmpeg_version: String,
    target: String,
    executable_bytes: u64,
    executable_sha256: String,
    configure: Vec<String>,
    binary_format: String,
    binary_architecture: String,
    dependencies: Vec<String>,
    toolchain: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildPolicy {
    schema_version: u64,
    ffmpeg_version: String,
    required_flags: Vec<String>,
    targets: BTreeMap<String, TargetPolicy>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetPolicy {
    binary_format: String,
    binary_architecture: String,
    additional_flags: Vec<String>,
    dynamic_dependencies: Vec<String>,
}

pub(crate) fn audit_repository(repository: &Path, errors: &mut Vec<String>) {
    let policy: BuildPolicy = match read(repository, "third_party/ffmpeg/build-policy.json")
        .and_then(|contents| {
            serde_json::from_str(&contents)
                .map_err(|error| format!("invalid FFmpeg build policy: {error}"))
        }) {
        Ok(value) => value,
        Err(error) => {
            errors.push(error);
            return;
        }
    };
    let source: serde_json::Value = serde_json::from_str(
        &read(repository, "third_party/ffmpeg/source.json").unwrap_or_default(),
    )
    .unwrap_or_default();
    let targets: BTreeSet<_> = policy.targets.keys().map(String::as_str).collect();
    let expected: BTreeSet<_> = crate::schema::SUPPORTED_TARGETS.into_iter().collect();
    if policy.schema_version != SCHEMA_VERSION
        || targets != expected
        || source.get("version").and_then(serde_json::Value::as_str)
            != Some(policy.ffmpeg_version.as_str())
    {
        errors.push("FFmpeg build policy does not cover exact source/target authority".to_owned());
    }
    let flags: BTreeSet<_> = policy.required_flags.iter().map(String::as_str).collect();
    if flags.len() != policy.required_flags.len()
        || ![
            "--disable-gpl",
            "--disable-version3",
            "--disable-nonfree",
            "--disable-network",
            "--disable-autodetect",
            "--disable-shared",
            "--enable-static",
        ]
        .into_iter()
        .all(|flag| flags.contains(flag))
    {
        errors.push("FFmpeg build policy lacks unique LGPL-compatible constraints".to_owned());
    }
}

pub(crate) fn validate(
    repository: &Path,
    projection: &ArchiveProjection,
    errors: &mut Vec<String>,
) {
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
    validate_bound_authority(projection, evidence, errors);
    let authority: BuildAuthority = match serde_json::from_str(&evidence.authority_contents) {
        Ok(value) => value,
        Err(error) => {
            errors.push(format!("invalid archived FFmpeg build authority: {error}"));
            return;
        }
    };
    let policy: BuildPolicy = match read(repository, "third_party/ffmpeg/build-policy.json")
        .and_then(|contents| {
            serde_json::from_str(&contents).map_err(|e| format!("invalid FFmpeg build policy: {e}"))
        }) {
        Ok(value) => value,
        Err(error) => {
            errors.push(error);
            return;
        }
    };
    let source: serde_json::Value = serde_json::from_str(
        &read(repository, "third_party/ffmpeg/source.json").unwrap_or_default(),
    )
    .unwrap_or_default();
    if policy.schema_version != SCHEMA_VERSION
        || authority.schema_version != SCHEMA_VERSION
        || authority.ffmpeg_version != policy.ffmpeg_version
        || source.get("version").and_then(serde_json::Value::as_str)
            != Some(policy.ffmpeg_version.as_str())
    {
        errors.push("FFmpeg authority, source, and build-policy versions disagree".to_owned());
    }
    if !matches_evidence(&authority, evidence) {
        errors.push("FFmpeg projection fields differ from archived build authority".to_owned());
    }
    let Some(target_policy) = policy.targets.get(&projection.target) else {
        errors.push(format!("FFmpeg build policy lacks target {}", projection.target));
        return;
    };
    validate_target(projection, &authority, &policy, target_policy, errors);
}

fn validate_bound_authority(
    projection: &ArchiveProjection,
    evidence: &FfmpegEvidence,
    errors: &mut Vec<String>,
) {
    let expected = format!("share/into-markdown/authority/ffmpeg-{}.json", projection.target);
    let digest = format!("{:x}", Sha256::digest(evidence.authority_contents.as_bytes()));
    let authority = projection.files.iter().find(|file| file.path == evidence.authority_path);
    if evidence.authority_path != expected
        || evidence.authority_bytes != evidence.authority_contents.len() as u64
        || evidence.authority_sha256 != digest
        || authority.is_none_or(|file| {
            file.kind != ArchiveFileKind::Component
                || file.component_id.as_deref() != Some("ffmpeg")
                || file.bytes != evidence.authority_bytes
                || file.sha256 != evidence.authority_sha256
        })
    {
        errors.push(
            "FFmpeg build evidence is not content-bound to its archived authority".to_owned(),
        );
    }
}

fn matches_evidence(authority: &BuildAuthority, evidence: &FfmpegEvidence) -> bool {
    authority.schema_version == evidence.schema_version
        && authority.ffmpeg_version == evidence.ffmpeg_version
        && authority.target == evidence.target
        && authority.executable_bytes == evidence.executable_bytes
        && authority.executable_sha256 == evidence.executable_sha256
        && authority.configure == evidence.configure
        && authority.dependencies == evidence.dependencies
        && authority.binary_format == evidence.binary_format
        && authority.binary_architecture == evidence.binary_architecture
        && authority.toolchain == evidence.toolchain
}

fn validate_target(
    projection: &ArchiveProjection,
    authority: &BuildAuthority,
    policy: &BuildPolicy,
    target_policy: &TargetPolicy,
    errors: &mut Vec<String>,
) {
    let executable = projection
        .ffmpeg_evidence
        .as_ref()
        .map(|item| item.executable_path.as_str())
        .unwrap_or_default();
    let binary = projection.files.iter().find(|file| file.path == executable);
    let authority_path = projection
        .ffmpeg_evidence
        .as_ref()
        .map(|item| item.authority_path.as_str())
        .unwrap_or_default();
    for file in
        projection.files.iter().filter(|file| file.component_id.as_deref() == Some("ffmpeg"))
    {
        if file.path != executable && file.path != authority_path {
            errors.push(format!(
                "FFmpeg component contains file outside binary/build authority: {}",
                file.path
            ));
        }
    }
    if authority.target != projection.target
        || binary.is_none_or(|file| {
            file.kind != ArchiveFileKind::Component
                || file.component_id.as_deref() != Some("ffmpeg")
                || file.bytes != authority.executable_bytes
                || file.sha256 != authority.executable_sha256
        })
    {
        errors.push("FFmpeg build authority is not bound to the projected binary".to_owned());
    }
    let actual: BTreeSet<_> = authority.configure.iter().map(String::as_str).collect();
    let mut expected: BTreeSet<_> = policy.required_flags.iter().map(String::as_str).collect();
    expected.extend(target_policy.additional_flags.iter().map(String::as_str));
    expected.insert("--prefix=/opt/into-markdown/ffmpeg");
    if actual.len() != authority.configure.len() || actual != expected {
        errors.push("FFmpeg configure arguments differ from reviewed build policy".to_owned());
    }
    let dependencies: BTreeSet<_> = authority.dependencies.iter().map(String::as_str).collect();
    let expected_dependencies: BTreeSet<_> =
        target_policy.dynamic_dependencies.iter().map(String::as_str).collect();
    if dependencies.len() != authority.dependencies.len() || dependencies != expected_dependencies {
        errors.push("FFmpeg dynamic dependencies differ from reviewed build policy".to_owned());
    }
    if authority.binary_format != target_policy.binary_format
        || authority.binary_architecture != target_policy.binary_architecture
        || authority.toolchain.trim().is_empty()
    {
        errors.push("FFmpeg binary identity or toolchain is not reviewed for target".to_owned());
    }
}

fn read(repository: &Path, path: &str) -> Result<String, String> {
    fs::read_to_string(repository.join(path))
        .map_err(|error| format!("cannot read {path}: {error}"))
}

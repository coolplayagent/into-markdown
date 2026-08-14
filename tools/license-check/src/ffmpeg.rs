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
    source_sha256: String,
    source_signature_sha256: String,
    signing_key_fingerprint: String,
    build_policy_sha256: String,
    config_log_sha256: String,
    relink_bytes: u64,
    relink_sha256: String,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildApprovals {
    schema_version: u64,
    ffmpeg_version: String,
    targets: BTreeMap<String, Option<ApprovedBuild>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovedBuild {
    authority_sha256: String,
    executable_bytes: u64,
    executable_sha256: String,
    config_log_sha256: String,
    relink_bytes: u64,
    relink_sha256: String,
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
    let approvals = load_approvals(repository, errors);
    if approvals.as_ref().is_some_and(|approvals| {
        approvals.schema_version != SCHEMA_VERSION
            || approvals.ffmpeg_version != policy.ffmpeg_version
            || approvals.targets.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected
    }) {
        errors.push("FFmpeg build approvals do not cover exact source/target authority".to_owned());
    }
    if approvals.as_ref().is_some_and(|approvals| {
        approvals.targets.values().flatten().any(|approved| !approved_fields_valid(approved))
    }) {
        errors.push("FFmpeg build approval lacks fixed artifact evidence".to_owned());
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
    if !policy_is_lgpl_compatible(&policy) {
        errors
            .push("FFmpeg build policy contains incompatible or external enable flags".to_owned());
    }
}

fn policy_is_lgpl_compatible(policy: &BuildPolicy) -> bool {
    let allowed: BTreeSet<_> = [
        "--disable-everything",
        "--disable-gpl",
        "--disable-version3",
        "--disable-nonfree",
        "--disable-network",
        "--disable-autodetect",
        "--disable-programs",
        "--enable-ffmpeg",
        "--disable-ffprobe",
        "--disable-doc",
        "--disable-debug",
        "--disable-devices",
        "--disable-avdevice",
        "--disable-swscale",
        "--enable-avutil",
        "--enable-avcodec",
        "--enable-avformat",
        "--enable-avfilter",
        "--enable-swresample",
        "--enable-protocol=file,pipe",
        "--enable-demuxer=aac,avi,flac,matroska,mov,mp3,mpegts,ogg,wav",
        "--enable-decoder=aac,flac,mp3,opus,vorbis,pcm_s8,pcm_s16be,pcm_s16le,pcm_s24be,pcm_s24le,pcm_s32be,pcm_s32le,pcm_f32be,pcm_f32le,pcm_f64be,pcm_f64le",
        "--enable-parser=aac,mpegaudio,opus,vorbis",
        "--enable-filter=aformat,aresample",
        "--enable-encoder=pcm_s16le",
        "--enable-muxer=pcm_s16le",
        "--enable-static",
        "--disable-shared",
        "--toolchain=msvc",
        "--disable-x86asm",
    ]
    .into_iter()
    .collect();
    let flags_are_allowed = policy
        .required_flags
        .iter()
        .chain(policy.targets.values().flat_map(|target| &target.additional_flags))
        .all(|flag| allowed.contains(flag.as_str()));
    let target_flags_are_exact = policy.targets.iter().all(|(target, value)| {
        let actual: BTreeSet<_> = value.additional_flags.iter().map(String::as_str).collect();
        let expected: BTreeSet<_> = if target == "x86_64-pc-windows-msvc" {
            ["--toolchain=msvc", "--disable-x86asm"].into_iter().collect()
        } else {
            BTreeSet::new()
        };
        actual.len() == value.additional_flags.len() && actual == expected
    });
    flags_are_allowed && target_flags_are_exact
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
    let approvals = load_approvals(repository, errors);
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
    let policy_bytes =
        fs::read(repository.join("third_party/ffmpeg/build-policy.json")).unwrap_or_default();
    let policy_hash = format!("{:x}", Sha256::digest(&policy_bytes));
    if source.get("source_sha256").and_then(serde_json::Value::as_str)
        != Some(authority.source_sha256.as_str())
        || source.get("signature_sha256").and_then(serde_json::Value::as_str)
            != Some(authority.source_signature_sha256.as_str())
        || source.get("signing_key_fingerprint").and_then(serde_json::Value::as_str)
            != Some(authority.signing_key_fingerprint.as_str())
        || authority.build_policy_sha256 != policy_hash
    {
        errors.push(
            "FFmpeg build authority is not bound to source signature and policy bytes".to_owned(),
        );
    }
    if authority.config_log_sha256.len() != 64
        || authority.relink_sha256.len() != 64
        || authority.relink_bytes == 0
    {
        errors.push("FFmpeg build authority lacks config.log or relink output evidence".to_owned());
    }
    if !matches_evidence(&authority, evidence) {
        errors.push("FFmpeg projection fields differ from archived build authority".to_owned());
    }
    match approvals
        .as_ref()
        .and_then(|approvals| approvals.targets.get(&projection.target))
        .and_then(Option::as_ref)
    {
        Some(approved) if approved_matches(approved, evidence) => {}
        Some(_) => errors.push("FFmpeg evidence differs from repository-approved build".to_owned()),
        None => errors.push(format!(
            "FFmpeg target {} has no repository-approved build evidence",
            projection.target
        )),
    }
    let Some(target_policy) = policy.targets.get(&projection.target) else {
        errors.push(format!("FFmpeg build policy lacks target {}", projection.target));
        return;
    };
    validate_target(projection, &authority, &policy, target_policy, errors);
}

fn approved_matches(approved: &ApprovedBuild, evidence: &FfmpegEvidence) -> bool {
    approved_fields_valid(approved)
        && approved.authority_sha256 == evidence.authority_sha256
        && approved.executable_bytes == evidence.executable_bytes
        && approved.executable_sha256 == evidence.executable_sha256
        && approved.config_log_sha256 == evidence.config_log_sha256
        && approved.relink_bytes == evidence.relink_bytes
        && approved.relink_sha256 == evidence.relink_sha256
}

fn approved_fields_valid(approved: &ApprovedBuild) -> bool {
    approved.executable_bytes > 0
        && approved.relink_bytes > 0
        && [
            approved.authority_sha256.as_str(),
            approved.executable_sha256.as_str(),
            approved.config_log_sha256.as_str(),
            approved.relink_sha256.as_str(),
        ]
        .into_iter()
        .all(|value| {
            value.len() == 64
                && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

fn load_approvals(repository: &Path, errors: &mut Vec<String>) -> Option<BuildApprovals> {
    read(repository, "third_party/ffmpeg/build-approvals.json")
        .and_then(|contents| {
            serde_json::from_str(&contents)
                .map_err(|error| format!("invalid FFmpeg build approvals: {error}"))
        })
        .map_err(|error| errors.push(error))
        .ok()
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
        && authority.source_sha256 == evidence.source_sha256
        && authority.source_signature_sha256 == evidence.source_signature_sha256
        && authority.signing_key_fingerprint == evidence.signing_key_fingerprint
        && authority.build_policy_sha256 == evidence.build_policy_sha256
        && authority.config_log_sha256 == evidence.config_log_sha256
        && authority.relink_bytes == evidence.relink_bytes
        && authority.relink_sha256 == evidence.relink_sha256
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

#[cfg(test)]
mod tests {
    use super::{BuildPolicy, policy_is_lgpl_compatible};

    #[test]
    fn repository_policy_rejects_incompatible_and_external_flags() {
        let policy_text = std::fs::read_to_string(
            crate::repository_root().unwrap().join("third_party/ffmpeg/build-policy.json"),
        )
        .unwrap();
        let mut policy: BuildPolicy = serde_json::from_str(&policy_text).unwrap();
        assert!(policy_is_lgpl_compatible(&policy));
        for flag in [
            "--enable-gpl",
            "--enable-nonfree",
            "--enable-libx264",
            "--enable-openssl",
            "--enable-gnutls",
            "--enable-cuda-nvcc",
            "--enable-vulkan",
            "--enable-vaapi",
            "--enable-amf",
            "--extra-ldflags=-lcrypto",
            "--disable-unreviewed-component",
        ] {
            policy.required_flags.push(flag.to_owned());
            assert!(!policy_is_lgpl_compatible(&policy));
            policy.required_flags.pop();
        }
    }
}

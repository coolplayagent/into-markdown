//! Package-external authority for generated declarations and release materials.

use crate::schema::{GeneratedFile, ReleaseInputs};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

const AUTHORITY_PATH: &str = "third_party/licenses/release-material-authority.json";
const REVIEWED_AUTHORITY_SHA256: &str =
    "51e0f32869832c8ec8b102a37404cbdaf18fdbabc0f93e0e4cedc67a7b297063";
const GENERATED_PATHS: [&str; 5] =
    ["NOTICE", "THIRD_PARTY_NOTICES.md", "SBOM.spdx.json", "SOURCES.json", "core-catalog.json"];
const PROFILE_PATHS: [&str; 12] = [
    "tools/license-check/fixtures/release-request-aarch64-apple-darwin.json",
    "tools/license-check/fixtures/release-request-aarch64-unknown-linux-gnu.json",
    "tools/license-check/fixtures/release-request-media-plugin-aarch64-apple-darwin.json",
    "tools/license-check/fixtures/release-request-media-plugin-aarch64-unknown-linux-gnu.json",
    "tools/license-check/fixtures/release-request-media-plugin-x86_64-pc-windows-msvc.json",
    "tools/license-check/fixtures/release-request-media-plugin-x86_64-unknown-linux-gnu.json",
    "tools/license-check/fixtures/release-request-ocr-plugin-aarch64-apple-darwin.json",
    "tools/license-check/fixtures/release-request-ocr-plugin-aarch64-unknown-linux-gnu.json",
    "tools/license-check/fixtures/release-request-ocr-plugin-x86_64-pc-windows-msvc.json",
    "tools/license-check/fixtures/release-request-ocr-plugin-x86_64-unknown-linux-gnu.json",
    "tools/license-check/fixtures/release-request-x86_64-pc-windows-msvc.json",
    "tools/license-check/fixtures/release-request-x86_64-unknown-linux-gnu.json",
];
const RENDERER_PATHS: [&str; 6] = [
    "tools/license-check/src/release.rs",
    "tools/license-check/src/materials.rs",
    "tools/license-check/src/sbom.rs",
    "tools/deterministic_zip.py",
    "tools/macos-release/release.py",
    "tools/platform-release/release.py",
];
const LICENSE_PATHS: [(&str, &str); 2] = [
    ("third_party/whisper-rs-0.16.0/LICENSE", "Unlicense"),
    ("third_party/whisper-rs-0.16.0/sys/whisper.cpp/LICENSE", "MIT"),
];

pub(crate) fn profile_paths() -> &'static [&'static str] {
    &PROFILE_PATHS
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WhisperRsSysAuthority {
    pub component_id: String,
    pub archive_path: String,
    pub source_directory: String,
    pub archive_format: String,
    pub bytes: u64,
    pub sha256: String,
    pub crates_io_sha256: String,
    pub tree_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseMaterialAuthority {
    schema_version: u64,
    renderers: Vec<FileAuthority>,
    licenses: Vec<LicenseAuthority>,
    whisper_rs_sys: WhisperRsSysAuthority,
    profiles: Vec<ProfileAuthority>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileAuthority {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LicenseAuthority {
    path: String,
    spdx: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileAuthority {
    request: String,
    files: Vec<FileAuthority>,
}

pub(crate) fn whisper_rs_sys(
    repository: &Path,
    errors: &mut Vec<String>,
) -> Option<WhisperRsSysAuthority> {
    load(repository, errors).map(|authority| authority.whisper_rs_sys)
}

pub(crate) fn validate_profiles(
    repository: &Path,
    generated: &BTreeMap<String, ReleaseInputs>,
    errors: &mut Vec<String>,
) {
    let Some(authority) = load(repository, errors) else { return };
    let expected_profiles: BTreeSet<_> = PROFILE_PATHS.iter().copied().collect();
    let actual_profiles: BTreeSet<_> =
        authority.profiles.iter().map(|profile| profile.request.as_str()).collect();
    if actual_profiles.len() != authority.profiles.len() || actual_profiles != expected_profiles {
        errors.push("release material authority does not bind the exact profile set".to_owned());
        return;
    }
    if generated.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected_profiles {
        errors.push("generated release material profile set is incomplete".to_owned());
        return;
    }
    for profile in &authority.profiles {
        let Some(inputs) = generated.get(&profile.request) else { continue };
        let files = generated_files(inputs);
        if profile.files.len() != GENERATED_PATHS.len()
            || profile.files.iter().zip(GENERATED_PATHS).any(|(record, path)| record.path != path)
        {
            errors.push(format!(
                "release material profile {} has an invalid file inventory",
                profile.request
            ));
            continue;
        }
        for (record, actual) in profile.files.iter().zip(files) {
            if record.bytes != actual.bytes || record.sha256 != actual.sha256 {
                errors.push(format!(
                    "release material profile {} drifted for {}",
                    profile.request, record.path
                ));
            }
        }
    }
}

fn load(repository: &Path, errors: &mut Vec<String>) -> Option<ReleaseMaterialAuthority> {
    let path = repository.join(AUTHORITY_PATH);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            errors.push(format!("cannot read release material authority: {error}"));
            return None;
        }
    };
    if sha256(&bytes) != REVIEWED_AUTHORITY_SHA256 {
        errors.push("release material authority bytes are not reviewed".to_owned());
        return None;
    }
    let authority: ReleaseMaterialAuthority = match serde_json::from_slice(&bytes) {
        Ok(authority) => authority,
        Err(error) => {
            errors.push(format!("invalid release material authority: {error}"));
            return None;
        }
    };
    validate_static(repository, &authority, errors);
    Some(authority)
}

fn validate_static(
    repository: &Path,
    authority: &ReleaseMaterialAuthority,
    errors: &mut Vec<String>,
) {
    if authority.schema_version != 1 {
        errors.push("unsupported release material authority schema".to_owned());
    }
    validate_files(repository, &authority.renderers, &RENDERER_PATHS, "renderer", errors);
    let license_paths: Vec<_> = LICENSE_PATHS.iter().map(|(path, _)| *path).collect();
    let license_files: Vec<_> = authority
        .licenses
        .iter()
        .map(|license| FileAuthority {
            path: license.path.clone(),
            bytes: license.bytes,
            sha256: license.sha256.clone(),
        })
        .collect();
    validate_files(repository, &license_files, &license_paths, "license", errors);
    if authority.licenses.len() != LICENSE_PATHS.len()
        || authority
            .licenses
            .iter()
            .zip(LICENSE_PATHS)
            .any(|(license, (path, spdx))| license.path != path || license.spdx != spdx)
    {
        errors.push("release material license authority is not exact".to_owned());
    }
    let source = &authority.whisper_rs_sys;
    if source.component_id != "cargo:whisper-rs-sys@0.15.0"
        || source.archive_path
            != "share/into-markdown/licenses/cargo/whisper-rs-sys-0.15.0-vendored.zip"
        || source.source_directory != "third_party/whisper-rs-0.16.0/sys"
        || source.archive_format != "deterministic-zip-stored-v1"
        || source.bytes == 0
        || !is_sha256(&source.sha256)
        || !is_sha256(&source.crates_io_sha256)
        || !is_sha256(&source.tree_sha256)
    {
        errors.push("whisper-rs-sys release source authority is incomplete".to_owned());
    }
    let patch: serde_json::Value = serde_json::from_slice(
        &fs::read(repository.join("third_party/whisper-rs-0.16.0/PATCH-AUTHORITY.json"))
            .unwrap_or_default(),
    )
    .unwrap_or_default();
    if patch.get("whisper_rs_sys_crates_io_sha256").and_then(serde_json::Value::as_str)
        != Some(source.crates_io_sha256.as_str())
        || patch.get("tree_sha256").and_then(serde_json::Value::as_str)
            != Some(source.tree_sha256.as_str())
    {
        errors.push("whisper-rs-sys source and patch authorities disagree".to_owned());
    }
}

fn validate_files(
    repository: &Path,
    files: &[FileAuthority],
    expected_paths: &[&str],
    kind: &str,
    errors: &mut Vec<String>,
) {
    if files.len() != expected_paths.len()
        || files.iter().zip(expected_paths).any(|(file, path)| file.path != *path)
    {
        errors.push(format!("release material {kind} authority is not exact"));
        return;
    }
    for file in files {
        let bytes = fs::read(repository.join(&file.path)).unwrap_or_default();
        if bytes.len() as u64 != file.bytes || sha256(&bytes) != file.sha256 {
            errors.push(format!("release material {kind} authority drifted for {}", file.path));
        }
    }
}

fn generated_files(inputs: &ReleaseInputs) -> [&GeneratedFile; 5] {
    [
        &inputs.notice,
        &inputs.third_party_notices,
        &inputs.sbom,
        &inputs.sources,
        &inputs.core_catalog,
    ]
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

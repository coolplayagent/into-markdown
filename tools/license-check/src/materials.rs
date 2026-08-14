//! Typed archive license-material verification without archive materialization.

use crate::schema::{ArchiveFileKind, ArchiveProjection, Component, LicenseMaterialKind};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

pub(crate) fn validate(
    repository: &Path,
    projection: &ArchiveProjection,
    components: &[&Component],
    errors: &mut Vec<String>,
) {
    let selected: BTreeSet<_> = components.iter().map(|item| item.id.as_str()).collect();
    let licenses: BTreeMap<_, _> = components
        .iter()
        .map(|item| (item.id.as_str(), item.license.as_deref().unwrap_or_default()))
        .collect();
    let mut covered = BTreeSet::new();
    let declared_paths: BTreeSet<_> =
        projection.license_materials.iter().map(|item| item.path.as_str()).collect();
    for file in projection.files.iter().filter(|file| file.kind == ArchiveFileKind::LicenseMaterial)
    {
        if !declared_paths.contains(file.path.as_str()) {
            errors
                .push(format!("archived license material {} has no typed declaration", file.path));
        }
    }
    for material in &projection.license_materials {
        let file = projection.files.iter().find(|file| file.path == material.path);
        if file.is_none_or(|file| {
            file.kind != ArchiveFileKind::LicenseMaterial
                || file.component_id.is_some()
                || file.bytes != material.bytes
                || file.sha256 != material.sha256
        }) {
            errors.push(format!(
                "license material {} is not bound to an archived file",
                material.path
            ));
        }
        if material.component_ids.is_empty() {
            errors.push(format!("license material {} covers no component", material.path));
        }
        let mut material_components = BTreeSet::new();
        for id in &material.component_ids {
            if !material_components.insert(id.as_str()) {
                errors
                    .push(format!("license material {} duplicates component {id}", material.path));
            }
            if !selected.contains(id.as_str()) {
                errors.push(format!(
                    "license material {} covers unknown component {id}",
                    material.path
                ));
                continue;
            }
            if material.kind == LicenseMaterialKind::LicenseText {
                covered.insert(id.as_str());
                let expression = licenses.get(id.as_str()).copied().unwrap_or_default();
                for term in expression.split(" AND ") {
                    if !material.spdx_expressions.iter().any(|item| item == term) {
                        errors.push(format!(
                            "license material {} omits {term} for {id}",
                            material.path
                        ));
                    }
                }
            }
        }
        match material.contents.as_deref() {
            Some(contents) => {
                if material.bytes != contents.len() as u64
                    || material.sha256 != format!("{:x}", Sha256::digest(contents.as_bytes()))
                {
                    errors.push(format!(
                        "license material {} content hash or size differs",
                        material.path
                    ));
                }
                if material.kind == LicenseMaterialKind::LicenseText
                    && !complete_license_text(contents, &material.spdx_expressions)
                {
                    errors.push(format!(
                        "license material {} is not complete license text",
                        material.path
                    ));
                }
            }
            None if material.kind == LicenseMaterialKind::LicenseText => {
                errors
                    .push(format!("license material {} lacks auditable full text", material.path));
            }
            None => {}
        }
    }
    for id in selected.difference(&covered) {
        errors.push(format!("projected component {id} lacks complete archived license text"));
    }
    validate_ffmpeg(repository, projection, errors);
    validate_pdfium(repository, projection, errors);
}

fn complete_license_text(contents: &str, expressions: &[String]) -> bool {
    if contents.len() < 500 {
        return false;
    }
    expressions.iter().all(|expression| {
        let marker = match expression.as_str() {
            "Apache-2.0" => "Apache License",
            "MIT" => "Permission is hereby granted",
            "BSD-3-Clause" => "Redistribution and use in source and binary forms",
            "BSL-1.0" => "Boost Software License",
            "ISC" => "Permission to use, copy, modify, and/or distribute",
            "LGPL-2.1-or-later" => "GNU LESSER GENERAL PUBLIC LICENSE",
            "MPL-2.0" => "Mozilla Public License",
            "Unicode-3.0" => "UNICODE LICENSE",
            "CDLA-Permissive-2.0" => "Community Data License Agreement",
            "Zlib" => "This software is provided 'as-is'",
            "OFL-1.1" => "SIL OPEN FONT LICENSE",
            _ => return false,
        };
        contents.contains(marker)
    })
}

fn validate_ffmpeg(repository: &Path, projection: &ArchiveProjection, errors: &mut Vec<String>) {
    if !projection.components.iter().any(|id| id == "ffmpeg") {
        return;
    }
    let source: serde_json::Value = serde_json::from_slice(
        &fs::read(repository.join("third_party/ffmpeg/source.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    let source_bytes = source.get("source_bytes").and_then(serde_json::Value::as_u64);
    let source_hash = source.get("source_sha256").and_then(serde_json::Value::as_str);
    let source_ok = projection.license_materials.iter().any(|item| {
        item.kind == LicenseMaterialKind::CorrespondingSource
            && item.component_ids == ["ffmpeg"]
            && Some(item.bytes) == source_bytes
            && Some(item.sha256.as_str()) == source_hash
    });
    let relink_ok = projection.license_materials.iter().any(|item| {
        item.kind == LicenseMaterialKind::RelinkMaterial
            && item.component_ids == ["ffmpeg"]
            && item.bytes > 0
            && item.sha256.len() == 64
    });
    if !source_ok || !relink_ok {
        errors
            .push("FFmpeg archive lacks exact corresponding source or relink material".to_owned());
    }
}

fn validate_pdfium(repository: &Path, projection: &ArchiveProjection, errors: &mut Vec<String>) {
    if !projection.components.iter().any(|id| id == "pdfium") {
        return;
    }
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(repository.join("third_party/pdfium/manifest.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    let target = manifest.pointer(&format!("/targets/{}", projection.target));
    let complete = projection.license_materials.iter().any(|item| {
        item.kind == LicenseMaterialKind::NoticeBundle
            && item.component_ids == ["pdfium"]
            && target.is_some_and(|target| {
                target.get("archive_size").and_then(serde_json::Value::as_u64) == Some(item.bytes)
                    && target.get("archive_sha256").and_then(serde_json::Value::as_str)
                        == Some(item.sha256.as_str())
            })
    });
    if !complete {
        errors
            .push("PDFium archive lacks its exact upstream full license/notice bundle".to_owned());
    }
}

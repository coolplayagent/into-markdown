use super::*;
use crate::ArchiveMember;
use into_markdown_core::{ExecutionOptions, ResourceLimits};
use sha2::{Digest, Sha256};
use std::io::Cursor;

const TAR_BLOCK: usize = 512;

fn context() -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn tar(entries: &[(&str, u8, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    for &(path, kind, bytes) in entries {
        let mut header = [0_u8; TAR_BLOCK];
        header[..path.len()].copy_from_slice(path.as_bytes());
        header[100..108].copy_from_slice(b"0000755\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", bytes.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].fill(b' ');
        header[156] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        let checksum = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(bytes);
        archive.resize(archive.len().div_ceil(TAR_BLOCK) * TAR_BLOCK, 0);
    }
    archive.resize(archive.len() + TAR_BLOCK * 2, 0);
    archive
}

fn artifact(archive: &[u8], entries: &[(&str, u8, &[u8])]) -> RuntimeArtifact {
    let target = entries[1];
    RuntimeArtifact {
        id: "archive-test".into(),
        role: "recognizer".into(),
        file_name: "model.onnx".into(),
        url: "https://example.invalid/model.tar".into(),
        archive_sha256: Some(sha(archive)),
        archive_size: Some(archive.len() as u64),
        archive_member: Some(target.0.into()),
        archive_members: Some(
            entries
                .iter()
                .map(|(path, kind, bytes)| ArchiveMember {
                    path: (*path).into(),
                    kind: if *kind == b'5' { "directory" } else { "file" }.into(),
                    size: bytes.len() as u64,
                    sha256: (*kind != b'5').then(|| sha(bytes)),
                })
                .collect(),
        ),
        sha256: sha(target.2),
        size: target.2.len() as u64,
        platforms: vec!["aarch64-apple-darwin".into()],
        license: "Apache-2.0".into(),
    }
}

fn acquired(artifact: &RuntimeArtifact, bytes: Vec<u8>) -> AcquiredModelArtifact {
    AcquiredModelArtifact {
        acquisition: ModelAcquisition::ArchiveMember {
            archive_sha256: artifact.archive_sha256.clone().unwrap(),
            archive_size: artifact.archive_size.unwrap(),
            member: artifact.archive_member.clone().unwrap(),
        },
        bytes: Box::new(Cursor::new(bytes)),
    }
}

#[test]
fn exact_tar_archive_and_every_member_hash_are_verified() {
    let entries = [
        ("bundle/", b'5', &b""[..]),
        ("bundle/model.onnx", b'0', &b"model"[..]),
        ("bundle/config.yml", b'0', &b"config"[..]),
    ];
    let archive = tar(&entries);
    let artifact = artifact(&archive, &entries);
    let mut output = Vec::new();
    write_verified_artifact(&artifact, acquired(&artifact, archive), &mut output, &context())
        .unwrap();
    assert_eq!(output, b"model");
}

#[test]
fn tar_traversal_links_unknown_duplicates_and_truncation_fail_closed() {
    for entries in [
        vec![
            ("bundle/", b'5', &b""[..]),
            ("../model.onnx", b'0', &b"model"[..]),
            ("bundle/config.yml", b'0', &b"config"[..]),
        ],
        vec![
            ("bundle/", b'5', &b""[..]),
            ("bundle/model.onnx", b'2', &b"model"[..]),
            ("bundle/config.yml", b'0', &b"config"[..]),
        ],
        vec![
            ("bundle/", b'5', &b""[..]),
            ("bundle/model.onnx", b'1', &b"model"[..]),
            ("bundle/config.yml", b'0', &b"config"[..]),
        ],
        vec![
            ("bundle/", b'5', &b""[..]),
            ("bundle/model.onnx", b'0', &b"model"[..]),
            ("bundle/model.onnx", b'0', &b"config"[..]),
        ],
    ] {
        let archive = tar(&entries);
        let artifact = artifact(&archive, &entries);
        let mut output = Vec::new();
        assert!(
            write_verified_artifact(
                &artifact,
                acquired(&artifact, archive),
                &mut output,
                &context(),
            )
            .is_err()
        );
    }

    let entries = [
        ("bundle/", b'5', &b""[..]),
        ("bundle/model.onnx", b'0', &b"model"[..]),
        ("bundle/config.yml", b'0', &b"config"[..]),
    ];
    let unknown_entries = [
        ("bundle/", b'5', &b""[..]),
        ("bundle/model.onnx", b'0', &b"model"[..]),
        ("bundle/unknown", b'0', &b"config"[..]),
    ];
    let unknown_archive = tar(&unknown_entries);
    let mut unknown_artifact = artifact(&unknown_archive, &entries);
    unknown_artifact.archive_sha256 = Some(sha(&unknown_archive));
    unknown_artifact.archive_size = Some(unknown_archive.len() as u64);
    let mut output = Vec::new();
    assert!(
        write_verified_artifact(
            &unknown_artifact,
            acquired(&unknown_artifact, unknown_archive),
            &mut output,
            &context(),
        )
        .is_err()
    );

    let mut archive = tar(&entries);
    let artifact = artifact(&archive, &entries);
    archive.truncate(archive.len() - 1);
    let mut output = Vec::new();
    assert!(
        write_verified_artifact(&artifact, acquired(&artifact, archive), &mut output, &context(),)
            .is_err()
    );
}

#[test]
fn tar_checksum_size_padding_and_trailer_corruption_fail_closed() {
    let entries = [
        ("bundle/", b'5', &b""[..]),
        ("bundle/model.onnx", b'0', &b"model"[..]),
        ("bundle/config.yml", b'0', &b"config"[..]),
    ];
    let original = tar(&entries);
    let artifact = artifact(&original, &entries);
    let corruptions = [
        {
            let mut archive = original.clone();
            archive[0] ^= 1;
            archive
        },
        {
            let mut archive = original.clone();
            archive[512 + 124..512 + 136].copy_from_slice(b"77777777777\0");
            rewrite_header_checksum(&mut archive[512..1024]);
            archive
        },
        {
            let mut archive = original.clone();
            archive[1024 + b"model".len()] = 1;
            archive
        },
        {
            let mut archive = original.clone();
            *archive.last_mut().unwrap() = 1;
            archive
        },
    ];
    for archive in corruptions {
        let mut output = Vec::new();
        assert!(
            write_verified_artifact(
                &artifact,
                acquired(&artifact, archive),
                &mut output,
                &context(),
            )
            .is_err()
        );
    }
}

fn rewrite_header_checksum(header: &mut [u8]) {
    header[148..156].fill(b' ');
    let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
}

#[test]
#[ignore = "explicit official archive acquisition audit"]
fn official_ppocrv6_tar_matches_the_embedded_structure_authority() {
    let archive =
        std::fs::read(std::env::var_os("OCR_OFFICIAL_TAR").expect("OCR_OFFICIAL_TAR")).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../../models/manifest.json")).unwrap();
    let artifact: RuntimeArtifact =
        serde_json::from_value(manifest["bundles"][1]["runtime_artifacts"][0].clone()).unwrap();
    let mut output = Vec::new();
    write_verified_artifact(&artifact, acquired(&artifact, archive), &mut output, &context())
        .unwrap();
    assert_eq!(output.len(), 4_462_639);
    assert_eq!(sha(&output), artifact.sha256);
}

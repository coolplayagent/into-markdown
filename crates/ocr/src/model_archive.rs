//! Exact, bounded TAR structure and member verification.

use crate::{ModelManagerError, RuntimeArtifact};
use into_markdown_core::ExecutionContext;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{Read, Write};

const TAR_BLOCK: usize = 512;
const COPY_BUFFER: usize = 64 * 1024;

pub(super) fn extract_tar(
    mut source: Box<dyn Read>,
    destination: &mut dyn Write,
    artifact: &RuntimeArtifact,
    expected_archive_sha256: &str,
    expected_archive_size: u64,
    target: &str,
    context: &ExecutionContext,
) -> Result<(), ModelManagerError> {
    let authority = artifact
        .archive_members
        .as_ref()
        .ok_or_else(|| corrupt(artifact, "missing archive members"))?;
    let mut archive_digest = Sha256::new();
    let mut archive_bytes = 0_u64;
    let mut seen = BTreeSet::new();
    let mut target_seen = false;
    let mut zero_blocks = 0_u8;
    let mut header = [0_u8; TAR_BLOCK];

    while read_archive_block(
        &mut *source,
        &mut header,
        &mut archive_digest,
        &mut archive_bytes,
        expected_archive_size,
        context,
    )? {
        if header.iter().all(|byte| *byte == 0) {
            zero_blocks = zero_blocks.saturating_add(1);
            if zero_blocks >= 2 {
                break;
            }
            continue;
        }
        if zero_blocks != 0 {
            return Err(corrupt(artifact, "nonzero TAR entry after end marker"));
        }
        validate_tar_checksum(&header).map_err(|detail| corrupt(artifact, detail))?;
        let path = tar_path(&header).map_err(|detail| corrupt(artifact, detail))?;
        if !seen.insert(path.clone()) {
            return Err(corrupt(artifact, "duplicate TAR member"));
        }
        let index = seen.len() - 1;
        let expected = authority
            .get(index)
            .filter(|entry| entry.path == path)
            .ok_or_else(|| corrupt(artifact, "unknown or reordered TAR member"))?;
        let size = parse_octal(&header[124..136]).map_err(|detail| corrupt(artifact, detail))?;
        if size != expected.size {
            return Err(corrupt(artifact, "TAR member size mismatch"));
        }
        let type_flag = header[156];
        let is_file = matches!(type_flag, 0 | b'0');
        let is_directory = type_flag == b'5';
        if (expected.kind == "file" && !is_file)
            || (expected.kind == "directory" && !is_directory)
            || !header[157..257].iter().all(|byte| *byte == 0)
        {
            return Err(corrupt(artifact, "unsafe TAR member type or link"));
        }
        if is_directory && size != 0 {
            return Err(corrupt(artifact, "nonempty TAR directory"));
        }
        let mut member_digest = Sha256::new();
        let write_target = path == target;
        if write_target {
            target_seen = true;
            if expected.sha256.as_deref() != Some(artifact.sha256.as_str())
                || expected.size != artifact.size
            {
                return Err(corrupt(artifact, "target member authority mismatch"));
            }
        }
        copy_tar_member(
            &mut *source,
            if write_target { Some(&mut *destination) } else { None },
            size,
            &mut archive_digest,
            &mut member_digest,
            &mut archive_bytes,
            expected_archive_size,
            context,
        )?;
        let member_sha = format!("{:x}", member_digest.finalize());
        if expected.sha256.as_deref().is_some_and(|sha| sha != member_sha) {
            return Err(corrupt(artifact, "TAR member SHA-256 mismatch"));
        }
    }
    if zero_blocks != 2 || seen.len() != authority.len() || !target_seen {
        return Err(corrupt(artifact, "incomplete TAR structure"));
    }
    let mut tail = [0_u8; COPY_BUFFER];
    loop {
        context.checkpoint()?;
        let count = source.read(&mut tail)?;
        if count == 0 {
            break;
        }
        if tail[..count].iter().any(|byte| *byte != 0) {
            return Err(corrupt(artifact, "nonzero TAR trailer"));
        }
        account_archive(
            &tail[..count],
            &mut archive_digest,
            &mut archive_bytes,
            expected_archive_size,
        )?;
    }
    if archive_bytes != expected_archive_size
        || format!("{:x}", archive_digest.finalize()) != expected_archive_sha256
    {
        return Err(corrupt(artifact, "archive size or SHA-256 mismatch"));
    }
    Ok(())
}

fn read_archive_block(
    source: &mut dyn Read,
    block: &mut [u8; TAR_BLOCK],
    digest: &mut Sha256,
    received: &mut u64,
    maximum: u64,
    context: &ExecutionContext,
) -> Result<bool, ModelManagerError> {
    let mut filled = 0;
    while filled < TAR_BLOCK {
        context.checkpoint()?;
        let count = source.read(&mut block[filled..])?;
        if count == 0 {
            if filled == 0 {
                return Ok(false);
            }
            return Err(ModelManagerError::Corrupt("truncated TAR block".into()));
        }
        filled += count;
    }
    account_archive(block, digest, received, maximum)?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn copy_tar_member(
    source: &mut dyn Read,
    mut destination: Option<&mut dyn Write>,
    size: u64,
    archive_digest: &mut Sha256,
    member_digest: &mut Sha256,
    archive_bytes: &mut u64,
    maximum: u64,
    context: &ExecutionContext,
) -> Result<(), ModelManagerError> {
    let padded = size
        .checked_add((TAR_BLOCK as u64 - size % TAR_BLOCK as u64) % TAR_BLOCK as u64)
        .ok_or_else(|| ModelManagerError::Corrupt("TAR member size overflow".into()))?;
    let mut remaining = padded;
    let mut content_remaining = size;
    let mut buffer = [0_u8; COPY_BUFFER];
    while remaining != 0 {
        context.checkpoint()?;
        let count = usize::try_from(remaining.min(COPY_BUFFER as u64))
            .map_err(|_| ModelManagerError::Corrupt("TAR member size overflow".into()))?;
        source.read_exact(&mut buffer[..count])?;
        account_archive(&buffer[..count], archive_digest, archive_bytes, maximum)?;
        let member_content = usize::try_from(content_remaining.min(count as u64)).unwrap_or(count);
        member_digest.update(&buffer[..member_content]);
        if let Some(writer) = destination.as_deref_mut() {
            writer.write_all(&buffer[..member_content])?;
        }
        if buffer[member_content..count].iter().any(|byte| *byte != 0) {
            return Err(ModelManagerError::Corrupt("nonzero TAR padding".into()));
        }
        content_remaining -= member_content as u64;
        remaining -= count as u64;
    }
    Ok(())
}

fn account_archive(
    bytes: &[u8],
    digest: &mut Sha256,
    received: &mut u64,
    maximum: u64,
) -> Result<(), ModelManagerError> {
    *received = received
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| ModelManagerError::Corrupt("archive size overflow".into()))?;
    if *received > maximum {
        return Err(ModelManagerError::Corrupt("archive exceeds declared size".into()));
    }
    digest.update(bytes);
    Ok(())
}

fn tar_path(header: &[u8; TAR_BLOCK]) -> Result<String, &'static str> {
    let name = nul_terminated(&header[..100])?;
    let prefix = nul_terminated(&header[345..500])?;
    let path = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.split('/').any(|part| part == "." || part == "..")
        || path.trim_end_matches('/').split('/').any(str::is_empty)
    {
        return Err("unsafe TAR member path");
    }
    Ok(path)
}

fn nul_terminated(field: &[u8]) -> Result<&str, &'static str> {
    let end = field.iter().position(|byte| *byte == 0).unwrap_or(field.len());
    if field[end..].iter().any(|byte| *byte != 0) {
        return Err("noncanonical TAR string field");
    }
    std::str::from_utf8(&field[..end]).map_err(|_| "non-UTF-8 TAR member")
}

fn parse_octal(field: &[u8]) -> Result<u64, &'static str> {
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        return Err("base-256 TAR numbers are unsupported");
    }
    let value = field
        .iter()
        .copied()
        .skip_while(|byte| matches!(byte, b' ' | 0))
        .take_while(|byte| *byte != 0 && *byte != b' ')
        .try_fold(0_u64, |value, byte| {
            if !(b'0'..=b'7').contains(&byte) {
                return Err("invalid TAR octal field");
            }
            value
                .checked_mul(8)
                .and_then(|value| value.checked_add(u64::from(byte - b'0')))
                .ok_or("TAR octal overflow")
        })?;
    Ok(value)
}

fn validate_tar_checksum(header: &[u8; TAR_BLOCK]) -> Result<(), &'static str> {
    let expected = parse_octal(&header[148..156])?;
    let actual = header.iter().enumerate().try_fold(0_u64, |sum, (index, byte)| {
        sum.checked_add(if (148..156).contains(&index) { 32 } else { u64::from(*byte) })
            .ok_or("TAR checksum overflow")
    })?;
    if actual != expected {
        return Err("TAR checksum mismatch");
    }
    Ok(())
}

fn corrupt(artifact: &RuntimeArtifact, detail: &str) -> ModelManagerError {
    ModelManagerError::Corrupt(format!("{} {detail}", artifact.id))
}

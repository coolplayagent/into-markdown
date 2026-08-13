//! Typed, bounded acquisition and exact TAR extraction for runtime artifacts.

use crate::{ModelManagerError, RuntimeArtifact};
use into_markdown_core::ExecutionContext;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const COPY_BUFFER: usize = 64 * 1024;

/// Whether a fetch stream contains the final file or its authoritative archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAcquisition {
    Direct,
    ArchiveMember { archive_sha256: String, archive_size: u64, member: String },
}

/// A transport-owned stream plus its explicit acquisition identity.
pub struct AcquiredModelArtifact {
    pub acquisition: ModelAcquisition,
    pub bytes: Box<dyn Read>,
}

pub(crate) fn write_verified_artifact(
    artifact: &RuntimeArtifact,
    acquired: AcquiredModelArtifact,
    destination: &mut dyn Write,
    context: &ExecutionContext,
) -> Result<(), ModelManagerError> {
    let expected = expected_acquisition(artifact)?;
    if acquired.acquisition != expected {
        return Err(corrupt(artifact, "acquisition authority mismatch"));
    }
    match expected {
        ModelAcquisition::Direct => copy_final(
            acquired.bytes,
            destination,
            artifact.size,
            &artifact.sha256,
            &artifact.file_name,
            context,
        ),
        ModelAcquisition::ArchiveMember { archive_sha256, archive_size, member } => {
            crate::model_archive::extract_tar(
                acquired.bytes,
                destination,
                artifact,
                &archive_sha256,
                archive_size,
                &member,
                context,
            )
        }
    }
}

fn expected_acquisition(artifact: &RuntimeArtifact) -> Result<ModelAcquisition, ModelManagerError> {
    match (&artifact.archive_sha256, artifact.archive_size, &artifact.archive_member) {
        (None, None, None) if artifact.archive_members.is_none() => Ok(ModelAcquisition::Direct),
        (Some(hash), Some(size), Some(member)) if artifact.archive_members.is_some() => {
            Ok(ModelAcquisition::ArchiveMember {
                archive_sha256: hash.clone(),
                archive_size: size,
                member: member.clone(),
            })
        }
        _ => Err(corrupt(artifact, "incomplete archive authority")),
    }
}

fn copy_final(
    mut source: Box<dyn Read>,
    destination: &mut dyn Write,
    expected_size: u64,
    expected_sha256: &str,
    label: &str,
    context: &ExecutionContext,
) -> Result<(), ModelManagerError> {
    let mut digest = Sha256::new();
    let mut received = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER];
    loop {
        context.checkpoint()?;
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        received = received
            .checked_add(count as u64)
            .ok_or_else(|| ModelManagerError::Corrupt(label.into()))?;
        if received > expected_size {
            return Err(ModelManagerError::Corrupt(format!("{label} exceeds declared size")));
        }
        destination.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);
    }
    if received != expected_size || format!("{:x}", digest.finalize()) != expected_sha256 {
        return Err(ModelManagerError::Corrupt(format!("{label} has a size or SHA-256 mismatch")));
    }
    Ok(())
}

fn corrupt(artifact: &RuntimeArtifact, detail: &str) -> ModelManagerError {
    ModelManagerError::Corrupt(format!("{} {detail}", artifact.id))
}

#[cfg(test)]
#[path = "model_acquisition_tests.rs"]
mod tests;

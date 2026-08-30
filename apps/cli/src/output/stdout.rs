//! Accounted stdout publication and companion-asset finalization.

use super::assets::StagedAssets;
use crate::error::CliError;
use into_markdown::{ExecutionContext, TemporaryFile};
use std::io::{Read, Seek, Write};

const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// Copy one fully serialized primary artifact to stdout.
///
/// A closed pipe preserves the existing CLI contract: conversion and
/// serialization already succeeded, so staged companion assets are committed.
/// Cancellation and every other I/O failure abort the companions.
pub(crate) fn publish(
    primary: &TemporaryFile,
    stdout: &mut dyn Write,
    staged_assets: Option<StagedAssets>,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    let result = copy_primary(primary, stdout, context);
    match result {
        Ok(()) => commit_assets(staged_assets),
        Err(error) if error.is_broken_pipe() => {
            commit_assets(staged_assets)?;
            Err(error)
        }
        Err(error) => {
            abort_assets(staged_assets, &error)?;
            Err(error)
        }
    }
}

fn copy_primary(
    primary: &TemporaryFile,
    stdout: &mut dyn Write,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    let _memory = context
        .reserve_memory(u64::try_from(COPY_BUFFER_BYTES).unwrap_or(u64::MAX))
        .map_err(CliError::from)?;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES].into_boxed_slice();
    let mut reader = primary.as_file().map_err(CliError::from)?.try_clone()?;
    reader.rewind()?;
    loop {
        context.checkpoint().map_err(CliError::from)?;
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        stdout.write_all(&buffer[..read])?;
    }
}

fn commit_assets(staged_assets: Option<StagedAssets>) -> Result<(), CliError> {
    if let Some(staged) = staged_assets {
        staged.commit()?;
    }
    Ok(())
}

fn abort_assets(
    staged_assets: Option<StagedAssets>,
    primary_error: &CliError,
) -> Result<(), CliError> {
    let Some(staged) = staged_assets else {
        return Ok(());
    };
    if let Err(recovery) = staged.abort() {
        return Err(CliError::new(
            crate::error::ExitClass::Io,
            "rollbackFailed",
            format!(
                "stdout failed ({}: {}); staged asset rollback failed ({}: {})",
                primary_error.code(),
                primary_error.message(),
                recovery.code(),
                recovery.message()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "stdout/tests.rs"]
mod tests;

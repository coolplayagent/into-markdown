//! Accounted stdout publication and companion-asset finalization.

use super::assets::StagedAssets;
use super::stream::ReplaySpool;
use crate::error::CliError;
use into_markdown::ExecutionContext;
use std::io::Write;

/// Copy one fully serialized primary artifact to stdout.
///
/// A closed pipe preserves the existing CLI contract: conversion and
/// serialization already succeeded, so staged companion assets are committed.
/// Cancellation and every other I/O failure abort the companions.
pub(crate) fn publish(
    primary: &ReplaySpool,
    stdout: &mut dyn Write,
    staged_assets: Option<StagedAssets>,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    let result = primary.copy_to(context, stdout);
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

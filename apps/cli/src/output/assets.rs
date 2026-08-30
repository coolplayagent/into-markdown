//! External asset planning and transactional staging.

use crate::args::{AssetModeArg, ConflictPolicy};
use crate::error::{CliError, ExitClass};
#[cfg(test)]
use crate::transaction::Target;
use crate::transaction::{self, FileTarget, PreparedTransaction};
use into_markdown::ExecutionContext;
#[cfg(test)]
use into_markdown::{ConversionOptions, ConversionResult, plan_assets};
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};

use super::commit::WriteOutcome;
#[cfg(test)]
use super::commit::write_exact_file;
use super::stream::StructuredSpool;

/// Write extracted assets using safe, deterministic filenames.
#[cfg(test)]
pub(crate) fn write_assets(
    result: &ConversionResult,
    directory: &Path,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
) -> Result<Vec<WriteOutcome>, CliError> {
    write_assets_with_hook(result, directory, mode, conflict, || Ok(()))
}

/// Fully staged external assets whose targets have not been mutated yet.
pub(crate) struct StagedAssets {
    transaction: Option<PreparedTransaction>,
    targets: Vec<PathBuf>,
}

/// Preflight, write, and fsync every external asset without changing targets.
#[cfg(test)]
pub(crate) fn stage_assets(
    result: &ConversionResult,
    directory: &Path,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
    context: &ExecutionContext,
) -> Result<StagedAssets, CliError> {
    let planned = plan_asset_writes(result, directory, mode, conflict, Some(context))?;
    if planned.is_empty() {
        return Ok(StagedAssets { transaction: None, targets: vec![] });
    }
    let targets_with_bytes = planned
        .iter()
        .map(|(source_index, path)| Target {
            path: path.clone(),
            bytes: result.assets[*source_index].bytes.as_slice(),
        })
        .collect::<Vec<_>>();
    let transaction =
        transaction::prepare(&targets_with_bytes, conflict == ConflictPolicy::Overwrite, context)?;
    Ok(StagedAssets {
        transaction: Some(transaction),
        targets: planned.into_iter().map(|(_, path)| path).collect(),
    })
}

/// Preflight and stage file-backed asset payloads without materializing them in memory.
pub(crate) fn stage_spooled_assets(
    spool: &StructuredSpool,
    directory: &Path,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
    context: &ExecutionContext,
) -> Result<StagedAssets, CliError> {
    let planned = plan_spooled_asset_writes(spool, directory, mode, conflict, context)?;
    if planned.is_empty() {
        return Ok(StagedAssets { transaction: None, targets: vec![] });
    }
    let files = planned
        .iter()
        .map(|(path, file)| FileTarget { path: path.clone(), file })
        .collect::<Vec<_>>();
    let transaction =
        transaction::prepare_files(&files, conflict == ConflictPolicy::Overwrite, context)?;
    Ok(StagedAssets {
        transaction: Some(transaction),
        targets: planned.into_iter().map(|(path, _)| path).collect(),
    })
}

pub(super) fn plan_spooled_asset_writes<'a>(
    spool: &'a StructuredSpool,
    directory: &Path,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
    context: &ExecutionContext,
) -> Result<Vec<(PathBuf, &'a std::fs::File)>, CliError> {
    if mode != AssetModeArg::Extract {
        return Ok(Vec::new());
    }
    let planned = spool
        .external_payloads()?
        .into_iter()
        .map(|(filename, file)| (directory.join(filename), file))
        .collect::<Vec<_>>();
    let paths = planned.iter().map(|(path, _)| path.clone()).collect::<Vec<_>>();
    transaction::recover_for_paths(&paths, context)?;
    if let Some((path, _)) =
        planned.iter().find(|(path, _)| path.exists() && conflict != ConflictPolicy::Overwrite)
    {
        return Err(CliError::new(
            ExitClass::Io,
            "assetConflict",
            format!(
                "stable asset output already exists and cannot be renamed safely: {}",
                path.display()
            ),
        ));
    }
    Ok(planned)
}

impl StagedAssets {
    /// Commit all staged assets after the stdout stream succeeds.
    pub(crate) fn commit(mut self) -> Result<Vec<WriteOutcome>, CliError> {
        if let Some(transaction) = self.transaction.take() {
            transaction.commit()?;
        }
        Ok(self.targets.into_iter().map(|path| WriteOutcome { path, renamed: false }).collect())
    }

    /// Discard staged resources without modifying external targets.
    pub(crate) fn abort(mut self) -> Result<(), CliError> {
        self.transaction.take().map_or(Ok(()), PreparedTransaction::abort)
    }
}

#[cfg(test)]
pub(super) fn write_assets_with_hook(
    result: &ConversionResult,
    directory: &Path,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
    after_preflight: impl FnOnce() -> Result<(), CliError>,
) -> Result<Vec<WriteOutcome>, CliError> {
    let planned = plan_asset_writes(result, directory, mode, conflict, None)?;
    if planned.is_empty() {
        return Ok(Vec::new());
    }
    fs::create_dir_all(directory)?;
    after_preflight()?;
    let mut outcomes = Vec::with_capacity(planned.len());
    for (source_index, path) in planned {
        write_exact_file(
            &path,
            &result.assets[source_index].bytes,
            conflict == ConflictPolicy::Overwrite,
        )?;
        outcomes.push(WriteOutcome { path, renamed: false });
    }
    Ok(outcomes)
}

#[cfg(test)]
pub(super) fn plan_asset_writes(
    result: &ConversionResult,
    directory: &Path,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
    context: Option<&ExecutionContext>,
) -> Result<Vec<(usize, PathBuf)>, CliError> {
    if mode != AssetModeArg::Extract || result.assets.is_empty() {
        return Ok(Vec::new());
    }
    let plan = plan_assets(&result.document, &result.assets, &ConversionOptions::default())
        .map_err(CliError::from)?;
    let mut planned = Vec::with_capacity(plan.entries().len());
    let mut targets = std::collections::BTreeSet::new();
    for asset in plan.entries() {
        let path = directory.join(&asset.filename);
        if !targets.insert(path.clone()) {
            return Err(CliError::internal(format!(
                "multiple assets resolve to {}",
                path.display()
            )));
        }
        planned.push((asset.source_index, path));
    }
    if let Some(context) = context {
        let paths = planned.iter().map(|(_, path)| path.clone()).collect::<Vec<_>>();
        transaction::recover_for_paths(&paths, context)?;
    }
    if let Some((_, path)) =
        planned.iter().find(|(_, path)| path.exists() && conflict != ConflictPolicy::Overwrite)
    {
        return Err(CliError::new(
            ExitClass::Io,
            "assetConflict",
            format!(
                "stable asset output already exists and cannot be renamed safely: {}",
                path.display()
            ),
        ));
    }
    Ok(planned)
}

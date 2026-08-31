//! Atomic primary-artifact publication and conflict handling.

use super::assets::plan_spooled_asset_writes;
use super::stream::StructuredSpool;
use crate::args::{AssetModeArg, ConflictPolicy};
use crate::error::{CliError, ExitClass};
use crate::transaction::{self, FileTarget, Target};
use into_markdown::ExecutionContext;
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::io::Write;
use std::path::{Path, PathBuf};

/// Outcome of one atomic file write.
#[derive(Debug, Clone)]
pub(crate) struct WriteOutcome {
    pub(crate) path: PathBuf,
    pub(crate) renamed: bool,
}

/// Atomically commit a file-backed primary artifact and file-backed companion assets.
pub(crate) fn write_spooled_output_set_file(
    primary: &Path,
    primary_file: &std::fs::File,
    spool: &StructuredSpool,
    asset_directory: Option<&Path>,
    mode: AssetModeArg,
    conflict: ConflictPolicy,
    context: &ExecutionContext,
) -> Result<WriteOutcome, CliError> {
    let primary = primary.to_path_buf();
    let planned_assets = asset_directory
        .map(|directory| plan_spooled_asset_writes(spool, directory, mode, conflict, context))
        .transpose()?
        .unwrap_or_default();
    let mut files = Vec::with_capacity(planned_assets.len() + 1);
    files.push(FileTarget { path: primary.clone(), file: primary_file });
    files.extend(planned_assets.into_iter().map(|(path, file)| FileTarget { path, file }));
    transaction::prepare_files(&files, conflict == ConflictPolicy::Overwrite, context)?.commit()?;
    Ok(WriteOutcome { path: primary, renamed: false })
}

/// Write a primary artifact using the requested conflict policy.
pub(crate) fn write_file(
    requested: &Path,
    bytes: &[u8],
    conflict: ConflictPolicy,
    context: &ExecutionContext,
) -> Result<WriteOutcome, CliError> {
    transaction::recover_for_paths(&[requested.to_path_buf()], context)?;
    let (path, renamed) = resolve_conflict(requested, conflict)?;
    transaction::prepare(
        &[Target { path: path.clone(), bytes }],
        conflict == ConflictPolicy::Overwrite,
        context,
    )?
    .commit()?;
    Ok(WriteOutcome { path, renamed })
}

/// Resolve an output conflict without writing the file.
pub(crate) fn preflight_file(
    path: &Path,
    conflict: ConflictPolicy,
    context: &ExecutionContext,
) -> Result<PathBuf, CliError> {
    transaction::recover_for_paths(&[path.to_path_buf()], context)?;
    resolve_conflict(path, conflict).map(|(resolved, _)| resolved)
}

/// Atomically write a previously resolved path without recalculating its name.
#[cfg(test)]
pub(super) fn write_preflighted_file(
    path: &Path,
    bytes: &[u8],
    conflict: ConflictPolicy,
) -> Result<WriteOutcome, CliError> {
    write_exact_file(path, bytes, conflict == ConflictPolicy::Overwrite)?;
    Ok(WriteOutcome { path: path.to_path_buf(), renamed: false })
}

fn resolve_conflict(
    requested: &Path,
    conflict: ConflictPolicy,
) -> Result<(PathBuf, bool), CliError> {
    if !requested.exists() || conflict == ConflictPolicy::Overwrite {
        return Ok((requested.to_path_buf(), false));
    }
    if conflict == ConflictPolicy::Error {
        return Err(CliError::new(
            ExitClass::Io,
            "outputConflict",
            format!("output already exists: {}", requested.display()),
        ));
    }
    let parent = requested.parent().unwrap_or_else(|| Path::new("."));
    let filename = requested.file_name().and_then(|value| value.to_str()).unwrap_or("output");
    let (stem, extension) = if let Some(stem) = filename.strip_suffix(".mdpkg.zip") {
        (stem.to_owned(), Some("mdpkg.zip".to_owned()))
    } else {
        (
            requested.file_stem().and_then(|value| value.to_str()).unwrap_or("output").to_owned(),
            requested.extension().and_then(|value| value.to_str()).map(ToOwned::to_owned),
        )
    };
    for number in 1_u64..=u64::MAX {
        let name = extension.as_ref().map_or_else(
            || format!("{stem}-{number}"),
            |extension| format!("{stem}-{number}.{extension}"),
        );
        let candidate = parent.join(name);
        if !candidate.exists() {
            return Ok((candidate, true));
        }
    }
    Err(CliError::new(
        ExitClass::Io,
        "outputConflict",
        format!("could not allocate a unique output name for {}", requested.display()),
    ))
}

#[cfg(test)]
pub(super) fn write_exact_file(path: &Path, bytes: &[u8], overwrite: bool) -> Result<(), CliError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let filename = path.file_name().and_then(|value| value.to_str()).unwrap_or("output");
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{filename}.into-md-"))
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    let result =
        if overwrite { temporary.persist(path) } else { temporary.persist_noclobber(path) };
    result.map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            CliError::new(
                ExitClass::Io,
                "outputConflict",
                format!("output appeared after preflight: {}", path.display()),
            )
        } else {
            CliError::from(error.error)
        }
    })?;
    Ok(())
}

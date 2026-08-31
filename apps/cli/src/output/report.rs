//! Bounded typed reports written directly to the existing file-backed transaction.

use super::commit::WriteOutcome;
use crate::error::{CliError, ExitClass};
use crate::transaction::{self, FileTarget, PreparedTransaction};
use into_markdown::{
    BatchReportDto, ConversionError, DtoErrorCode, DtoJsonStyle, ExecutionContext, TemporaryFile,
};
use std::io::{self, BufWriter, Write};
use std::path::Path;

const BUFFER_BYTES: usize = 64 * 1024;

pub(crate) fn write_report(
    path: &Path,
    report: &BatchReportDto,
    context: &ExecutionContext,
) -> Result<WriteOutcome, CliError> {
    prepare_report(path, report, context)?.commit()?;
    Ok(WriteOutcome { path: path.to_path_buf(), renamed: false })
}

fn prepare_report(
    path: &Path,
    report: &BatchReportDto,
    context: &ExecutionContext,
) -> Result<PreparedTransaction, CliError> {
    let _buffer_memory = context.reserve_memory(BUFFER_BYTES as u64).map_err(CliError::from)?;
    let mut stage = ReportFile {
        file: context.temporary_file("into-md-report").map_err(CliError::from)?,
        error: None,
    };
    let result = {
        let mut buffered = BufWriter::with_capacity(BUFFER_BYTES, &mut stage);
        let result = report
            .write_json(DtoJsonStyle::Pretty, &mut buffered)
            .map_err(|error| {
                let (class, code) = if error.code == DtoErrorCode::ResourceLimit {
                    (ExitClass::Policy, "resourceLimit")
                } else {
                    (ExitClass::Internal, "internal")
                };
                CliError::new(class, code, format!("write batch report DTO: {error}"))
            })
            .and_then(|()| buffered.write_all(b"\n").map_err(CliError::from))
            .and_then(|()| buffered.flush().map_err(CliError::from));
        // Do not retry flushing buffered prefixes during unwinding after a failure.
        let _ = buffered.into_parts();
        result
    };
    if let Some(error) = stage.error.take() {
        return Err(error.into());
    }
    result?;
    stage.file.sync_all().map_err(CliError::from)?;
    transaction::recover_for_paths(&[path.to_path_buf()], context)?;
    transaction::prepare_files(
        &[FileTarget {
            path: path.to_path_buf(),
            file: stage.file.as_file().map_err(CliError::from)?,
        }],
        true,
        context,
    )
}

struct ReportFile {
    file: TemporaryFile,
    error: Option<ConversionError>,
}

impl Write for ReportFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Err(error) = self.file.write_all_checked(bytes) {
            self.error = Some(error);
            return Err(io::Error::other("batch report stage write failed"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;

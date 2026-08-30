//! Stable primary-artifact and report serialization.

use super::bundle;
use crate::args::EmitKind;
use crate::error::CliError;
use into_markdown::{BatchReportDto, ConversionResult, DtoJsonStyle, ResultDto};
use std::io::{Cursor, Seek, Write};

/// Serialize a conversion result into the selected primary artifact.
pub(crate) fn encode_result(
    result: &ConversionResult,
    emit: EmitKind,
) -> Result<Vec<u8>, CliError> {
    let mut destination = Cursor::new(Vec::new());
    encode_result_into(result, emit, &mut destination)?;
    Ok(destination.into_inner())
}

/// Stream a conversion result into a seekable destination without allocating
/// another complete primary-artifact buffer.
pub(crate) fn encode_result_into<W: Write + Seek>(
    result: &ConversionResult,
    emit: EmitKind,
    mut destination: W,
) -> Result<(), CliError> {
    match emit {
        EmitKind::Markdown => {
            for chunk in result.markdown.as_bytes().chunks(64 * 1024) {
                destination.write_all(chunk)?;
            }
        }
        EmitKind::IrJson => {
            result
                .document
                .validate()
                .map_err(|error| CliError::internal(format!("validate document IR: {error}")))?;
            serde_json::to_writer_pretty(&mut destination, &result.document)
                .map_err(|error| CliError::internal(format!("serialize document IR: {error}")))?;
            destination.write_all(b"\n")?;
        }
        EmitKind::ResultJson => {
            ResultDto::write_json_from_result(result, DtoJsonStyle::Pretty, &mut destination)
                .map_err(|error| CliError::internal(format!("serialize result DTO: {error}")))?;
            destination.write_all(b"\n")?;
        }
        EmitKind::Bundle => bundle::write_bundle(result, destination)?,
    }
    Ok(())
}

pub(super) fn report_json_with_newline(report: &BatchReportDto) -> Result<Vec<u8>, CliError> {
    let json = report
        .to_pretty_json()
        .map_err(|error| CliError::internal(format!("serialize batch report DTO: {error}")))?;
    let mut bytes = json.into_bytes();
    bytes.push(b'\n');
    Ok(bytes)
}

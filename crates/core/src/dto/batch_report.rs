//! Borrowed output for already typed reports; untrusted JSON decoding stays unchanged.

use super::{
    BatchItemDto, BatchItemOutcome, BatchItemStatus, BatchReportDto, DiagnosticDto,
    DiagnosticSeverityDto, DtoError, DtoErrorCode, DtoJsonStyle, DtoLimits, InternalDiagnosticWire,
    RawBatchItemOutcome, RawBatchItemStatus, RawBatchOcrUsageDto, RawBatchResourceUsageDto,
    RawDiagnosticSeverityDto, WireAccounting, limit,
};
use serde::{Serialize, ser::SerializeSeq};
use std::io::{self, Write};

impl BatchReportDto {
    /// Stream an internally constructed report without copying its items or diagnostics.
    /// Semantic and encoded-byte limits still apply. Parser complexity limits are
    /// reserved for untrusted JSON input, not this fixed, typed wire structure.
    /// The destination may contain a prefix on failure; callers must stage publication.
    ///
    /// # Errors
    ///
    /// Returns an invalid-report, encoded-byte-limit, or destination-write error.
    #[doc(hidden)]
    pub fn write_json<W: Write>(
        &self,
        style: DtoJsonStyle,
        destination: W,
    ) -> Result<(), DtoError> {
        write_report(self, style, destination, &DtoLimits::default())
    }
}

fn write_report<W: Write>(
    report: &BatchReportDto,
    style: DtoJsonStyle,
    destination: W,
    limits: &DtoLimits,
) -> Result<(), DtoError> {
    report.validate(limits)?;
    let wire = ReportWire {
        schema_version: report.schema_version,
        succeeded: report.succeeded,
        failed: report.failed,
        items: Items(&report.items),
        wall_duration_ms: report.wall_duration_ms,
        resource_usage: report.resource_usage.as_ref().map(|usage| RawBatchResourceUsageDto {
            memory: usage.memory.map(Into::into),
            ocr_runtime: usage.ocr_runtime.map(Into::into),
            shared_lease_budget_bytes: usage.shared_lease_budget_bytes,
            shared_lease_peak_bytes: usage.shared_lease_peak_bytes,
            temporary_lease_budget_bytes: usage.temporary_lease_budget_bytes,
            temporary_lease_peak_bytes: usage.temporary_lease_peak_bytes,
            ocr: usage.ocr.map(|ocr| RawBatchOcrUsageDto {
                recognized_regions: ocr.recognized_regions,
                recognized_chars: ocr.recognized_chars,
            }),
        }),
    };
    let mut writer =
        ReportWriter { destination, accounting: WireAccounting::default(), limits, error: None };
    let result = match style {
        DtoJsonStyle::Compact => serde_json::to_writer(&mut writer, &wire),
        DtoJsonStyle::Pretty => serde_json::to_writer_pretty(&mut writer, &wire),
    };
    if let Some(error) = writer.error {
        return Err(error);
    }
    result.map_err(|error| {
        DtoError::new(DtoErrorCode::InvalidJson, "$", format!("write batch report: {error}"))
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportWire<'a> {
    schema_version: u32,
    succeeded: u64,
    failed: u64,
    items: Items<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wall_duration_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_usage: Option<RawBatchResourceUsageDto>,
}

struct Items<'a>(&'a [BatchItemDto]);

impl Serialize for Items<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for item in self.0 {
            sequence.serialize_element(&ItemWire {
                input: &item.input,
                output: item.output.as_deref(),
                format: item.format.as_deref(),
                status: match item.status {
                    BatchItemStatus::Success => RawBatchItemStatus::Success,
                    BatchItemStatus::Failed => RawBatchItemStatus::Failed,
                },
                outcome: match item.outcome {
                    BatchItemOutcome::Complete => RawBatchItemOutcome::Complete,
                    BatchItemOutcome::Degraded => RawBatchItemOutcome::Degraded,
                    BatchItemOutcome::Failed => RawBatchItemOutcome::Failed,
                },
                diagnostics: Diagnostics(&item.diagnostics),
                error_code: item.error_code.as_deref(),
                reason_code: item.reason_code.as_deref(),
                component: item.component.as_deref(),
                part: item.part.as_deref(),
                limit: item
                    .limit
                    .as_ref()
                    .map(|value| LimitWire { name: &value.name, detail: value.detail.as_deref() }),
                message: item.message.as_deref(),
                warnings: &item.warnings,
                duration_ms: item.duration_ms,
                processing_duration_ms: item.processing_duration_ms,
            })?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemWire<'a> {
    input: &'a str,
    output: Option<&'a str>,
    format: Option<&'a str>,
    status: RawBatchItemStatus,
    outcome: RawBatchItemOutcome,
    diagnostics: Diagnostics<'a>,
    error_code: Option<&'a str>,
    reason_code: Option<&'a str>,
    component: Option<&'a str>,
    part: Option<&'a str>,
    limit: Option<LimitWire<'a>>,
    message: Option<&'a str>,
    warnings: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    processing_duration_ms: Option<f64>,
}

#[derive(Serialize)]
struct LimitWire<'a> {
    name: &'a str,
    detail: Option<&'a str>,
}

struct Diagnostics<'a>(&'a [DiagnosticDto]);

impl Serialize for Diagnostics<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for diagnostic in self.0 {
            sequence.serialize_element(&InternalDiagnosticWire {
                code: &diagnostic.code,
                severity: match diagnostic.severity {
                    DiagnosticSeverityDto::Info => RawDiagnosticSeverityDto::Info,
                    DiagnosticSeverityDto::Warning => RawDiagnosticSeverityDto::Warning,
                    DiagnosticSeverityDto::Error => RawDiagnosticSeverityDto::Error,
                },
                message: &diagnostic.message,
                locator: diagnostic.locator.as_ref(),
            })?;
        }
        sequence.end()
    }
}

struct ReportWriter<'a, W> {
    destination: W,
    accounting: WireAccounting,
    limits: &'a DtoLimits,
    error: Option<DtoError>,
}

impl<W: Write> ReportWriter<'_, W> {
    fn check_bytes(&self) -> Result<(), DtoError> {
        if self.accounting.json_bytes > self.limits.max_json_bytes {
            return limit("$", "dtoJsonBytes", self.limits.max_json_bytes);
        }
        if self.accounting.max_string_bytes.max(self.accounting.string_bytes)
            > self.limits.max_string_bytes
        {
            return limit("$", "dtoStringBytes", self.limits.max_string_bytes);
        }
        if self.accounting.total_string_bytes > self.limits.max_total_string_bytes {
            return limit("$", "dtoTotalStringBytes", self.limits.max_total_string_bytes);
        }
        Ok(())
    }
}

impl<W: Write> Write for ReportWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.accounting.write_all(bytes)?;
        if let Err(error) = self.check_bytes() {
            self.error = Some(error);
            return Err(io::Error::other("batch report exceeds encoded-byte budget"));
        }
        self.destination.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.destination.flush()
    }
}

#[cfg(test)]
#[path = "batch_report_tests.rs"]
mod tests;

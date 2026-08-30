//! Monotonic CLI timing and presentation, kept separate from conversion orchestration.

use crate::error::CliError;
use crate::i18n::Catalog;
use into_markdown::BatchItemDto;
use serde::Serialize;
use std::io::Write;
use std::time::Instant;

pub(crate) struct ItemTimer(Instant);

impl ItemTimer {
    pub(crate) fn start() -> Self {
        Self(Instant::now())
    }

    pub(crate) fn elapsed_ms(&self) -> f64 {
        elapsed_ms(self.0)
    }
}

pub(crate) fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

pub(crate) fn write_summary(
    stderr: &mut dyn Write,
    reports: &[BatchItemDto],
    wall_duration_ms: f64,
    catalog: Catalog,
    json_log: bool,
) -> Result<(), CliError> {
    if json_log {
        write_json_summary(stderr, reports, wall_duration_ms)
    } else {
        write_human_summary(stderr, reports, wall_duration_ms, catalog)
    }
}

fn write_human_summary(
    stderr: &mut dyn Write,
    reports: &[BatchItemDto],
    wall_duration_ms: f64,
    catalog: Catalog,
) -> Result<(), CliError> {
    let include_input = reports.len() > 1;
    for report in reports {
        let total = display_duration(report.duration_ms, catalog);
        let processing = display_duration(report.processing_duration_ms, catalog);
        if include_input {
            writeln!(
                stderr,
                "{}: {}: {} {total}, {} {processing}",
                catalog.timing_prefix(),
                safe_input_label(&report.input),
                catalog.total_duration_label(),
                catalog.processing_duration_label(),
            )?;
        } else {
            writeln!(
                stderr,
                "{}: {} {total}, {} {processing}",
                catalog.timing_prefix(),
                catalog.total_duration_label(),
                catalog.processing_duration_label(),
            )?;
        }
    }
    writeln!(
        stderr,
        "{}: {} {wall_duration_ms:.2} ms",
        catalog.timing_prefix(),
        catalog.batch_wall_duration_label(),
    )?;
    Ok(())
}

fn display_duration(value: Option<f64>, catalog: Catalog) -> String {
    value.map_or_else(
        || catalog.unavailable_label().to_owned(),
        |duration| format!("{duration:.2} ms"),
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemTimingEvent<'a> {
    level: &'static str,
    code: &'static str,
    message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    processing_duration_ms: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchTimingEvent {
    level: &'static str,
    code: &'static str,
    message: &'static str,
    wall_duration_ms: f64,
}

fn write_json_summary(
    stderr: &mut dyn Write,
    reports: &[BatchItemDto],
    wall_duration_ms: f64,
) -> Result<(), CliError> {
    let include_input = reports.len() > 1;
    for report in reports {
        serde_json::to_writer(
            &mut *stderr,
            &ItemTimingEvent {
                level: "info",
                code: "itemTiming",
                message: "conversion item timing",
                input: include_input.then(|| safe_input_label(&report.input)),
                duration_ms: report.duration_ms,
                processing_duration_ms: report.processing_duration_ms,
            },
        )
        .map_err(|error| CliError::internal(format!("serialize timing event: {error}")))?;
        writeln!(stderr)?;
    }
    serde_json::to_writer(
        &mut *stderr,
        &BatchTimingEvent {
            level: "info",
            code: "batchTiming",
            message: "conversion batch timing",
            wall_duration_ms,
        },
    )
    .map_err(|error| CliError::internal(format!("serialize timing event: {error}")))?;
    writeln!(stderr)?;
    Ok(())
}

fn safe_input_label(input: &str) -> &str {
    if input.contains("://") { "remote input" } else { input }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown::{BatchItemOutcome, BatchItemStatus};

    fn item() -> BatchItemDto {
        BatchItemDto {
            input: r"C:\work\input.txt".into(),
            output: Some("C:/work/output.md".into()),
            format: Some("text".into()),
            status: BatchItemStatus::Success,
            outcome: BatchItemOutcome::Complete,
            diagnostics: vec![],
            error_code: None,
            reason_code: None,
            component: None,
            part: None,
            limit: None,
            message: None,
            warnings: vec![],
            duration_ms: Some(12.5),
            processing_duration_ms: Some(8.25),
        }
    }

    #[test]
    fn json_summary_is_structured_and_hides_single_input() {
        let mut output = Vec::new();
        write_summary(&mut output, &[item()], 13.0, Catalog::new(crate::args::Language::En), true)
            .unwrap();
        let lines = String::from_utf8(output).unwrap();
        let events = lines
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events[0]["code"], "itemTiming");
        assert!(events[0].get("input").is_none());
        assert_eq!(events[1]["wallDurationMs"], 13.0);
    }

    #[test]
    fn batch_labels_keep_windows_paths_and_hide_remote_queries() {
        assert_eq!(safe_input_label(r"C:\work\input.txt"), r"C:\work\input.txt");
        assert_eq!(safe_input_label("https://example.test/a?secret=x"), "remote input");
    }
}

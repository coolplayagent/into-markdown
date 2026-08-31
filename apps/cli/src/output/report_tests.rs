use super::*;
use into_markdown::{
    BatchItemDto, BatchItemOutcome, BatchItemStatus, Diagnostic, DiagnosticDto,
    DiagnosticSeverityDto, ExecutionOptions, ResourceLimits,
};
use std::fs;

fn report(diagnostics: Vec<DiagnosticDto>) -> BatchReportDto {
    BatchReportDto::try_new_with_wall_duration(
        vec![BatchItemDto {
            input: "diagnostics-replay".into(),
            output: None,
            format: None,
            status: BatchItemStatus::Success,
            outcome: BatchItemOutcome::Degraded,
            diagnostics,
            error_code: None,
            reason_code: None,
            component: None,
            part: None,
            limit: None,
            message: None,
            warnings: vec![],
            duration_ms: Some(12.5),
            processing_duration_ms: Some(8.25),
        }],
        Some(14.0),
    )
    .unwrap()
}

fn context(limits: ResourceLimits) -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), limits)
}

#[test]
fn report_stage_preserves_bytes_and_rolls_back_commit_io_failure() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let path = root.join("report.json");
    fs::write(&path, b"previous").unwrap();
    let report = report(vec![]);
    let execution = context(ResourceLimits::default());
    let error = prepare_report(&path, &report, &execution)
        .unwrap()
        .commit_with_hook(|phase, _| {
            if phase == "targetInstalled" {
                Err(io::Error::from(io::ErrorKind::StorageFull).into())
            } else {
                Ok(crate::transaction::HookDecision::Continue)
            }
        })
        .unwrap_err();
    assert_eq!(error.code(), "io");
    assert_eq!(fs::read(&path).unwrap(), b"previous");
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    assert_eq!(execution.reserved_memory_bytes(), 0);
    assert_eq!(execution.reserved_temporary_bytes(), 0);
    write_report(&path, &report, &execution).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), report.to_pretty_json().unwrap() + "\n");
    assert_eq!(execution.reserved_memory_bytes(), 0);
    assert_eq!(execution.reserved_temporary_bytes(), 0);
}

#[test]
fn report_temporary_budget_failure_preserves_existing_destination() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let path = root.join("report.json");
    fs::write(&path, b"previous").unwrap();
    let diagnostic = DiagnosticDto {
        code: "ocr.lowConfidence".into(),
        severity: DiagnosticSeverityDto::Warning,
        message: "omitted region ".repeat(64),
        locator: None,
    };
    let report = report(vec![diagnostic; 512]);
    let encoded_bytes = u64::try_from(report.to_pretty_json().unwrap().len()).unwrap() + 1;
    for max_temporary_bytes in [128 * 1024, encoded_bytes + 128 * 1024] {
        let execution =
            context(ResourceLimits { max_temporary_bytes, ..ResourceLimits::default() });
        let error = write_report(&path, &report, &execution).unwrap_err();
        assert_eq!(error.code(), "resourceLimit", "{error:?}");
        assert_eq!(fs::read(&path).unwrap(), b"previous");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        assert_eq!(execution.reserved_memory_bytes(), 0);
        assert_eq!(execution.reserved_temporary_bytes(), 0);
    }
}

#[test]
#[ignore = "requires INTO_MD_REPORT_DIAGNOSTICS_FIXTURE and INTO_MD_REPORT_REPLAY_OUTPUT"]
fn real_diagnostics_report_replay_without_reconversion() {
    #[derive(serde::Deserialize)]
    struct DiagnosticsOnly {
        diagnostics: Vec<Diagnostic>,
    }
    #[derive(serde::Deserialize)]
    struct ReportOnly {
        items: Vec<DiagnosticsOnly>,
    }
    let fixture = std::env::var_os("INTO_MD_REPORT_DIAGNOSTICS_FIXTURE").unwrap();
    let output =
        std::path::PathBuf::from(std::env::var_os("INTO_MD_REPORT_REPLAY_OUTPUT").unwrap());
    assert!(!output.exists(), "keep existing evidence unchanged");
    let fixture: DiagnosticsOnly =
        serde_json::from_reader(std::io::BufReader::new(fs::File::open(fixture).unwrap())).unwrap();
    let report = report(fixture.diagnostics.into_iter().map(|value| (&value).into()).collect());
    let legacy = report.to_pretty_json().unwrap_err();
    assert_eq!(legacy.code, DtoErrorCode::ResourceLimit);
    assert!(legacy.detail.contains("dtoValues"));
    let execution =
        context(ResourceLimits { max_memory_bytes: 4 * 1024 * 1024, ..ResourceLimits::default() });
    write_report(&output, &report, &execution).unwrap();
    let decoded: ReportOnly =
        serde_json::from_reader(std::io::BufReader::new(fs::File::open(&output).unwrap())).unwrap();
    let diagnostics: Vec<DiagnosticDto> = decoded
        .items
        .into_iter()
        .next()
        .unwrap()
        .diagnostics
        .into_iter()
        .map(|value| (&value).into())
        .collect();
    assert_eq!(diagnostics, report.items[0].diagnostics);
    assert_eq!(execution.reserved_memory_bytes(), 0);
    assert_eq!(execution.reserved_temporary_bytes(), 0);
    println!(
        "replayed {} exact diagnostics into {} bytes; no OCR invoked",
        diagnostics.len(),
        fs::metadata(output).unwrap().len()
    );
}

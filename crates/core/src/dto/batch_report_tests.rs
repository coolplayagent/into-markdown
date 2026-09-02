use super::*;
use crate::SourceLocator;
use crate::dto::{BatchLimitDto, BatchOcrUsageDto, BatchResourceUsageDto};

fn item() -> BatchItemDto {
    BatchItemDto {
        input: "C:/资料/\"report\".pdf".into(),
        output: Some("out.md".into()),
        format: Some("pdf".into()),
        status: BatchItemStatus::Success,
        outcome: BatchItemOutcome::Degraded,
        diagnostics: vec![DiagnosticDto {
            code: "ocr.lowConfidence".into(),
            severity: DiagnosticSeverityDto::Warning,
            message: "omitted region\nwith a \\ and 中文".into(),
            locator: Some(SourceLocator {
                page: Some(1),
                bounds: Some(crate::Rect { x: 0.0, y: 0.0, width: 842.0, height: 667.0 }),
                rotation_degrees: Some(0.0),
                page_width: Some(842.0),
                page_height: Some(667.0),
                ..SourceLocator::default()
            }),
        }],
        error_code: None,
        reason_code: Some("ocr.lowConfidence".into()),
        component: None,
        part: None,
        limit: None,
        message: None,
        warnings: vec!["warning".into()],
        duration_ms: Some(12.5),
        processing_duration_ms: Some(8.25),
    }
}

#[test]
fn typed_report_matches_existing_bytes_for_optional_fields_and_outcomes() {
    let mut failed = item();
    failed.status = BatchItemStatus::Failed;
    failed.outcome = BatchItemOutcome::Failed;
    failed.output = None;
    failed.format = None;
    failed.error_code = Some("resourceLimit".into());
    failed.reason_code = Some("max_pages".into());
    failed.component = Some("converter".into());
    failed.part = Some("page".into());
    failed.limit = Some(BatchLimitDto { name: "max_pages".into(), detail: Some("limit".into()) });
    failed.message = Some("failed".into());
    failed.processing_duration_ms = None;
    let report = BatchReportDto::try_new_with_resource_usage(
        vec![item(), failed],
        Some(20.0),
        Some(BatchResourceUsageDto {
            memory: None,
            ocr_runtime: None,
            shared_lease_budget_bytes: 1024,
            shared_lease_peak_bytes: 512,
            temporary_lease_budget_bytes: 2048,
            temporary_lease_peak_bytes: 256,
            ocr: Some(BatchOcrUsageDto { recognized_regions: 1, recognized_chars: 2 }),
        }),
    )
    .unwrap();
    for report in [BatchReportDto::try_new(vec![]).unwrap(), report] {
        for (style, expected) in [
            (DtoJsonStyle::Compact, report.to_json().unwrap()),
            (DtoJsonStyle::Pretty, report.to_pretty_json().unwrap()),
        ] {
            let mut bytes = Vec::new();
            report.write_json(style, &mut bytes).unwrap();
            assert_eq!(bytes, expected.as_bytes());
            assert_eq!(BatchReportDto::from_json(&expected).unwrap(), report);
        }
    }
}

#[test]
fn typed_report_streams_past_input_value_limit_without_weakening_decoder() {
    let mut item = item();
    item.diagnostics = vec![item.diagnostics[0].clone(); 56_000];
    let report = BatchReportDto::try_new(vec![item]).unwrap();
    let mut bytes = Vec::new();
    report.write_json(DtoJsonStyle::Compact, &mut bytes).unwrap();
    let json = String::from_utf8(bytes).unwrap();
    let error = BatchReportDto::from_json(&json).unwrap_err();
    assert_eq!(error.code, DtoErrorCode::ResourceLimit);
    assert!(error.detail.contains("dtoValues"));
    let decoded: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded["items"][0]["diagnostics"].as_array().unwrap().len(), 56_000);
    assert_eq!(decoded["items"][0]["processingDurationMs"], 8.25);
    assert_eq!(DtoLimits::default().max_values, 2_000_000);
}

#[test]
fn typed_report_keeps_semantics_and_encoded_byte_boundaries() {
    let mut report = BatchReportDto::try_new(vec![item()]).unwrap();
    for style in [DtoJsonStyle::Compact, DtoJsonStyle::Pretty] {
        let mut bytes = Vec::new();
        report.write_json(style, &mut bytes).unwrap();
        let exact = DtoLimits { max_json_bytes: bytes.len(), ..DtoLimits::default() };
        write_report(&report, style, io::sink(), &exact).unwrap();
        let short = DtoLimits { max_json_bytes: bytes.len() - 1, ..exact };
        let mut destination = Vec::new();
        let error = write_report(&report, style, &mut destination, &short).unwrap_err();
        assert_eq!(error.code, DtoErrorCode::ResourceLimit);
        assert!(error.detail.contains("dtoJsonBytes"));
        assert!(destination.len() <= short.max_json_bytes);
    }
    for limits in [
        DtoLimits { max_string_bytes: 8, ..DtoLimits::default() },
        DtoLimits { max_total_string_bytes: 32, ..DtoLimits::default() },
    ] {
        assert_eq!(
            write_report(&report, DtoJsonStyle::Pretty, io::sink(), &limits).unwrap_err().code,
            DtoErrorCode::ResourceLimit
        );
    }
    report.succeeded = 0;
    let mut destination = Vec::new();
    assert_eq!(
        report.write_json(DtoJsonStyle::Pretty, &mut destination).unwrap_err().code,
        DtoErrorCode::InvalidField
    );
    assert!(destination.is_empty());
}

#[test]
fn typed_report_propagates_partial_destination_failure() {
    struct FailingWriter(usize);
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::ErrorKind::StorageFull.into());
            }
            let written = bytes.len().min(self.0);
            self.0 -= written;
            Ok(written)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let report = BatchReportDto::try_new(vec![item()]).unwrap();
    let error = report.write_json(DtoJsonStyle::Pretty, FailingWriter(31)).unwrap_err();
    assert_eq!(error.code, DtoErrorCode::InvalidJson);
}

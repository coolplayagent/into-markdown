use super::*;

fn read_report(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn text_batches_publish_real_shared_budget_and_peak_without_job_multiplication() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let first = root.join("first.txt");
    let second = root.join("second.txt");
    fs::write(&first, b"first\n").unwrap();
    fs::write(&second, b"second\n").unwrap();

    let explicit_report = root.join("explicit-report.json");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    run(
        vec![
            OsString::from("--no-config"),
            first.clone().into_os_string(),
            second.clone().into_os_string(),
            OsString::from("--output-dir"),
            root.join("explicit-output").into_os_string(),
            OsString::from("--report"),
            explicit_report.clone().into_os_string(),
            OsString::from("--jobs"),
            OsString::from("2"),
            OsString::from("--max-memory-size"),
            OsString::from("16MiB"),
            OsString::from("--ocr"),
            OsString::from("off"),
        ],
        RunContext {
            user_data_anchor: Some(root.join(".test-user-data")),
            stdout: &mut stdout,
            stderr: &mut stderr,
            stdin_is_terminal: true,
            cwd: root.clone(),
        },
    )
    .unwrap();
    let report = read_report(&explicit_report);
    let usage = &report["resourceUsage"];
    assert_eq!(usage["sharedLeaseBudgetBytes"], 16 * 1024 * 1024);
    assert!(usage["sharedLeasePeakBytes"].as_u64().is_some_and(|peak| peak > 0));
    assert!(usage["sharedLeasePeakBytes"].as_u64().unwrap() <= 16 * 1024 * 1024);
    assert!(usage.get("ocr").is_none());

    let automatic_report = root.join("automatic-report.json");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    run(
        vec![
            OsString::from("--no-config"),
            first.into_os_string(),
            second.into_os_string(),
            OsString::from("--output-dir"),
            root.join("automatic-output").into_os_string(),
            OsString::from("--report"),
            automatic_report.clone().into_os_string(),
            OsString::from("--jobs"),
            OsString::from("2"),
            OsString::from("--max-memory-size"),
            OsString::from("auto"),
            OsString::from("--ocr"),
            OsString::from("auto"),
        ],
        RunContext {
            user_data_anchor: Some(root.join(".test-user-data")),
            stdout: &mut stdout,
            stderr: &mut stderr,
            stdin_is_terminal: true,
            cwd: root,
        },
    )
    .unwrap();
    let report = read_report(&automatic_report);
    let usage = &report["resourceUsage"];
    assert_eq!(usage["sharedLeaseBudgetBytes"], config::adaptive_memory_budget());
    assert!(usage["sharedLeasePeakBytes"].as_u64().unwrap() <= config::adaptive_memory_budget());
    assert_eq!(usage["ocr"]["recognizedRegions"], 0);
    assert_eq!(usage["ocr"]["recognizedChars"], 0);
}

fn stopped_plan(root: &Path, name: &str) -> WorkPlan {
    let input = root.join(format!("{name}.txt"));
    fs::write(&input, b"stop me\n").unwrap();
    WorkPlan {
        item: WorkItem {
            input: InputRef::Path(input.clone()),
            display: input.display().to_string(),
            relative: PathBuf::from(format!("{name}.txt")),
            root_label: name.into(),
            input_root: root.to_path_buf(),
            from_directory: false,
            local_path: Some(input),
        },
        output: Some(root.join(format!("{name}.md"))),
        output_root: Some(root.to_path_buf()),
    }
}

#[test]
fn cancellation_and_timeout_reports_keep_terminal_zero_ocr_and_resource_usage() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let cancellation = into_markdown::CancellationToken::new();
    cancellation.cancel();
    for (name, execution) in [
        (
            "cancelled",
            into_markdown::ExecutionOptions {
                cancellation,
                ..into_markdown::ExecutionOptions::default()
            },
        ),
        (
            "timeout",
            into_markdown::ExecutionOptions {
                timeout: Some(std::time::Duration::ZERO),
                ..into_markdown::ExecutionOptions::default()
            },
        ),
    ] {
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Auto;
        options.limits.max_memory_bytes = 16 * 1024 * 1024;
        let policy = ExecutionPolicy {
            output_context: into_markdown::ExecutionContext::new(
                execution.clone(),
                options.limits.clone(),
            ),
            options,
            execution,
            loaded: config::load(&root, &[], true, None, None).unwrap(),
            hint: FormatHint::default(),
            emit: EmitKind::Markdown,
            asset_mode: AssetModeArg::Extract,
            conflict: ConflictPolicy::Error,
            assets_dir: None,
            working_directory: root.clone(),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut run_context = RunContext {
            user_data_anchor: Some(root.join(".test-user-data")),
            stdout: &mut stdout,
            stderr: &mut stderr,
            stdin_is_terminal: true,
            cwd: root.clone(),
        };
        let reports = execute_plans(
            vec![stopped_plan(&root, name)],
            &policy,
            1,
            true,
            Catalog::new(crate::args::Language::En),
            true,
            &mut run_context,
        )
        .unwrap();
        assert_eq!(reports[0].error_code.as_deref(), Some(name));

        let report_path = root.join(format!("{name}-report.json"));
        let error = finish_reports(
            reports,
            FinishReportsContext {
                report_path: Some(&report_path),
                global: &crate::args::GlobalArgs::default(),
                catalog: Catalog::new(crate::args::Language::En),
                json_log: true,
                stderr: &mut stderr,
                output_context: &policy.output_context,
                wall_duration_ms: 0.0,
                ocr_enabled: true,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "partialFailure");
        let report = read_report(&report_path);
        assert_eq!(report["items"][0]["errorCode"], name);
        assert_eq!(report["resourceUsage"]["sharedLeaseBudgetBytes"], 16 * 1024 * 1024);
        assert_eq!(report["resourceUsage"]["sharedLeasePeakBytes"], 0);
        assert_eq!(report["resourceUsage"]["ocr"]["recognizedRegions"], 0);
        assert_eq!(report["resourceUsage"]["ocr"]["recognizedChars"], 0);
    }
}

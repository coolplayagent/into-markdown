use super::*;

fn read_report(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn every_embedded_visual_entry_assembles_ocr_and_preserves_legacy_auto_routing() {
    let root = tempfile::tempdir().unwrap();
    let mut plan = stopped_plan(root.path(), "visual");
    for extension in [
        "pdf", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "odt", "ods", "odp", "rtf", "epub",
        "html", "ipynb", "zip", "msg", "png",
    ] {
        plan.item.local_path = Some(root.path().join(format!("visual.{extension}")));
        let mut options = ConversionOptions::default();
        for policy in [OcrPolicy::Auto, OcrPolicy::Always] {
            options.ocr.policy = policy;
            let needs = invocation_capabilities(std::slice::from_ref(&plan), None, None, &options);
            let legacy_auto =
                policy == OcrPolicy::Auto && matches!(extension, "doc" | "ppt" | "xls");
            assert_eq!(needs.ocr, !legacy_auto, "{extension} {policy:?}");
        }
    }
}

#[test]
fn exhausted_automatic_budget_is_a_resource_refusal_before_output_admission() {
    let root = tempfile::tempdir().unwrap();
    let mut loaded = config::load(root.path(), &[], true, None, None).unwrap();
    loaded.memory_snapshot = config::memory::select(Some(16 * 1024_u64.pow(3)), Some(0));
    let error = resource_usage::prepare(
        &ConversionArgs {
            max_memory_size: Some(crate::args::MemorySizeArg::Auto),
            ..Default::default()
        },
        &mut loaded,
    )
    .unwrap_err();
    assert_eq!(error.code(), "resourceLimit");
    assert_eq!(error.exit_code(), 5);
    assert_eq!(error.limit().unwrap().0, "max_memory_bytes");
    assert!(error.message().contains("availableBytes=Some(0)"));
    assert!(root.path().read_dir().unwrap().next().is_none());
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
    let mut loaded = config::load(&root, &[], true, None, None).unwrap();
    loaded.memory_snapshot =
        config::memory::select(Some(16 * 1024_u64.pow(3)), Some(12 * 1024_u64.pow(3)));
    run_conversion(
        ConversionArgs {
            inputs: vec![first.into_os_string(), second.into_os_string()],
            output_dir: Some(root.join("automatic-output")),
            report: Some(automatic_report.clone()),
            jobs: std::num::NonZero::new(4),
            max_memory_size: Some(crate::args::MemorySizeArg::Auto),
            ocr: Some(crate::args::OcrPolicyArg::Auto),
            ..Default::default()
        },
        &crate::args::GlobalArgs::default(),
        loaded,
        Catalog::new(crate::args::Language::En),
        false,
        &mut RunContext {
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
    assert_eq!(usage["sharedLeaseBudgetBytes"], usage["memory"]["autoBudgetBytes"]);
    assert_eq!(usage["memory"]["automatic"], true);
    assert!(
        usage["sharedLeasePeakBytes"].as_u64().unwrap()
            <= usage["sharedLeaseBudgetBytes"].as_u64().unwrap()
    );
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
            services: into_markdown::Services::default(),
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
                memory_snapshot: into_markdown::MemoryBudgetSnapshotDto {
                    effective_budget_bytes: policy.options.limits.max_memory_bytes,
                    automatic: false,
                    ..config::memory::select(None, None)
                },
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

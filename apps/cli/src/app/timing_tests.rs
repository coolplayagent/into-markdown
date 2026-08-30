use super::*;

#[test]
fn single_and_parallel_reports_publish_real_timing_boundaries() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let first = root.join("first.txt");
    let second = root.join("second.txt");
    fs::write(&first, b"first\n").unwrap();
    fs::write(&second, b"second\n").unwrap();

    for (inputs, jobs, name) in [
        (vec![first.clone()], "1", "single"),
        (vec![first.clone(), second.clone()], "2", "parallel"),
    ] {
        let output_dir = root.join(format!("{name}-output"));
        let report_path = root.join(format!("{name}-report.json"));
        let mut arguments = vec![OsString::from("--no-config")];
        arguments.extend(inputs.into_iter().map(PathBuf::into_os_string));
        arguments.extend([
            OsString::from("--output-dir"),
            output_dir.into_os_string(),
            OsString::from("--report"),
            report_path.clone().into_os_string(),
            OsString::from("--jobs"),
            OsString::from(jobs),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run(
            arguments,
            RunContext {
                user_data_anchor: Some(root.join(".test-user-data")),
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdin_is_terminal: true,
                cwd: root.clone(),
            },
        )
        .unwrap();

        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
        let wall = report["wallDurationMs"].as_f64().unwrap();
        assert_eq!(report["items"].as_array().unwrap().len(), usize::from(jobs == "2") + 1);
        for item in report["items"].as_array().unwrap() {
            let duration = item["durationMs"].as_f64().unwrap();
            let processing = item["processingDurationMs"].as_f64().unwrap();
            assert!(wall >= duration);
            assert!(duration >= processing);
            assert!(processing >= 0.0);
            assert!(!item["output"].as_str().unwrap().contains('\\'));
        }
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.contains("timing: "));
        assert!(stderr.contains("timing: batch wall "));
    }
}

#[test]
fn failures_remain_auditable_with_elapsed_time() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let invalid = root.join("invalid.epub");
    fs::write(&invalid, b"PK\x03\x04broken ZIP archive").unwrap();
    let report_path = root.join("failure-report.json");
    let arguments = vec![
        OsString::from("--no-config"),
        invalid.into_os_string(),
        OsString::from("--output"),
        root.join("failure.md").into_os_string(),
        OsString::from("--report"),
        report_path.clone().into_os_string(),
    ];
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let error = run(
        arguments,
        RunContext {
            user_data_anchor: Some(root.join(".test-user-data")),
            stdout: &mut stdout,
            stderr: &mut stderr,
            stdin_is_terminal: true,
            cwd: root,
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "partialFailure");
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
    assert_eq!(report["items"][0]["status"], "failed");
    assert!(report["items"][0]["durationMs"].as_f64().is_some_and(|value| value >= 0.0));
    assert!(report["items"][0].get("processingDurationMs").is_none());
    assert!(
        report["wallDurationMs"].as_f64().unwrap()
            >= report["items"][0]["durationMs"].as_f64().unwrap()
    );
}

#[test]
fn cancelled_worker_item_records_elapsed_time_without_a_fake_processing_stage() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let input = root.join("cancelled.txt");
    let output = root.join("cancelled.md");
    fs::write(&input, b"cancel me\n").unwrap();
    let cancellation = into_markdown::CancellationToken::new();
    cancellation.cancel();
    let execution = into_markdown::ExecutionOptions {
        cancellation,
        ..into_markdown::ExecutionOptions::default()
    };
    let options = ConversionOptions::default();
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
    let plan = WorkPlan {
        item: WorkItem {
            input: InputRef::Path(input.clone()),
            display: input.display().to_string(),
            relative: PathBuf::from("cancelled.txt"),
            root_label: "cancelled".into(),
            input_root: root.clone(),
            from_directory: false,
            local_path: Some(input),
        },
        output: Some(output),
        output_root: Some(root),
    };

    let report = process_file_task(plan, &policy, &Mutex::new(()), &Mutex::new(()));

    assert_eq!(report.status, BatchItemStatus::Failed);
    assert_eq!(report.error_code.as_deref(), Some("cancelled"));
    assert!(report.duration_ms.is_some_and(|value| value >= 0.0));
    assert_eq!(report.processing_duration_ms, None);
}

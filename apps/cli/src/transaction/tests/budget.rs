use super::*;

#[test]
fn stage_failure_fsync_failure_budget_and_cancellation_leave_old_set() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let first = root.join("one.md");
    let second = root.join("two.bin");
    fs::write(&first, b"old-one").unwrap();
    fs::write(&second, b"old-two").unwrap();
    let targets = [
        Target { path: first.clone(), bytes: b"new-one" },
        Target { path: second.clone(), bytes: b"new-two" },
    ];

    for (phase, index, code) in
        [("beforeStage", 1, "injectedStageFailure"), ("beforeStageSync", 0, "injectedFsyncFailure")]
    {
        let error = prepare_with_hook(&targets, true, &context(), |seen, seen_index| {
            if seen == phase && seen_index == index {
                Err(CliError::new(ExitClass::Io, code, "injected"))
            } else {
                Ok(HookDecision::Continue)
            }
        })
        .err()
        .expect("injected prepare failure");
        assert_eq!(error.code(), code);
        assert_eq!(fs::read(&first).unwrap(), b"old-one");
        assert_eq!(fs::read(&second).unwrap(), b"old-two");
        assert!(manager_directories(&root).is_empty());
    }

    let limited = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_temporary_bytes: 4, ..ResourceLimits::default() },
    );
    let error = prepare(&targets, true, &limited).err().expect("temporary budget failure");
    assert_eq!(error.code(), "resourceLimit");
    assert!(manager_directories(&root).is_empty());

    let token = into_markdown::CancellationToken::new();
    let cancelled = ExecutionContext::new(
        ExecutionOptions { cancellation: token.clone(), ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let transaction = prepare(&targets, true, &cancelled).unwrap();
    token.cancel();
    let error = transaction.commit().unwrap_err();
    assert_eq!(error.code(), "cancelled");
    assert_eq!(fs::read(&first).unwrap(), b"old-one");
    assert_eq!(fs::read(&second).unwrap(), b"old-two");
    assert!(manager_directories(&root).is_empty());
    assert_eq!(cancelled.reserved_memory_bytes(), 0);
    assert_eq!(cancelled.reserved_temporary_bytes(), 0);
}

#[test]
fn storage_full_at_stage_and_commit_boundaries_restores_the_old_set() {
    let phases = [("beforeStageSync", false), ("targetInstalled", true)];
    for (phase, commit_phase) in phases {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let first = root.join("one.md");
        let second = root.join("two.bin");
        fs::write(&first, b"old-one").unwrap();
        fs::write(&second, b"old-two").unwrap();
        let targets = [
            Target { path: first.clone(), bytes: b"new-one" },
            Target { path: second.clone(), bytes: b"new-two" },
        ];
        let execution = context();
        let fault = || CliError::from(io::Error::from(io::ErrorKind::StorageFull));
        let result = if commit_phase {
            prepare(&targets, true, &execution).unwrap().commit_with_hook(|seen, _| {
                if seen == phase { Err(fault()) } else { Ok(HookDecision::Continue) }
            })
        } else {
            prepare_with_hook(&targets, true, &execution, |seen, _| {
                if seen == phase { Err(fault()) } else { Ok(HookDecision::Continue) }
            })
            .and_then(PreparedTransaction::commit)
        };
        assert_eq!(result.unwrap_err().code(), "io", "{phase}");
        assert_eq!(fs::read(&first).unwrap(), b"old-one", "{phase}");
        assert_eq!(fs::read(&second).unwrap(), b"old-two", "{phase}");
        assert!(manager_artifacts(&root).is_empty(), "{phase}");
        assert_eq!(execution.reserved_memory_bytes(), 0, "{phase}");
        assert_eq!(execution.reserved_temporary_bytes(), 0, "{phase}");
    }
}

#[test]
fn cancellation_during_streaming_releases_every_reservation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output = root.join("cancelled.md");
    let token = into_markdown::CancellationToken::new();
    let execution = ExecutionContext::new(
        ExecutionOptions { cancellation: token.clone(), ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let mut stream = StreamingFileTransaction::begin(&output, false, &execution).unwrap();
    stream.write_all_checked(b"first chunk").unwrap();
    token.cancel();
    assert_eq!(stream.write_all_checked(b"cancelled chunk").unwrap_err().code(), "cancelled");
    assert!(stream.write_all_checked(b"cannot reuse a failed stream").is_err());
    assert!(manager_artifacts(&root).is_empty());
    assert_eq!(execution.reserved_memory_bytes(), 0);
    assert_eq!(execution.reserved_temporary_bytes(), 0);
    stream.abort().unwrap();
    assert!(!output.exists());
    assert!(manager_artifacts(&root).is_empty());
    assert_eq!(execution.reserved_memory_bytes(), 0);
    assert_eq!(execution.reserved_temporary_bytes(), 0);
}

#[test]
fn begin_target_failure_after_registration_closes_and_releases_every_reservation() {
    for injected_phase in ["directoryCreated", "directoryIdentityBound"] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let output = root.join("document.md");
        let asset_directory = root.join("assets");
        let execution = context();
        let mut stream = StreamingFileTransaction::begin_with_root_hint(
            &output,
            Some(&asset_directory),
            false,
            &execution,
        )
        .unwrap();
        stream.write_all_checked(b"markdown").unwrap();

        let error = stream
            .begin_target_with_hook(&asset_directory.join("asset.bin"), false, |phase, _| {
                if phase == injected_phase {
                    Err(CliError::new(ExitClass::Io, "injectedTargetFailure", "injected"))
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .unwrap_err();

        assert_eq!(error.code(), "injectedTargetFailure", "{injected_phase}");
        assert!(stream.begin_target(&asset_directory.join("retry.bin"), false).is_err());
        assert!(stream.write_all_checked(b"retry").is_err());
        assert!(!output.exists(), "{injected_phase}");
        assert!(!asset_directory.exists(), "{injected_phase}");
        assert!(manager_artifacts(&root).is_empty(), "{injected_phase}");
        assert_eq!(execution.reserved_memory_bytes(), 0, "{injected_phase}");
        assert_eq!(execution.reserved_temporary_bytes(), 0, "{injected_phase}");
    }
}

#[test]
fn timeout_during_streaming_closes_and_releases_every_reservation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output = root.join("timed-out.md");
    let execution = ExecutionContext::new(
        ExecutionOptions {
            timeout: Some(std::time::Duration::from_millis(500)),
            ..ExecutionOptions::default()
        },
        ResourceLimits::default(),
    );
    let mut stream = StreamingFileTransaction::begin(&output, false, &execution).unwrap();
    stream.write_all_checked(b"first chunk").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(600));

    assert_eq!(stream.write_all_checked(b"timed out").unwrap_err().code(), "timeout");
    assert!(stream.write_all_checked(b"cannot reuse a failed stream").is_err());
    assert!(!output.exists());
    assert!(manager_artifacts(&root).is_empty());
    assert_eq!(execution.reserved_memory_bytes(), 0);
    assert_eq!(execution.reserved_temporary_bytes(), 0);
}

#[test]
fn low_temporary_budget_rejects_one_hundred_thousand_missing_parents_before_mkdir() {
    const TARGET_COUNT: usize = 100_000;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let targets = (0..TARGET_COUNT)
        .map(|index| Target {
            path: root.join(format!("parent-{index:06}/output.bin")),
            bytes: &[] as &[u8],
        })
        .collect::<Vec<_>>();
    let execution = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_temporary_bytes: 1024 * 1024, ..ResourceLimits::default() },
    );

    let error = prepare(&targets, false, &execution).err().expect("temporary budget failure");

    assert_eq!(error.code(), "resourceLimit");
    assert!(fs::read_dir(&root).unwrap().next().is_none());
    assert_eq!(execution.reserved_memory_bytes(), 0);
    assert_eq!(execution.reserved_temporary_bytes(), 0);
}

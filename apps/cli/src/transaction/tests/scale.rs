use super::*;

#[test]
fn successful_transaction_removes_the_empty_registry() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output = root.join("document.md");
    prepare(&[Target { path: output.clone(), bytes: b"document" }], false, &context())
        .unwrap()
        .commit()
        .unwrap();

    assert_eq!(fs::read(output).unwrap(), b"document");
    assert!(manager_artifacts(&root).is_empty());
}

#[test]
fn large_streaming_success_removes_registry_and_parent_lease() {
    const CHUNK_BYTES: usize = 64 * 1024;
    const CHUNK_COUNT: usize = 512;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output = root.join("large.md");
    let total = u64::try_from(CHUNK_BYTES * CHUNK_COUNT).unwrap();
    let limits =
        ResourceLimits { max_temporary_bytes: total + 1024 * 1024, ..ResourceLimits::default() };
    let context = ExecutionContext::new(ExecutionOptions::default(), limits);
    let chunk = vec![b'x'; CHUNK_BYTES];
    let mut transaction = StreamingFileTransaction::begin(&output, false, &context).unwrap();
    for _ in 0..CHUNK_COUNT {
        transaction.write_all_checked(&chunk).unwrap();
    }
    let temporary_peak = context.reserved_temporary_bytes();
    let memory_peak = context.reserved_memory_bytes();
    eprintln!(
        "transaction_metrics case=32m_stream temporary_peak={temporary_peak} memory_peak={memory_peak}"
    );
    assert!(temporary_peak >= total + TRANSACTION_METADATA_TEMPORARY_BYTES);
    assert!(temporary_peak <= total + 2 * 1024 * 1024);
    assert!(memory_peak > 0);
    assert!(memory_peak <= 2 * 1024 * 1024);
    transaction.seal().unwrap().commit().unwrap();

    assert_eq!(fs::metadata(output).unwrap().len(), total);
    assert!(manager_artifacts(&root).is_empty());
    assert_eq!(context.reserved_memory_bytes(), 0);
    assert_eq!(context.reserved_temporary_bytes(), 0);
}

#[test]
fn completed_sibling_does_not_remove_an_active_transaction() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let active_parent = root.join("active");
    let completed_parent = root.join("completed");
    fs::create_dir_all(&active_parent).unwrap();
    fs::create_dir_all(&completed_parent).unwrap();
    let active_path = active_parent.join("active.md");
    let completed_path = completed_parent.join("completed.md");
    let mut active = StreamingFileTransaction::begin_with_root_hint(
        &active_path,
        Some(&completed_parent),
        false,
        &context(),
    )
    .unwrap();
    active.write_all_checked(b"active").unwrap();
    let active_directory = manager_directories(&root).pop().unwrap();

    let mut completed = StreamingFileTransaction::begin_with_root_hint(
        &completed_path,
        Some(&active_parent),
        false,
        &context(),
    )
    .unwrap();
    completed.write_all_checked(b"completed").unwrap();
    completed.seal().unwrap().commit().unwrap();

    assert_eq!(fs::read(completed_path).unwrap(), b"completed");
    assert!(active_directory.exists());
    assert_eq!(manager_directories(&root), vec![active_directory]);
    assert!(active_parent.join(PARENT_LEASE_NAME).is_file());
    assert!(!completed_parent.join(PARENT_LEASE_NAME).exists());

    active.seal().unwrap().commit().unwrap();
    assert_eq!(fs::read(active_path).unwrap(), b"active");
    assert!(manager_artifacts(&root).is_empty());
}

#[test]
fn streaming_stage_crash_boundaries_never_publish_partial_payloads() {
    for phase in ["journalCreated", "stageAllocated"] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let output = root.join("document.md");
        let error = prepare_sources_with_hook_internal(
            &[Target { path: output.clone(), bytes: &[] }],
            false,
            &context(),
            |current, _| {
                Ok(if current == phase {
                    HookDecision::SimulateCrash
                } else {
                    HookDecision::Continue
                })
            },
            true,
            &[],
        )
        .err()
        .expect("crash point must stop streaming preparation");
        assert_eq!(error.code(), "simulatedCrash", "{phase}");
        recover_pending(&root).unwrap_or_else(|error| panic!("{phase}: {error:?}"));
        assert!(!output.exists(), "{phase}");
        assert!(manager_directories(&root).is_empty(), "{phase}");
    }

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output = root.join("document.md");
    let mut stream = StreamingFileTransaction::begin(&output, false, &context()).unwrap();
    stream.write_all_checked(b"partial").unwrap();
    stream.abandon_for_test();
    recover_pending(&root).unwrap();
    assert!(!output.exists());
    assert!(manager_directories(&root).is_empty());

    let mut stream = StreamingFileTransaction::begin(&output, false, &context()).unwrap();
    stream.write_all_checked(b"sealed but unpublished").unwrap();
    let transaction = stream.seal().unwrap();
    transaction.abandon_for_test();
    recover_pending(&root).unwrap();
    assert!(!output.exists());
    assert!(manager_directories(&root).is_empty());
}

#[test]
fn dynamic_stream_target_recovery_removes_partial_payload_and_created_directory() {
    for seal_asset in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let output = root.join("document.md");
        let asset_directory = root.join("assets/nested");
        let asset = asset_directory.join("asset.bin");
        let mut stream = StreamingFileTransaction::begin_with_root_hint(
            &output,
            Some(&asset_directory),
            false,
            &context(),
        )
        .unwrap();
        stream.write_all_checked(b"markdown").unwrap();
        stream.begin_target(&asset, false).unwrap();
        stream.write_all_checked(b"partial asset").unwrap();
        if seal_asset {
            stream.seal_current().unwrap();
        }
        stream.abandon_for_test();
        recover_pending(&root).unwrap();
        assert!(!output.exists());
        assert!(!asset.exists());
        assert!(!asset_directory.exists());
        assert!(!root.join("assets").exists());
        assert!(manager_directories(&root).is_empty());
    }
}

#[test]
fn same_parent_dynamic_targets_use_bounded_journal_snapshots_and_recover_orphan_stages() {
    const ASSET_COUNT: usize = 256;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output = root.join("document.md");
    let asset_directory = root.join("assets");
    fs::create_dir(&asset_directory).unwrap();
    let mut stream = StreamingFileTransaction::begin_with_root_hint(
        &output,
        Some(&asset_directory),
        false,
        &context(),
    )
    .unwrap();
    stream.write_all_checked(b"markdown").unwrap();
    for index in 0..ASSET_COUNT {
        stream.begin_target(&asset_directory.join(format!("asset-{index}.bin")), false).unwrap();
        stream.write_all_checked(b"asset").unwrap();
    }
    let transaction = stream.transaction.as_ref().unwrap();
    assert_eq!(transaction.journal_persist_calls, 2);
    assert_eq!(transaction.journal_record_calls, ASSET_COUNT as u64 * 2);
    assert!(transaction.journal_record_bytes < ASSET_COUNT as u64 * 4 * 1024);
    assert_eq!(transaction.journal_record_sync_calls, 1);
    assert_eq!(stream.target_index.as_ref().unwrap().target_lookups.get(), ASSET_COUNT as u64);
    eprintln!(
        "transaction_metrics case=256_assets journal_records={} journal_bytes={} journal_syncs={} temporary_peak={} memory_peak={}",
        transaction.journal_record_calls,
        transaction.journal_record_bytes,
        transaction.journal_record_sync_calls,
        transaction.context.reserved_temporary_bytes(),
        transaction.context.reserved_memory_bytes()
    );

    stream.abandon_for_test();
    recover_pending(&root).unwrap();
    assert!(!output.exists());
    assert!(fs::read_dir(&asset_directory).unwrap().next().is_none());
    assert!(manager_directories(&root).is_empty());
}

#[test]
fn two_hundred_fifty_six_assets_commit_as_one_complete_set() {
    const ASSET_COUNT: usize = 256;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output = root.join("document.md");
    let asset_directory = root.join("assets");
    fs::create_dir(&asset_directory).unwrap();
    let execution = context();
    let mut stream = StreamingFileTransaction::begin_with_root_hint(
        &output,
        Some(&asset_directory),
        false,
        &execution,
    )
    .unwrap();
    stream.write_all_checked(b"markdown").unwrap();
    for index in 0..ASSET_COUNT {
        stream.begin_target(&asset_directory.join(format!("asset-{index}.bin")), false).unwrap();
        stream.write_all_checked(index.to_string().as_bytes()).unwrap();
    }
    let paths = stream.seal().unwrap().commit().unwrap();

    assert_eq!(paths.len(), ASSET_COUNT + 1);
    assert_eq!(fs::read(&output).unwrap(), b"markdown");
    for index in 0..ASSET_COUNT {
        assert_eq!(
            fs::read(asset_directory.join(format!("asset-{index}.bin"))).unwrap(),
            index.to_string().as_bytes()
        );
    }
    assert!(manager_artifacts(&root).is_empty());
    assert_eq!(execution.reserved_memory_bytes(), 0);
    assert_eq!(execution.reserved_temporary_bytes(), 0);
}

#[test]
fn streaming_target_index_lookup_count_is_linear_at_one_hundred_thousand_targets() {
    const TARGET_COUNT: usize = 100_000;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let primary = root.join("document.md");
    let context = context();
    let started = std::time::Instant::now();
    let primary_parent = FileIdentity { platform: "test".into(), first: 1, second: 1, size: 0 };
    let mut index = StreamingTargetIndex::new(primary, None, primary_parent, &context).unwrap();
    for number in 0..TARGET_COUNT {
        let target = root.join(format!("assets/asset-{number}.bin"));
        assert!(!index.contains_target(&target));
        index.insert_target(target).unwrap();
        let parent = FileIdentity {
            platform: "test".into(),
            first: u64::try_from(number).unwrap() + 2,
            second: 1,
            size: 0,
        };
        assert!(!index.contains_parent(&parent));
        index.insert_parent(parent).unwrap();
    }
    assert_eq!(index.target_lookups.get(), TARGET_COUNT as u64);
    assert_eq!(index.parent_lookups.get(), TARGET_COUNT as u64);
    assert_eq!(index.targets.len(), TARGET_COUNT + 1);
    let elapsed = started.elapsed();
    eprintln!(
        "transaction_metrics case=100k_index elapsed_ms={} target_lookups={} parent_lookups={} memory_peak={}",
        elapsed.as_millis(),
        index.target_lookups.get(),
        index.parent_lookups.get(),
        context.reserved_memory_bytes()
    );
    drop(index);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn one_hundred_thousand_same_parent_targets_share_one_authenticated_handle() {
    const TARGET_COUNT: usize = 100_000;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let root_handle = SafeDir::open_absolute(&root).unwrap();
    let execution = context();
    let entries = (0..TARGET_COUNT)
        .map(|index| JournalEntry {
            target: encode_path(Path::new(&format!("target-{index:06}.md"))).unwrap(),
            parent_index: Some(0),
            original: None,
            content_sha256: String::new(),
            size: 0,
            staged_identity: None,
            state: EntryState::Prepared,
        })
        .collect::<Vec<_>>();

    let parent_identities = vec![root_handle.identity.clone()];
    let mut authenticator = TargetAuthenticator::new(&root_handle, &execution).unwrap();
    let mut first = None;
    for entry in &entries {
        let target = authenticator.authenticate(entry, &parent_identities).unwrap();
        let current = Arc::as_ptr(&target.parent);
        if let Some(first) = first {
            assert_eq!(current, first);
        } else {
            first = Some(current);
        }
    }
    assert_eq!(authenticator.cached_parent_count(), 1);
    assert_eq!(TargetAuthenticator::cached_parent_limit(), 64);
    assert!(execution.reserved_memory_bytes() > 0);
    drop(authenticator);
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn one_hundred_thousand_distinct_parent_keys_keep_a_fixed_handle_window() {
    const OPERATION_COUNT: usize = 100_000;
    const PARENT_COUNT: usize = 65;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let root_handle = SafeDir::open_absolute(&root).unwrap();
    let execution = context();
    let mut parent_identities = Vec::with_capacity(PARENT_COUNT);
    for index in 0..PARENT_COUNT {
        let relative = PathBuf::from(format!("parent-{index:02}"));
        fs::create_dir(root.join(&relative)).unwrap();
        parent_identities.push(root_handle.open_descendant(&relative).unwrap().identity);
    }
    let mut authenticator = TargetAuthenticator::new(&root_handle, &execution).unwrap();
    for operation in 0..OPERATION_COUNT {
        let parent_index = operation % PARENT_COUNT;
        let entry = JournalEntry {
            target: encode_path(Path::new(&format!(
                "parent-{parent_index:02}/target-{operation:06}.md"
            )))
            .unwrap(),
            parent_index: Some(parent_index),
            original: None,
            content_sha256: String::new(),
            size: 0,
            staged_identity: None,
            state: EntryState::Prepared,
        };
        drop(authenticator.authenticate(&entry, &parent_identities).unwrap());
        assert!(authenticator.cached_parent_count() <= TargetAuthenticator::cached_parent_limit());
    }
    assert_eq!(authenticator.cached_parent_count(), TargetAuthenticator::cached_parent_limit());
    drop(authenticator);
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn dynamic_directory_creation_is_recoverable_at_every_durable_boundary() {
    for phase in [
        "directoryIntentPersisted",
        "directoryCreated",
        "directoryIdentityBound",
        "dynamicParentLeased",
        "dynamicStageAllocated",
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let output = root.join("document.md");
        let asset_directory = root.join("new-assets/nested");
        let mut stream = StreamingFileTransaction::begin_with_root_hint(
            &output,
            Some(&asset_directory),
            false,
            &context(),
        )
        .unwrap();
        stream.write_all_checked(b"markdown").unwrap();
        let error = stream
            .begin_target_with_hook(&asset_directory.join("asset.bin"), false, |current, _| {
                Ok(if current == phase {
                    HookDecision::SimulateCrash
                } else {
                    HookDecision::Continue
                })
            })
            .unwrap_err();
        assert_eq!(error.code(), "simulatedCrash", "{phase}");
        stream.abandon_for_test();
        recover_pending(&root).unwrap_or_else(|error| panic!("{phase}: {error:?}"));
        assert!(!output.exists(), "{phase}");
        assert!(!root.join("new-assets").exists(), "{phase}");
        assert!(manager_directories(&root).is_empty(), "{phase}");
    }

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output = root.join("document.md");
    let asset_directory = root.join("existing-assets");
    fs::create_dir(&asset_directory).unwrap();
    let mut stream = StreamingFileTransaction::begin_with_root_hint(
        &output,
        Some(&asset_directory),
        false,
        &context(),
    )
    .unwrap();
    let error = stream
        .begin_target_with_hook(&asset_directory.join("asset.bin"), false, |phase, _| {
            Ok(if phase == "directoryIdentityBound" {
                HookDecision::SimulateCrash
            } else {
                HookDecision::Continue
            })
        })
        .unwrap_err();
    assert_eq!(error.code(), "simulatedCrash");
    stream.abandon_for_test();
    recover_pending(&root).unwrap();
    assert!(asset_directory.is_dir());
}

#[test]
fn static_directory_intent_recovery_removes_only_authenticated_created_parents() {
    for phase in ["directoryIntentPersisted", "directoryCreated"] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let existing = root.join("existing");
        fs::create_dir(&existing).unwrap();
        let target = root.join("created/nested/output.md");
        let error = prepare_with_hook(
            &[Target { path: target, bytes: b"output" }],
            false,
            &context(),
            |current, _| {
                Ok(if current == phase {
                    HookDecision::SimulateCrash
                } else {
                    HookDecision::Continue
                })
            },
        )
        .err()
        .expect("injected preparation crash");
        assert_eq!(error.code(), "simulatedCrash", "{phase}");
        recover_pending(&root).unwrap_or_else(|error| panic!("{phase}: {error:?}"));
        assert!(!root.join("created").exists(), "{phase}");
        assert!(existing.is_dir(), "{phase}");
        assert!(manager_directories(&root).is_empty(), "{phase}");
    }
}

use super::*;
use std::fs::OpenOptions;

#[test]
fn journal_slot_boundary_accepts_maximum_payload_plus_its_newline_only() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("journal.json");
    let mut file = OpenOptions::new().read(true).write(true).create_new(true).open(path).unwrap();
    file.set_len(MAX_JOURNAL_BYTES).unwrap();
    assert!(super::super::journal::slot_boundary_is_valid(&mut file, MAX_JOURNAL_BYTES).unwrap());

    file.set_len(MAX_JOURNAL_BYTES + 1).unwrap();
    assert!(
        !super::super::journal::slot_boundary_is_valid(&mut file, MAX_JOURNAL_BYTES + 1).unwrap()
    );
    file.seek(io::SeekFrom::End(-1)).unwrap();
    file.write_all(b"\n").unwrap();
    assert!(
        super::super::journal::slot_boundary_is_valid(&mut file, MAX_JOURNAL_BYTES + 1).unwrap()
    );
}

#[test]
fn journal_decode_memory_is_reserved_before_deserialization_at_its_exact_boundary() {
    let file_bytes = 128 * 1024;
    let required = super::super::journal::journal_decode_memory_bytes(file_bytes).unwrap();
    let insufficient = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_memory_bytes: required - 1, ..ResourceLimits::default() },
    );
    let error = insufficient.reserve_memory(required).map_err(CliError::from).unwrap_err();
    assert_eq!(error.code(), "resourceLimit");
    assert_eq!(insufficient.reserved_memory_bytes(), 0);

    let exact = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_memory_bytes: required, ..ResourceLimits::default() },
    );
    let reservation = exact.reserve_memory(required).unwrap();
    assert_eq!(exact.reserved_memory_bytes(), required);
    drop(reservation);
    assert_eq!(exact.reserved_memory_bytes(), 0);
}

#[test]
fn recovery_journal_reads_obey_and_release_memory_and_temporary_budgets() {
    for (memory, temporary_bytes) in [(1, u64::MAX), (u64::MAX, 1)] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let output = root.join("document.md");
        let transaction =
            prepare(&[Target { path: output.clone(), bytes: b"new" }], false, &context()).unwrap();
        transaction.abandon_for_test();
        let limited = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits {
                max_memory_bytes: memory,
                max_temporary_bytes: temporary_bytes,
                ..ResourceLimits::default()
            },
        );

        let error = super::super::recovery::recover_root_transactions(&root, &limited).unwrap_err();
        assert_eq!(error.code(), "resourceLimit");
        assert_eq!(limited.reserved_memory_bytes(), 0);
        assert_eq!(limited.reserved_temporary_bytes(), 0);
        assert!(!output.exists());
        assert_eq!(manager_directories(&root).len(), 1);

        recover_pending(&root).unwrap();
        assert!(manager_artifacts(&root).is_empty());
    }
}

#[test]
fn cancelled_request_cleanup_scope_can_recover_and_releases_journal_leases() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output = root.join("document.md");
    let transaction =
        prepare(&[Target { path: output.clone(), bytes: b"new" }], false, &context()).unwrap();
    transaction.abandon_for_test();
    let token = into_markdown::CancellationToken::new();
    let cancelled = ExecutionContext::new(
        ExecutionOptions { cancellation: token.clone(), ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    token.cancel();
    let cleanup = cancelled.cleanup_scope();

    super::super::recovery::recover_root_transactions(&root, &cleanup).unwrap();

    assert!(!output.exists());
    assert!(manager_artifacts(&root).is_empty());
    assert_eq!(cancelled.reserved_memory_bytes(), 0);
    assert_eq!(cancelled.reserved_temporary_bytes(), 0);
}

#[test]
fn every_durable_phase_is_recoverable_by_a_new_manager() {
    let phases = [
        "journalCreated",
        "stageAllocated",
        "stageWritten",
        "stageSynced",
        "prepared",
        "committing",
        "backupRenamed",
        "backupJournaled",
        "targetInstalled",
        "installJournaled",
        "committed",
    ];
    for phase in phases {
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
        let mut fired = false;
        let result = prepare_with_hook(&targets, true, &context(), |seen, _| {
            if !fired && seen == phase {
                fired = true;
                Ok(HookDecision::SimulateCrash)
            } else {
                Ok(HookDecision::Continue)
            }
        });
        let result = match result {
            Ok(mut transaction) => transaction.commit_with_hook(|seen, _| {
                if !fired && seen == phase {
                    fired = true;
                    Ok(HookDecision::SimulateCrash)
                } else {
                    Ok(HookDecision::Continue)
                }
            }),
            Err(error) => Err(error),
        };
        assert_eq!(result.unwrap_err().code(), "simulatedCrash", "{phase}");
        recover_pending(&root).unwrap();
        let values = (fs::read(&first).unwrap(), fs::read(&second).unwrap());
        let expected = if phase == "committed" {
            (b"new-one".to_vec(), b"new-two".to_vec())
        } else {
            (b"old-one".to_vec(), b"old-two".to_vec())
        };
        assert_eq!(values, expected, "wrong recovered set after {phase}");
        assert!(manager_directories(&root).is_empty());
    }
}

#[test]
fn recovery_rejects_a_same_size_stage_with_the_wrong_digest() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output = root.join("document.md");
    let mut stream = StreamingFileTransaction::begin(&output, false, &context()).unwrap();
    stream.write_all_checked(b"trusted").unwrap();
    let transaction = stream.seal().unwrap();
    let directory = transaction.directory.clone();
    fs::write(directory.join("stage-0"), b"forged!").unwrap();
    transaction.abandon_for_test();

    let error = recover_pending(&root).unwrap_err();
    assert_eq!(error.code(), "transactionRecoveryFailed");
    assert!(error.message().contains("digest"));
    assert!(!output.exists());
    assert!(directory.exists());

    fs::write(directory.join("stage-0"), b"trusted").unwrap();
    recover_pending(&root).unwrap();
    assert!(!output.exists());
    assert!(manager_artifacts(&root).is_empty());
}

#[test]
fn recovery_ignores_only_an_incomplete_journal_tail() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output = root.join("document.md");
    let mut stream = StreamingFileTransaction::begin(&output, false, &context()).unwrap();
    stream.write_all_checked(b"markdown").unwrap();
    let transaction = stream.seal().unwrap();
    let directory = transaction.directory.clone();
    transaction.abandon_for_test();
    OpenOptions::new()
        .append(true)
        .open(directory.join(JOURNAL_LOG_NAME))
        .unwrap()
        .write_all(&JOURNAL_RECORD_MAGIC[..4])
        .unwrap();

    recover_pending(&root).unwrap();
    assert!(!output.exists());
    assert!(manager_artifacts(&root).is_empty());
}

#[cfg(unix)]
#[test]

fn deep_parent_without_a_lease_never_uses_an_ancestor_scan_limit() {
    for depth in [130_usize, 500] {
        let temporary = tempfile::tempdir().unwrap();
        let mut parent = temporary.path().canonicalize().unwrap();
        let mut supported = true;
        for _ in 0..depth {
            parent.push("d");
            if let Err(error) = fs::create_dir(&parent) {
                assert!(
                    matches!(error.raw_os_error(), Some(libc::ENAMETOOLONG | libc::EINVAL)),
                    "unexpected deep directory failure: {error}"
                );
                supported = false;
                break;
            }
        }
        if !supported {
            continue;
        }
        let target = parent.join("document.md");
        let output = [Target { path: target.clone(), bytes: b"deep" }];
        prepare(&output, false, &context()).unwrap().commit().unwrap();
        assert_eq!(fs::read(target).unwrap(), b"deep");
    }
}

#[cfg(unix)]
#[test]
fn parent_swap_after_handle_authentication_never_writes_external_directory() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let parent = root.join("safe");
    let held = root.join("safe-held");
    let external = root.join("external");
    fs::create_dir(&parent).unwrap();
    fs::create_dir(&external).unwrap();
    let target = parent.join("document.md");
    fs::write(&target, b"old").unwrap();
    let targets = [Target { path: target.clone(), bytes: b"new" }];
    let mut transaction = prepare(&targets, true, &context()).unwrap();
    let error = transaction
        .commit_with_hook(|phase, _| {
            if phase == "afterTargetAuthentication" {
                fs::rename(&parent, &held)?;
                symlink(&external, &parent)?;
            }
            Ok(HookDecision::Continue)
        })
        .unwrap_err();
    assert_eq!(error.code(), "rollbackFailed");
    assert!(!external.join("document.md").exists());
    assert_eq!(fs::read(held.join("document.md")).unwrap(), b"old");
    assert_eq!(manager_directories(&held).len(), 1);

    fs::remove_file(&parent).unwrap();
    fs::rename(&held, &parent).unwrap();
    recover_pending(&parent).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"old");
    assert!(!external.join("document.md").exists());
    assert!(manager_directories(&root).is_empty());
}

#[test]
fn rollback_failure_preserves_backup_and_a_later_recovery_completes() {
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
    let mut transaction = prepare(&targets, true, &context()).unwrap();
    let directory = transaction.directory.clone();
    let error = transaction
        .commit_with_hook(|phase, index| {
            if phase == "targetInstalled" && index == 0 {
                fs::remove_file(&first)?;
                fs::create_dir(&first)?;
                fs::write(first.join("blocker"), b"do-not-delete")?;
                Err(CliError::new(ExitClass::Io, "injectedCommitFailure", "injected"))
            } else {
                Ok(HookDecision::Continue)
            }
        })
        .unwrap_err();
    assert_eq!(error.code(), "rollbackFailed");
    assert!(error.message().contains("injectedCommitFailure"));
    #[cfg(unix)]
    assert!(error.message().contains("outputTargetTypeDenied"));
    #[cfg(windows)]
    assert!(
        error.message().contains("one or more rollback operations failed"),
        "{}",
        error.message()
    );
    assert_eq!(fs::read(directory.join("backup-0")).unwrap(), b"old-one");
    assert!(directory.join("journal-a.json").exists());
    assert_eq!(fs::read(&second).unwrap(), b"old-two");

    fs::remove_dir_all(&first).unwrap();
    recover_pending(&root).unwrap();
    assert_eq!(fs::read(&first).unwrap(), b"old-one");
    assert_eq!(fs::read(&second).unwrap(), b"old-two");
    assert!(!directory.exists());
}

#[cfg(unix)]
#[test]
fn rollback_failure_keeps_the_backup_recoverable() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output_parent = root.join("locked");
    fs::create_dir(&output_parent).unwrap();
    let output = output_parent.join("document.md");
    fs::write(&output, b"old").unwrap();
    let targets = [Target { path: output.clone(), bytes: b"new" }];
    let mut transaction = prepare(&targets, true, &context()).unwrap();
    let directory = transaction.directory.clone();
    let error = transaction
        .commit_with_hook(|phase, index| {
            if phase == "targetInstalled" && index == 0 {
                Ok(HookDecision::SimulateRollbackFailure)
            } else {
                Ok(HookDecision::Continue)
            }
        })
        .unwrap_err();
    assert_eq!(error.code(), "rollbackFailed");
    assert!(error.message().contains("injectedPermissionFailure"));
    assert_eq!(fs::read(directory.join("backup-0")).unwrap(), b"old");

    recover_pending(&output_parent).unwrap();
    assert_eq!(fs::read(output).unwrap(), b"old");
    assert!(!directory.exists());
}

#[test]
fn late_non_regular_target_is_rejected_before_the_first_output_mutation() {
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
    let held_second = root.join("held-two.bin");
    let mut transaction = prepare(&targets, true, &context()).unwrap();
    let error = transaction
        .commit_with_hook(|phase, _| {
            if phase == "committing" {
                fs::rename(&second, &held_second)?;
                fs::create_dir(&second)?;
            }
            Ok(HookDecision::Continue)
        })
        .unwrap_err();
    assert_eq!(error.code(), "rollbackFailed");
    assert_eq!(fs::read(&first).unwrap(), b"old-one");
    assert!(second.is_dir());
    assert_eq!(manager_directories(&root).len(), 1);

    fs::remove_dir(&second).unwrap();
    fs::rename(&held_second, &second).unwrap();
    recover_pending(&root).unwrap();
    assert_eq!(fs::read(&first).unwrap(), b"old-one");
    assert_eq!(fs::read(&second).unwrap(), b"old-two");
}

#[test]
fn absent_target_race_is_never_replaced_by_commit_or_rollback() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let target = root.join("document.md");
    let targets = [Target { path: target.clone(), bytes: b"new" }];
    let mut transaction = prepare(&targets, false, &context()).unwrap();
    let error = transaction
        .commit_with_hook(|phase, index| {
            if phase == "beforeTarget" && index == 0 {
                fs::write(&target, b"racer")?;
            }
            Ok(HookDecision::Continue)
        })
        .unwrap_err();
    assert!(matches!(error.code(), "outputIdentityChanged" | "outputConflict"));
    assert_eq!(fs::read(&target).unwrap(), b"racer");
    assert!(manager_directories(&root).is_empty());
}

#[cfg(unix)]
#[test]
fn directories_symlinks_fifos_and_devices_are_never_overwrite_targets() {
    use std::os::unix::fs::symlink;
    use std::process::Command;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let directory = root.join("directory");
    fs::create_dir(&directory).unwrap();
    let link = root.join("link");
    symlink(&directory, &link).unwrap();
    let fifo = root.join("fifo");
    assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());

    for path in [directory, link, fifo, PathBuf::from("/dev/null")] {
        let target = [Target { path: path.clone(), bytes: b"new" }];
        let error = prepare(&target, true, &context()).err().expect("non-regular target");
        assert_eq!(error.code(), "outputTargetTypeDenied", "{}", path.display());
    }
    assert!(manager_directories(&root).is_empty());
}

#[cfg(unix)]
#[test]
fn symlink_swap_preserves_the_safe_old_primary_and_defers_unsafe_cleanup() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let primary = root.join("document.md");
    let asset_parent = root.join("assets");
    let held_parent = root.join("assets-held");
    let attacker = root.join("attacker");
    fs::write(&primary, b"old-document").unwrap();
    fs::create_dir(&asset_parent).unwrap();
    fs::create_dir(&attacker).unwrap();
    let asset = asset_parent.join("image.png");
    let targets = [
        Target { path: primary.clone(), bytes: b"new-document" },
        Target { path: asset.clone(), bytes: b"new-image" },
    ];
    let mut transaction = prepare(&targets, true, &context()).unwrap();
    let error = transaction
        .commit_with_hook(|phase, index| {
            if phase == "beforeTarget" && index == 1 {
                fs::rename(&asset_parent, &held_parent)?;
                symlink(&attacker, &asset_parent)?;
            }
            Ok(HookDecision::Continue)
        })
        .unwrap_err();
    assert_eq!(error.code(), "rollbackFailed");
    assert_eq!(fs::read(&primary).unwrap(), b"old-document");
    assert!(!attacker.join("image.png").exists());
    assert_eq!(manager_directories(&root).len(), 1);

    fs::remove_file(&asset_parent).unwrap();
    fs::rename(&held_parent, &asset_parent).unwrap();
    if let Err(error) = recover_pending(&root) {
        panic!("recovery failed: {error}");
    }
    assert_eq!(fs::read(&primary).unwrap(), b"old-document");
    assert!(!asset.exists());
    assert!(manager_directories(&root).is_empty());
}

#[cfg(unix)]
#[test]
fn malformed_manager_directory_is_preserved_and_rejected() {
    use std::os::unix::fs::PermissionsExt as _;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let nonce = "0123456789abcdef0123456789abcdef";
    let registry = root.join(REGISTRY_NAME);
    fs::create_dir(&registry).unwrap();
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o700)).unwrap();
    let managed = registry.join(format!("{TRANSACTION_PREFIX}{nonce}"));
    fs::create_dir(&managed).unwrap();
    fs::write(managed.join("journal-a.json"), b"not-json").unwrap();
    let error = recover_pending(&root).unwrap_err();
    assert_eq!(error.code(), "transactionRecoveryFailed");
    assert!(managed.exists());
    let unrelated = root.join(".into-md-txn-01-not-managed");
    fs::create_dir(&unrelated).unwrap();
    assert!(unrelated.exists());
}

#[test]
fn active_transaction_is_locked_and_unexpected_members_block_recovery() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let output = root.join("document.md");
    fs::write(&output, b"old").unwrap();
    let targets = [Target { path: output.clone(), bytes: b"new" }];
    let transaction = prepare(&targets, true, &context()).unwrap();
    let directory = transaction.directory.clone();

    recover_pending(&root).unwrap();
    assert_eq!(fs::read(&output).unwrap(), b"old");
    assert!(directory.exists());
    transaction.abort().unwrap();

    let transaction = prepare(&targets, true, &context()).unwrap();
    let directory = transaction.directory.clone();
    transaction.abandon_for_test();
    fs::write(directory.join("not-in-journal"), b"untrusted").unwrap();
    let error = recover_pending(&root).unwrap_err();
    assert_eq!(error.code(), "transactionRecoveryFailed");
    assert!(directory.join("not-in-journal").exists());
    assert_eq!(fs::read(&output).unwrap(), b"old");
}

#[cfg(unix)]
#[test]
fn manager_symlink_is_rejected_without_touching_its_destination() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let destination = root.join("destination");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("keep"), b"keep").unwrap();
    let registry = root.join(REGISTRY_NAME);
    fs::create_dir(&registry).unwrap();
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o700)).unwrap();
    let manager =
        registry.join(format!("{TRANSACTION_PREFIX}{}", "0123456789abcdef0123456789abcdef"));
    symlink(&destination, &manager).unwrap();
    let error = recover_pending(&root).unwrap_err();
    assert_eq!(error.code(), "transactionRecoveryFailed");
    assert_eq!(fs::read(destination.join("keep")).unwrap(), b"keep");
    assert!(fs::symlink_metadata(manager).unwrap().file_type().is_symlink());
}

#[cfg(target_os = "linux")]
#[test]
fn cross_filesystem_set_is_rejected_before_transaction_allocation() {
    use std::os::unix::fs::MetadataExt as _;

    if !Path::new("/dev/shm").is_dir()
        || fs::metadata("/dev/shm").unwrap().dev() == fs::metadata("/tmp").unwrap().dev()
    {
        return;
    }
    let first_root = tempfile::tempdir_in("/tmp").unwrap();
    let second_root = tempfile::tempdir_in("/dev/shm").unwrap();
    let first = first_root.path().join("document.md");
    let second = second_root.path().join("asset.bin");
    let targets = [
        Target { path: first.clone(), bytes: b"document" },
        Target { path: second.clone(), bytes: b"asset" },
    ];
    let error = prepare(&targets, true, &context()).err().expect("cross-filesystem rejection");
    assert_eq!(error.code(), "crossFilesystemTransaction");
    assert!(!first.exists());
    assert!(!second.exists());
}

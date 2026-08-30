use super::process_helper::{continue_process, spawn_process_helper, wait_for_process_signal};
use super::*;

fn assert_private_registry_lock_namespace(root: &Path) {
    let root = SafeDir::open_absolute(root).unwrap();
    let lock_directory =
        root.open_child(OsStr::new(super::super::registry::REGISTRY_LOCK_DIRECTORY_NAME)).unwrap();
    lock_directory.verify_private_namespace().unwrap();
    assert_eq!(
        lock_directory.names_private().unwrap(),
        [OsString::from(super::super::registry::REGISTRY_LOCK_NAME)]
    );
    lock_directory
        .open_regular_private(OsStr::new(super::super::registry::REGISTRY_LOCK_NAME))
        .unwrap();
}

#[test]
fn stale_registry_epoch_waiter_reopens_after_atomic_cleanup() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let root_handle = SafeDir::open_absolute(&root).unwrap();
    let epoch = lock_registry_epoch(&root_handle, true).unwrap().unwrap();

    let mut child = spawn_process_helper("registry-waiter", &root, None);
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    wait_for_process_signal(&mut output, "READY");
    assert!(epoch.try_cleanup().unwrap());
    continue_process(&mut child);
    wait_for_process_signal(&mut output, "ACQUIRED");
    let status = child.wait().unwrap();
    assert!(status.success(), "registry waiter failed: {status}");
    assert!(!root.join(REGISTRY_NAME).exists());
    assert!(fs::read_dir(&root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(super::super::registry::REGISTRY_TOMBSTONE_PREFIX)
    }));
    assert_private_registry_lock_namespace(&root);
}

#[test]
fn fixed_parent_lease_serializes_processes_and_recovers_after_kill() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let target = root.join("same-parent.md");
    fs::write(&target, b"old").unwrap();

    let mut owner = spawn_process_helper("prepared-owner", &root, Some(&target));
    let mut owner_output = std::io::BufReader::new(owner.stdout.take().unwrap());
    wait_for_process_signal(&mut owner_output, "READY");

    let root_handle = SafeDir::open_absolute(&root).unwrap();
    let epoch = lock_registry_epoch(&root_handle, false).unwrap().unwrap();
    assert!(!epoch.try_cleanup().unwrap(), "an active child transaction lost its registry epoch");

    let mut contender = spawn_process_helper("expect-busy", &root, Some(&target));
    let mut contender_output = std::io::BufReader::new(contender.stdout.take().unwrap());
    wait_for_process_signal(&mut contender_output, "BUSY");
    assert!(contender.wait().unwrap().success());

    owner.kill().unwrap();
    let _ = owner.wait().unwrap();
    let replacement = [Target { path: target.clone(), bytes: b"replacement" }];
    match prepare(&replacement, true, &context()) {
        Ok(transaction) => {
            transaction.commit().unwrap();
        }
        Err(error) => {
            assert_eq!(error.code(), "transactionRecoveredRetry");
            prepare(&replacement, true, &context()).unwrap().commit().unwrap();
        }
    }
    assert_eq!(fs::read(target).unwrap(), b"replacement");
    assert!(manager_artifacts(&root).is_empty());
}

#[test]
fn process_killed_after_a_durable_target_install_recovers_atomically() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let first = root.join("commit-first.md");
    let second = root.join("commit-second.md");
    fs::write(&first, b"old-first").unwrap();
    fs::write(&second, b"old-second").unwrap();

    let mut owner = spawn_process_helper("commit-target-installed", &root, None);
    let mut owner_output = std::io::BufReader::new(owner.stdout.take().unwrap());
    wait_for_process_signal(&mut owner_output, "READY");
    owner.kill().unwrap();
    let status = owner.wait().unwrap();
    assert!(!status.success(), "killed commit helper unexpectedly succeeded");

    recover_pending(&root).unwrap();
    let values = (fs::read(&first).unwrap(), fs::read(&second).unwrap());
    assert!(
        values == (b"old-first".to_vec(), b"old-second".to_vec())
            || values == (b"new-first".to_vec(), b"new-second".to_vec()),
        "recovery exposed a partially committed output set: {values:?}"
    );
    assert!(manager_artifacts(&root).is_empty());
    assert!(manager_directories(&root).is_empty());
    assert_private_registry_lock_namespace(&root);
}

#[test]
fn distinct_parent_leases_allow_two_processes_to_prepare_together() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let first_parent = root.join("first");
    let second_parent = root.join("second");
    fs::create_dir(&first_parent).unwrap();
    fs::create_dir(&second_parent).unwrap();
    let first_target = first_parent.join("output.md");
    let second_target = second_parent.join("output.md");

    let mut first = spawn_process_helper("prepared-owner", &root, Some(&first_target));
    let mut first_output = std::io::BufReader::new(first.stdout.take().unwrap());
    wait_for_process_signal(&mut first_output, "READY");
    let mut second = spawn_process_helper("prepared-owner", &root, Some(&second_target));
    let mut second_output = std::io::BufReader::new(second.stdout.take().unwrap());
    wait_for_process_signal(&mut second_output, "READY");

    continue_process(&mut first);
    continue_process(&mut second);
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    assert_eq!(fs::read(first_target).unwrap(), b"child");
    assert_eq!(fs::read(second_target).unwrap(), b"child");
    assert!(manager_artifacts(&root).is_empty());
}

#[test]
fn registry_cleanup_refuses_an_active_transaction_member() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let root_handle = SafeDir::open_absolute(&root).unwrap();
    let epoch = lock_registry_epoch(&root_handle, true).unwrap().unwrap();
    let member = OsStr::new("active-member");
    let file = epoch.registry().create_regular_private(member).unwrap();
    file.sync_all().unwrap();
    epoch.registry().sync().unwrap();
    drop(file);
    assert!(!epoch.try_cleanup().unwrap());
    assert!(root.join(REGISTRY_NAME).join(member).exists());

    let epoch = lock_registry_epoch(&root_handle, false).unwrap().unwrap();
    epoch.registry().remove_regular_private(member).unwrap();
    assert!(epoch.try_cleanup().unwrap());
    assert!(!root.join(REGISTRY_NAME).exists());
    assert_private_registry_lock_namespace(&root);
}

#[cfg(unix)]
#[test]
fn overlapping_cross_directory_transaction_is_recovered_before_any_new_write() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let a = root.join("a");
    let b = root.join("b");
    fs::create_dir_all(a.join("child")).unwrap();
    fs::create_dir_all(&b).unwrap();
    let first = a.join("child/one.md");
    let second = b.join("two.bin");
    let parent_level = a.join("parent.txt");
    fs::write(&first, b"old-one").unwrap();
    fs::write(&second, b"old-two").unwrap();
    fs::write(&parent_level, b"old-parent").unwrap();
    let targets = [
        Target { path: first.clone(), bytes: b"new-one" },
        Target { path: second.clone(), bytes: b"new-two" },
        Target { path: parent_level.clone(), bytes: b"new-parent" },
    ];

    for requested in [&first, &second, &parent_level] {
        fs::write(&first, b"old-one").unwrap();
        fs::write(&second, b"old-two").unwrap();
        fs::write(&parent_level, b"old-parent").unwrap();
        let mut transaction = prepare(&targets, true, &context()).unwrap();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "targetInstalled" && index == 0 {
                    Ok(HookDecision::SimulateCrash)
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .unwrap_err();
        assert_eq!(error.code(), "simulatedCrash");
        drop(transaction);

        let third = [Target { path: requested.clone(), bytes: b"third" }];
        prepare(&third, true, &context()).unwrap().commit().unwrap();
        assert_eq!(fs::read(requested).unwrap(), b"third");
        for untouched in [&first, &second, &parent_level] {
            if untouched != requested {
                let expected = if untouched == &first {
                    b"old-one".as_slice()
                } else if untouched == &second {
                    b"old-two".as_slice()
                } else {
                    b"old-parent".as_slice()
                };
                assert_eq!(fs::read(untouched).unwrap(), expected);
            }
        }
        assert!(manager_directories(&root).is_empty());
    }
}

#[cfg(unix)]
#[test]
fn physical_parent_lease_serializes_an_absent_different_basename() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let first_path = root.join("first.md");
    let second_path = root.join("second.md");
    fs::write(&first_path, b"old").unwrap();
    let first = [Target { path: first_path.clone(), bytes: b"interrupted" }];
    let mut transaction = prepare(&first, true, &context()).unwrap();

    let active = [Target { path: second_path.clone(), bytes: b"blocked" }];
    let error = prepare(&active, false, &context()).err().expect("physical parent is leased");
    assert_eq!(error.code(), "transactionBusy");

    let error = transaction
        .commit_with_hook(|phase, index| {
            if phase == "targetInstalled" && index == 0 {
                Ok(HookDecision::SimulateCrash)
            } else {
                Ok(HookDecision::Continue)
            }
        })
        .unwrap_err();
    assert_eq!(error.code(), "simulatedCrash");
    drop(transaction);

    let second = [Target { path: second_path.clone(), bytes: b"final" }];
    prepare(&second, false, &context()).unwrap().commit().unwrap();
    assert_eq!(fs::read(first_path).unwrap(), b"old");
    assert_eq!(fs::read(second_path).unwrap(), b"final");
    assert!(manager_directories(&root).is_empty());
}

#[test]
fn registry_cleanup_waits_for_a_registered_preparation_window() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let first_path = root.join("first.md");
    let second_path = root.join("second.md");
    let first = [Target { path: first_path.clone(), bytes: b"first" }];
    let first_transaction = prepare(&first, false, &context()).unwrap();
    let (registered_tx, registered_rx) = std::sync::mpsc::channel();
    let (continue_tx, continue_rx) = std::sync::mpsc::channel();

    let second_thread = std::thread::spawn(move || {
        let second = [Target { path: second_path.clone(), bytes: b"second" }];
        let mut announced = false;
        let transaction = prepare_with_hook(&second, false, &context(), |phase, _| {
            if phase == "preparingRootRegistered" && !announced {
                announced = true;
                registered_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
            }
            Ok(HookDecision::Continue)
        })?;
        transaction.commit()?;
        Ok::<_, CliError>(second_path)
    });

    registered_rx.recv().unwrap();
    first_transaction.commit().unwrap();
    assert_eq!(fs::read(&first_path).unwrap(), b"first");
    continue_tx.send(()).unwrap();
    let second_path = second_thread.join().unwrap().unwrap();
    assert_eq!(fs::read(second_path).unwrap(), b"second");
    assert!(manager_directories(&root).is_empty());
}

#[test]
fn native_streaming_targets_in_one_parent_are_serialized_by_the_parent_lease() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let first_path = root.join("native-first.md");
    let second_path = root.join("native-second.md");
    let mut first = StreamingFileTransaction::begin(&first_path, false, &context()).unwrap();
    first.write_all_checked(b"first-stream").unwrap();
    let error = StreamingFileTransaction::begin(&second_path, false, &context())
        .err()
        .expect("same-parent streaming transaction must be serialized");
    assert_eq!(error.code(), "transactionBusy");

    first.seal().unwrap().commit().unwrap();
    let mut second = StreamingFileTransaction::begin(&second_path, false, &context()).unwrap();
    second.write_all_checked(b"second-stream").unwrap();
    second.seal().unwrap().commit().unwrap();

    assert_eq!(fs::read(first_path).unwrap(), b"first-stream");
    assert_eq!(fs::read(second_path).unwrap(), b"second-stream");
    assert!(manager_directories(&root).is_empty());
}

#[test]
fn concurrent_same_target_rejects_the_contender_without_rolling_back_the_owner() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let target = root.join("same.md");
    let first = [Target { path: target.clone(), bytes: b"first" }];
    let second = [Target { path: target.clone(), bytes: b"second" }];
    let first_transaction = prepare(&first, false, &context()).unwrap();
    let error = prepare(&second, false, &context())
        .err()
        .expect("same-target contender must be serialized");
    assert_eq!(error.code(), "transactionBusy");
    first_transaction.commit().unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"first");
    assert!(manager_directories(&root).is_empty());
}

#[test]
fn forged_parent_lease_namespace_fails_closed() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    fs::write(root.join(PARENT_LEASE_NAME), b"forged").unwrap();
    let target = [Target { path: root.join("document.md"), bytes: b"content" }];
    let error = prepare(&target, false, &context()).err().expect("forged lease rejection");
    assert_eq!(error.code(), "transactionRecoveryUnsafe");
    assert!(!root.join("document.md").exists());
}

#[cfg(unix)]
#[test]
fn hardlink_alias_observes_the_same_physical_parent_lease() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let original = root.join("original.md");
    let alias = root.join("alias.md");
    fs::write(&original, b"old").unwrap();
    fs::hard_link(&original, &alias).unwrap();
    let first = [Target { path: original.clone(), bytes: b"first" }];
    let mut transaction = prepare(&first, true, &context()).unwrap();
    let error = transaction
        .commit_with_hook(|phase, index| {
            if phase == "targetInstalled" && index == 0 {
                Ok(HookDecision::SimulateCrash)
            } else {
                Ok(HookDecision::Continue)
            }
        })
        .unwrap_err();
    assert_eq!(error.code(), "simulatedCrash");
    drop(transaction);

    let second = [Target { path: alias.clone(), bytes: b"second" }];
    prepare(&second, true, &context()).unwrap().commit().unwrap();
    assert_eq!(fs::read(&original).unwrap(), b"old");
    assert_eq!(fs::read(&alias).unwrap(), b"second");
    assert!(manager_directories(&root).is_empty());
}

#[cfg(target_vendor = "apple")]
#[test]
fn case_and_unicode_parent_aliases_share_the_physical_lease() {
    for (created_name, alias_name) in [("CaseParent", "caseparent"), ("é", "e\u{301}")] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let created = root.join(created_name);
        fs::create_dir(&created).unwrap();
        let alias = root.join(alias_name);
        let (Ok(created_handle), Ok(alias_handle)) =
            (SafeDir::open_absolute(&created), SafeDir::open_absolute(&alias))
        else {
            continue;
        };
        if created_handle.identity != alias_handle.identity {
            continue;
        }
        let first_path = created.join("first.md");
        let alias_path = alias.join("second.md");
        fs::write(&first_path, b"old").unwrap();
        let first = [Target { path: first_path.clone(), bytes: b"interrupted" }];
        let mut transaction = prepare(&first, true, &context()).unwrap();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "targetInstalled" && index == 0 {
                    Ok(HookDecision::SimulateCrash)
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .unwrap_err();
        assert_eq!(error.code(), "simulatedCrash");
        drop(transaction);

        let second = [Target { path: alias_path.clone(), bytes: b"final" }];
        prepare(&second, false, &context()).unwrap().commit().unwrap();
        assert_eq!(fs::read(first_path).unwrap(), b"old");
        assert_eq!(fs::read(alias_path).unwrap(), b"final");
        assert!(manager_directories(&created).is_empty());
    }
}

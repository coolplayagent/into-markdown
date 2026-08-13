use super::*;
use std::fs;
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;

use into_markdown_converters::{ContentFormatDetector, MemorySourceResolver, TextConverter};
use into_markdown_core::{
    Asset, BoxFuture, ConversionError, ConversionOptions, ConversionRequest, Document,
    ExecutionContext, InputRef, MarkdownRenderer,
};
use into_markdown_engine::{Engine, EngineBuilder};
use into_markdown_render_markdown::GfmRenderer;

fn input(token_byte: char) -> InputReference {
    InputReference {
        schema_version: 1,
        input_fingerprint: "a".repeat(64),
        options_fingerprint: "c".repeat(64),
        byte_len: 42,
        recovery_token: token_byte.to_string().repeat(32),
    }
}

fn private_temp() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn store() -> (tempfile::TempDir, TaskStore) {
    let directory = private_temp();
    let store = TaskStore::open(directory.path(), BusyControl::default()).unwrap();
    (directory, store)
}

fn create(store: &mut TaskStore) -> TaskRecord {
    let mut task_input = input('b');
    random_id().unwrap().as_str().clone_into(&mut task_input.recovery_token);
    store
        .create(NewTask { input: task_input, configuration: ConfigurationSnapshot::default() })
        .unwrap()
}

fn transition(expected: TaskStatus, next: TaskStatus, progress: u32) -> TaskTransition {
    TaskTransition {
        expected,
        next,
        progress_millionths: progress,
        diagnostics: Vec::new(),
        artifacts: Vec::new(),
    }
}

fn succeeded_transition() -> TaskTransition {
    let artifacts = [
        ArtifactKind::Markdown,
        ArtifactKind::DocumentIr,
        ArtifactKind::Diagnostics,
        ArtifactKind::Bundle,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, kind)| ArtifactReference {
        storage_key: format!("{index:032x}"),
        kind,
        byte_len: 1,
        sha256: format!("{index:064x}"),
        asset_id: None,
        filename: None,
        media_type: None,
    })
    .collect();
    TaskTransition {
        expected: TaskStatus::Converted,
        next: TaskStatus::Succeeded,
        progress_millionths: 1_000_000,
        diagnostics: vec![],
        artifacts,
    }
}

struct FailingRenderer;

impl MarkdownRenderer for FailingRenderer {
    fn id(&self) -> &'static str {
        "task-store-failing-renderer"
    }

    fn planned_markdown_bytes(
        &self,
        _: &Document,
        _: &[Asset],
        _: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        Ok(context.available_memory_bytes())
    }
    fn render<'a>(
        &'a self,
        _document: &'a Document,
        _assets: &'a [Asset],
        _options: &'a ConversionOptions,
        _context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<String, ConversionError>> {
        Box::pin(async {
            Err(ConversionError::Internal { detail: "intentional render fixture".into() })
        })
    }
}

fn recovery_engine(renderer: Arc<dyn MarkdownRenderer>) -> Engine {
    let mut builder = EngineBuilder::new().renderer(renderer);
    builder
        .registry_mut()
        .register_source_resolver(Arc::new(MemorySourceResolver))
        .register_format_detector(Arc::new(ContentFormatDetector))
        .register_converter(Arc::new(TextConverter));
    builder.build().unwrap()
}

#[test]
fn create_get_list_pin_and_legal_state_machine_are_atomic() {
    let (_directory, mut store) = store();
    let first = create(&mut store);
    let second = create(&mut store);
    assert!(second.updated_at_ms >= first.updated_at_ms);
    store.set_pinned(&first.id, true).unwrap();
    let running = store
        .transition(&first.id, transition(TaskStatus::Pending, TaskStatus::Running, 100_000))
        .unwrap();
    assert!(running.pinned);
    let converted = store
        .transition(
            &first.id,
            TaskTransition {
                expected: TaskStatus::Running,
                next: TaskStatus::Converted,
                progress_millionths: 900_000,
                diagnostics: vec![],
                artifacts: vec![],
            },
        )
        .unwrap();
    assert!(converted.artifacts.is_empty());
    store.transition(&first.id, succeeded_transition()).unwrap();
    assert!(matches!(
        store.transition(
            &first.id,
            transition(TaskStatus::Succeeded, TaskStatus::Failed, 1_000_000)
        ),
        Err(TaskStoreError::Conflict(_))
    ));
    let page = store.list(1, None).unwrap();
    assert_eq!(page.len(), 1);
    let cursor = TaskCursor { updated_at_ms: page[0].updated_at_ms, id: page[0].id.clone() };
    assert_eq!(store.list(100, Some(&cursor)).unwrap().len(), 1);
}

#[test]
fn illegal_regression_stale_cas_and_invalid_progress_fail_closed() {
    let (_directory, mut write_store) = store();
    let task = create(&mut write_store);
    assert!(matches!(
        write_store.transition(&task.id, transition(TaskStatus::Pending, TaskStatus::Converted, 1)),
        Err(TaskStoreError::Conflict(_))
    ));
    write_store
        .transition(&task.id, transition(TaskStatus::Pending, TaskStatus::Running, 50))
        .unwrap();
    assert!(matches!(
        write_store.transition(&task.id, transition(TaskStatus::Pending, TaskStatus::Running, 60)),
        Err(TaskStoreError::Conflict(_))
    ));
    assert!(matches!(
        write_store
            .transition(&task.id, transition(TaskStatus::Running, TaskStatus::Converted, 49)),
        Err(TaskStoreError::Conflict(_))
    ));
    assert!(matches!(
        write_store.transition(
            &task.id,
            transition(TaskStatus::Running, TaskStatus::Converted, 1_000_001)
        ),
        Err(TaskStoreError::Limit(_))
    ));
}

#[test]
fn schema_migration_is_idempotent_and_newer_versions_are_rejected() {
    let directory = private_temp();
    drop(TaskStore::open(directory.path(), BusyControl::default()).unwrap());
    drop(TaskStore::open(directory.path(), BusyControl::default()).unwrap());
    let connection = Connection::open(directory.path().join(DATABASE_FILE)).unwrap();
    connection.pragma_update(None, "user_version", 99).unwrap();
    drop(connection);
    assert!(matches!(
        TaskStore::open(directory.path(), BusyControl::default()),
        Err(TaskStoreError::UnsupportedVersion { found: 99, supported: 3 })
    ));
}

fn legacy_asset_fixture(version: i64) -> (tempfile::TempDir, TaskId) {
    let (directory, mut store) = store();
    let task = create(&mut store);
    store.transition(&task.id, transition(TaskStatus::Pending, TaskStatus::Running, 1)).unwrap();
    store
        .transition(&task.id, transition(TaskStatus::Running, TaskStatus::Converted, 900_000))
        .unwrap();
    let mut completed = succeeded_transition();
    completed.artifacts.push(ArtifactReference {
        storage_key: "f".repeat(32),
        kind: ArtifactKind::Asset,
        byte_len: 7,
        sha256: "e".repeat(64),
        asset_id: Some("legacy-id".into()),
        filename: Some("legacy.bin".into()),
        media_type: Some("application/octet-stream".into()),
    });
    store.transition(&task.id, completed).unwrap();
    store
        .connection
        .execute(
            "UPDATE artifacts SET asset_id=NULL, filename=NULL, media_type=NULL WHERE kind='asset'",
            [],
        )
        .unwrap();
    store
        .connection
        .execute_batch(&format!(
            "DROP TRIGGER artifacts_limit; DROP TRIGGER artifacts_terminal;\
                 ALTER TABLE artifacts RENAME TO artifacts_v3;\
                 CREATE TABLE artifacts(\
                   task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,\
                   storage_key TEXT NOT NULL, kind TEXT NOT NULL, byte_len INTEGER NOT NULL,\
                   sha256 TEXT NOT NULL, PRIMARY KEY(task_id, storage_key)) STRICT;\
                 INSERT INTO artifacts SELECT task_id,storage_key,kind,byte_len,sha256 FROM artifacts_v3;\
                 DROP TABLE artifacts_v3;\
                 CREATE TRIGGER artifacts_limit BEFORE INSERT ON artifacts WHEN (SELECT count(*) FROM artifacts WHERE task_id=NEW.task_id)>=128 BEGIN SELECT RAISE(ABORT, 'artifact limit'); END;\
                 CREATE TRIGGER artifacts_terminal BEFORE INSERT ON artifacts WHEN (SELECT status FROM tasks WHERE id=NEW.task_id) IN ('failed','interrupted','cancelled') BEGIN SELECT RAISE(ABORT, 'terminal artifact'); END;\
                 PRAGMA user_version={version};"
        ))
        .unwrap();
    drop(store);
    (directory, task.id)
}

#[test]
fn legacy_v1_and_v2_assets_migrate_without_fabricating_metadata() {
    for version in [1, 2] {
        let (directory, id) = legacy_asset_fixture(version);
        let store = TaskStore::open(directory.path(), BusyControl::default()).unwrap();
        let record = store.get(&id).unwrap().unwrap();
        assert_eq!(record.status, TaskStatus::Succeeded);
        let asset =
            record.artifacts.iter().find(|artifact| artifact.kind == ArtifactKind::Asset).unwrap();
        assert_eq!(asset.byte_len, 7);
        assert_eq!(asset.sha256, "e".repeat(64));
        assert_eq!(
            (asset.asset_id.as_ref(), asset.filename.as_ref(), asset.media_type.as_ref()),
            (None, None, None)
        );
    }
}

#[test]
fn v3_assets_require_complete_metadata_on_write_and_load() {
    let (_directory, mut write_store) = store();
    let task = create(&mut write_store);
    let partial = ArtifactReference {
        storage_key: "d".repeat(32),
        kind: ArtifactKind::Asset,
        byte_len: 1,
        sha256: "e".repeat(64),
        asset_id: Some("asset".into()),
        filename: None,
        media_type: Some("application/octet-stream".into()),
    };
    assert!(matches!(
        write_store.transition(
            &task.id,
            TaskTransition {
                expected: TaskStatus::Pending,
                next: TaskStatus::Running,
                progress_millionths: 1,
                diagnostics: Vec::new(),
                artifacts: vec![partial],
            },
        ),
        Err(TaskStoreError::Limit(_))
    ));

    let (_directory, mut corrupt_store) = store();
    let task = create(&mut corrupt_store);
    corrupt_store
        .connection
        .execute(
            "INSERT INTO artifacts(task_id,storage_key,kind,byte_len,sha256,asset_id) VALUES(?1,?2,'asset',1,?3,'partial')",
            params![task.id.as_str(), "d".repeat(32), "e".repeat(64)],
        )
        .unwrap();
    assert!(matches!(corrupt_store.get(&task.id), Err(TaskStoreError::Limit(_))));
}

#[test]
fn v2_migration_abort_at_every_statement_is_atomic_and_reopenable() {
    const ROOT: &str = "INTO_MD_TASK_STORE_V2_MIGRATION_ROOT";
    const PHASE: &str = "INTO_MD_TASK_STORE_V2_MIGRATION_ABORT_PHASE";
    if let Ok(root) = std::env::var(ROOT) {
        let _ = TaskStore::open(root, BusyControl::default());
        unreachable!();
    }
    for phase in 1..=4 {
        let (directory, id) = legacy_asset_fixture(2);
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::v2_migration_abort_at_every_statement_is_atomic_and_reopenable",
            ])
            .env(ROOT, directory.path())
            .env(PHASE, phase.to_string())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let store = TaskStore::open(directory.path(), BusyControl::default()).unwrap();
        assert_eq!(store.get(&id).unwrap().unwrap().status, TaskStatus::Succeeded);
        let version: i64 =
            store.connection.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}

#[test]
fn corrupt_database_and_unknown_persisted_enum_are_diagnosed() {
    let directory = private_temp();
    fs::write(directory.path().join(DATABASE_FILE), b"not sqlite").unwrap();
    assert!(matches!(
        TaskStore::open(directory.path(), BusyControl::default()),
        Err(TaskStoreError::Corrupt(_) | TaskStoreError::Io(_) | TaskStoreError::UnsafePath(_))
    ));

    let (_directory, mut store) = store();
    let task = create(&mut store);
    store.connection.execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
    store
        .connection
        .execute("UPDATE tasks SET status='future' WHERE id=?1", [task.id.as_str()])
        .unwrap();
    assert!(matches!(store.get(&task.id), Err(TaskStoreError::Corrupt(_))));
}

#[test]
fn persisted_succeeded_progress_invariant_fails_closed() {
    let (_directory, mut store) = store();
    let task = create(&mut store);
    store.connection.execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
    store
        .connection
        .execute("UPDATE tasks SET status='succeeded', progress=0 WHERE id=?1", [task.id.as_str()])
        .unwrap();
    assert!(matches!(store.get(&task.id), Err(TaskStoreError::Corrupt(_))));
}

#[test]
fn secret_fields_are_rejected_and_canary_never_reaches_database_or_wal() {
    let (directory, mut store) = store();
    let canary = "CANARY_PROVIDER_SECRET_019ff8fd";
    let json = format!(
        r#"{{"schemaVersion":1,"outputFormat":"markdown","ocrEnabled":false,"preserveLayout":false,"apiKey":"{canary}"}}"#
    );
    assert!(serde_json::from_str::<ConfigurationSnapshot>(&json).is_err());
    create(&mut store);
    store.connection.execute_batch("PRAGMA wal_checkpoint(PASSIVE)").unwrap();
    for name in [DATABASE_FILE, "tasks.sqlite3-wal", "tasks.sqlite3-shm"] {
        let path = directory.path().join(name);
        if let Ok(bytes) = fs::read(path) {
            assert!(!bytes.windows(canary.len()).any(|window| window == canary.as_bytes()));
        }
    }
}

#[test]
fn wal_reader_observes_only_committed_writer_state() {
    let directory = private_temp();
    let mut writer = TaskStore::open(directory.path(), BusyControl::default()).unwrap();
    let task = create(&mut writer);
    let reader = TaskStore::open(directory.path(), BusyControl::default()).unwrap();
    writer.connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    writer.connection.execute("UPDATE tasks SET pinned=1 WHERE id=?1", [task.id.as_str()]).unwrap();
    assert!(!reader.get(&task.id).unwrap().unwrap().pinned);
    writer.connection.execute_batch("COMMIT").unwrap();
    assert!(reader.get(&task.id).unwrap().unwrap().pinned);
}

#[test]
fn concurrent_compare_and_set_has_one_winner() {
    let directory = private_temp();
    let mut initial = TaskStore::open(directory.path(), BusyControl::default()).unwrap();
    let task = create(&mut initial);
    drop(initial);
    let root = directory.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        let id = task.id.clone();
        handles.push(thread::spawn(move || {
            let mut store = TaskStore::open(root, BusyControl::default()).unwrap();
            barrier.wait();
            store.transition(&id, transition(TaskStatus::Pending, TaskStatus::Running, 1)).is_ok()
        }));
    }
    barrier.wait();
    let winners =
        handles.into_iter().map(|handle| handle.join().unwrap()).filter(|won| *won).count();
    assert_eq!(winners, 1);
}

#[test]
fn lock_wait_has_deadline_and_reports_pre_cancelled_wait() {
    let directory = private_temp();
    let mut holder = TaskStore::open(directory.path(), BusyControl::default()).unwrap();
    let task = create(&mut holder);
    holder.connection.execute_batch("BEGIN IMMEDIATE").unwrap();

    let busy = BusyControl::new(Duration::from_millis(40)).unwrap();
    let mut contender = TaskStore::open(directory.path(), busy.clone()).unwrap();
    let start = Instant::now();
    assert!(matches!(contender.set_pinned(&task.id, true), Err(TaskStoreError::BusyTimeout)));
    assert!(start.elapsed() < Duration::from_secs(1));
    busy.cancel();
    assert!(matches!(contender.set_pinned(&task.id, true), Err(TaskStoreError::Cancelled)));
    busy.reset();
    let cancellable = BusyControl::new(Duration::from_secs(5)).unwrap();
    let cancel_handle = cancellable.clone();
    let mut contender = TaskStore::open(directory.path(), cancellable).unwrap();
    let id = task.id.clone();
    let started = Instant::now();
    let waiting = thread::spawn(move || contender.set_pinned(&id, true));
    thread::sleep(Duration::from_millis(30));
    cancel_handle.cancel();
    assert!(matches!(waiting.join().unwrap(), Err(TaskStoreError::Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(1));
    holder.connection.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn backup_lock_wait_uses_the_operation_deadline() {
    let directory = private_temp();
    let busy = BusyControl::new(Duration::from_millis(40)).unwrap();
    let mut store = TaskStore::open(directory.path(), busy).unwrap();
    create(&mut store);
    store.connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = Instant::now();
    let result = store.backup(&TaskId::parse("5".repeat(32)).unwrap());
    assert!(matches!(result, Err(TaskStoreError::BusyTimeout)));
    assert!(started.elapsed() >= Duration::from_millis(30));
    assert!(started.elapsed() < Duration::from_secs(1));
    store.connection.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn backup_is_consistent_standalone_private_and_no_replace() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_directory, mut store) = store();
    let task = create(&mut store);
    let backup_id = TaskId::parse("e".repeat(32)).unwrap();
    let path = store.backup(&backup_id).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    assert!(
        !path
            .with_file_name(format!("{}-wal", path.file_name().unwrap().to_string_lossy()))
            .exists()
    );
    let backup = Connection::open(&path).unwrap();
    let count: i64 = backup.query_row("SELECT count(*) FROM tasks", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 1);
    let status: String = backup
        .query_row("SELECT status FROM tasks WHERE id=?1", [task.id.as_str()], |row| row.get(0))
        .unwrap();
    assert_eq!(status, "pending");
    assert!(matches!(store.backup(&backup_id), Err(TaskStoreError::Conflict(_))));
    assert!(
        fs::read_dir(path.parent().unwrap()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".backup-"))
    );
}

#[test]
fn configured_sqlite_safety_pragmas_are_effective() {
    let (_directory, store) = store();
    let integer = |name: &str| -> i64 {
        store.connection.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0)).unwrap()
    };
    assert_eq!(integer("foreign_keys"), 1);
    assert_eq!(integer("synchronous"), 2);
    assert_eq!(integer("trusted_schema"), 0);
    assert_eq!(integer("secure_delete"), 1);
    assert_eq!(integer("temp_store"), 2);
    assert_eq!(integer("page_size"), PAGE_SIZE);
    assert_eq!(integer("max_page_count"), MAX_DATABASE_BYTES / PAGE_SIZE);
}

#[test]
fn terminal_failures_reject_artifact_publication() {
    let (_directory, mut store) = store();
    let task = create(&mut store);
    let artifact = ArtifactReference {
        storage_key: "8".repeat(32),
        kind: ArtifactKind::Markdown,
        byte_len: 1,
        sha256: "7".repeat(64),
        asset_id: None,
        filename: None,
        media_type: None,
    };
    assert!(matches!(
        store.transition(
            &task.id,
            TaskTransition {
                expected: TaskStatus::Pending,
                next: TaskStatus::Failed,
                progress_millionths: 0,
                diagnostics: vec![],
                artifacts: vec![artifact],
            }
        ),
        Err(TaskStoreError::Conflict(_))
    ));
    assert_eq!(store.get(&task.id).unwrap().unwrap().status, TaskStatus::Pending);
}

#[test]
fn backup_static_symlink_destination_is_no_replace() {
    use std::os::unix::fs::symlink;
    let (directory, mut store) = store();
    create(&mut store);
    let id = TaskId::parse("6".repeat(32)).unwrap();
    let target = directory.path().join("outside");
    fs::write(&target, b"untouched").unwrap();
    symlink(&target, directory.path().join(format!("backup-{}.sqlite3", id.as_str()))).unwrap();
    assert!(matches!(store.backup(&id), Err(TaskStoreError::Conflict(_))));
    assert_eq!(fs::read(target).unwrap(), b"untouched");
}

#[test]
fn backup_replaced_source_is_not_published() {
    let directory = private_temp();
    let safe = SafeDirectory::open_or_create(directory.path().to_path_buf()).unwrap();
    safe.create_private_file("source").unwrap();
    let expected = safe.regular_private_identity("source").unwrap();
    INJECT_PUBLISH_SOURCE_SWAP.with(|flag| flag.set(true));
    assert!(matches!(
        safe.publish_verified_link("source", "published", expected),
        Err(TaskStoreError::UnsafePath(_))
    ));
    assert!(!directory.path().join("published").exists());
}

#[test]
fn backup_replaced_final_after_link_is_not_returned_or_left_published() {
    let (directory, mut store) = store();
    create(&mut store);
    let id = TaskId::parse("4".repeat(32)).unwrap();
    INJECT_PUBLISHED_FINAL_SWAP.with(|flag| flag.set(true));
    assert!(matches!(store.backup(&id), Err(TaskStoreError::UnsafePath(_))));
    assert!(!directory.path().join(format!("backup-{}.sqlite3", id.as_str())).exists());
}

#[test]
fn restart_without_checkpoint_marks_task_interrupted() {
    let (directory, mut store) = store();
    let task = create(&mut store);
    store.transition(&task.id, transition(TaskStatus::Pending, TaskStatus::Running, 20)).unwrap();
    let recovery_root = directory.path().join("checkpoints");
    let recovery = RecoveryStore::open(&recovery_root).unwrap();
    let summary = store.reconcile(&recovery).unwrap();
    assert_eq!(summary.interrupted, 1);
    let task = store.get(&task.id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Interrupted);
    assert_eq!(task.diagnostics[0].code, DiagnosticCode::RecoveryCheckpointMissing);
}

#[test]
fn restart_reconcile_promotes_real_converted_and_succeeded_checkpoints() {
    let directory = private_temp();
    let task_root = directory.path().join("tasks");
    let recovery_root = directory.path().join("recovery");
    let recovery = RecoveryStore::open(&recovery_root).unwrap();
    let token = recovery.create_token().unwrap();
    let request = || ConversionRequest::new(InputRef::bytes(b"hello".as_slice(), Some("x.txt")));

    let failing = recovery_engine(Arc::new(FailingRenderer));
    assert!(
        futures::executor::block_on(failing.convert_recoverable(request(), &recovery, &token))
            .is_err()
    );
    assert_eq!(recovery.inspect(&token).unwrap().unwrap().phase, TaskPhase::Converted);

    let mut store = TaskStore::open(&task_root, BusyControl::default()).unwrap();
    let checkpoint = recovery.inspect(&token).unwrap().unwrap();
    let task = store
        .create(NewTask {
            input: InputReference {
                schema_version: 1,
                input_fingerprint: checkpoint.input_fingerprint.clone(),
                options_fingerprint: checkpoint.options_fingerprint.clone(),
                byte_len: 5,
                recovery_token: token.as_str().into(),
            },
            configuration: ConfigurationSnapshot::default(),
        })
        .unwrap();
    drop(store);
    let mut restarted = TaskStore::open(&task_root, BusyControl::default()).unwrap();
    assert_eq!(restarted.reconcile(&recovery).unwrap().converted, 1);
    assert_eq!(restarted.get(&task.id).unwrap().unwrap().status, TaskStatus::Converted);

    let successful = recovery_engine(Arc::new(GfmRenderer));
    futures::executor::block_on(successful.convert_recoverable(request(), &recovery, &token))
        .unwrap();
    drop(restarted);
    let mut restarted = TaskStore::open(&task_root, BusyControl::default()).unwrap();
    assert_eq!(restarted.reconcile(&recovery).unwrap().converted, 1);
    assert_eq!(restarted.get(&task.id).unwrap().unwrap().status, TaskStatus::Converted);

    assert!(matches!(
        restarted.create(NewTask {
            input: InputReference {
                schema_version: 1,
                input_fingerprint: checkpoint.input_fingerprint,
                options_fingerprint: checkpoint.options_fingerprint,
                byte_len: 5,
                recovery_token: token.as_str().into(),
            },
            configuration: ConfigurationSnapshot::default(),
        }),
        Err(TaskStoreError::Conflict(_))
    ));
}

#[test]
fn restart_reconcile_rejects_input_and_options_fingerprint_mismatches() {
    for mismatch_input in [true, false] {
        let directory = private_temp();
        let recovery = RecoveryStore::open(directory.path().join("recovery")).unwrap();
        let token = recovery.create_token().unwrap();
        let engine = recovery_engine(Arc::new(GfmRenderer));
        futures::executor::block_on(engine.convert_recoverable(
            ConversionRequest::new(InputRef::bytes(b"bound".as_slice(), Some("x.txt"))),
            &recovery,
            &token,
        ))
        .unwrap();
        let checkpoint = recovery.inspect(&token).unwrap().unwrap();
        let mut input = InputReference {
            schema_version: 1,
            input_fingerprint: checkpoint.input_fingerprint,
            options_fingerprint: checkpoint.options_fingerprint,
            byte_len: 5,
            recovery_token: token.as_str().into(),
        };
        if mismatch_input {
            input.input_fingerprint = "f".repeat(64);
        } else {
            input.options_fingerprint = "e".repeat(64);
        }
        let mut store =
            TaskStore::open(directory.path().join("tasks"), BusyControl::default()).unwrap();
        let task = store
            .create(NewTask { input, configuration: ConfigurationSnapshot::default() })
            .unwrap();
        let summary = store.reconcile(&recovery).unwrap();
        assert_eq!(summary.failed, 1);
        let task = store.get(&task.id).unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.diagnostics[0].code, DiagnosticCode::RecoveryCheckpointIncompatible);
    }
}

#[test]
fn reconcile_same_state_repairs_low_progress_and_lost_cas_is_skipped() {
    let (_directory, mut store) = store();
    let task = create(&mut store);
    store.transition(&task.id, transition(TaskStatus::Pending, TaskStatus::Running, 1)).unwrap();
    store.transition(&task.id, transition(TaskStatus::Running, TaskStatus::Converted, 2)).unwrap();
    assert!(
        store
            .reconcile_transition(
                &task.id,
                TaskStatus::Converted,
                TaskStatus::Converted,
                900_000,
                None
            )
            .unwrap()
    );
    assert_eq!(store.get(&task.id).unwrap().unwrap().progress_millionths, 900_000);
    store.transition(&task.id, succeeded_transition()).unwrap();
    assert!(
        !store
            .reconcile_transition(
                &task.id,
                TaskStatus::Converted,
                TaskStatus::Succeeded,
                1_000_000,
                None
            )
            .unwrap()
    );
}

#[test]
fn transient_recovery_store_failure_propagates_without_changing_task() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = private_temp();
    let recovery_root = directory.path().join("recovery");
    let recovery = RecoveryStore::open(&recovery_root).unwrap();
    let token = recovery.create_token().unwrap();
    let mut store =
        TaskStore::open(directory.path().join("tasks"), BusyControl::default()).unwrap();
    let task = store
        .create(NewTask {
            input: InputReference {
                schema_version: 1,
                input_fingerprint: "a".repeat(64),
                options_fingerprint: "c".repeat(64),
                byte_len: 1,
                recovery_token: token.as_str().into(),
            },
            configuration: ConfigurationSnapshot::default(),
        })
        .unwrap();
    fs::rename(&recovery_root, directory.path().join("recovery-moved")).unwrap();
    fs::create_dir(&recovery_root).unwrap();
    fs::set_permissions(&recovery_root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(store.reconcile(&recovery), Err(TaskStoreError::Io(_))));
    let unchanged = store.get(&task.id).unwrap().unwrap();
    assert_eq!(unchanged.status, TaskStatus::Pending);
    assert!(unchanged.diagnostics.is_empty());
}

#[test]
fn public_roots_and_symlinked_roots_or_database_files_are_rejected() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let outer = private_temp();
    let public = outer.path().join("public");
    fs::create_dir(&public).unwrap();
    fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        TaskStore::open(&public, BusyControl::default()),
        Err(TaskStoreError::UnsafePath(_))
    ));
    let private = outer.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let linked = outer.path().join("linked");
    symlink(&private, &linked).unwrap();
    assert!(TaskStore::open(&linked, BusyControl::default()).is_err());

    let db_root = outer.path().join("db-root");
    fs::create_dir(&db_root).unwrap();
    fs::set_permissions(&db_root, fs::Permissions::from_mode(0o700)).unwrap();
    let target = outer.path().join("target");
    fs::write(&target, b"not touched").unwrap();
    symlink(&target, db_root.join(DATABASE_FILE)).unwrap();
    assert!(TaskStore::open(&db_root, BusyControl::default()).is_err());
    assert_eq!(fs::read(target).unwrap(), b"not touched");
}

#[test]
fn namespace_swap_is_detected_before_more_mutation() {
    use std::os::unix::fs::PermissionsExt as _;
    let outer = private_temp();
    let root = outer.path().join("store");
    let mut store = TaskStore::open(&root, BusyControl::default()).unwrap();
    let task = create(&mut store);
    fs::rename(&root, outer.path().join("moved")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        store.set_pinned(&task.id, true),
        Err(TaskStoreError::UnsafePath(_) | TaskStoreError::Io(_))
    ));
    let moved = TaskStore::open(outer.path().join("moved"), BusyControl::default()).unwrap();
    assert!(!moved.get(&task.id).unwrap().unwrap().pinned);
}

#[test]
fn permission_and_database_identity_changes_are_rejected_before_mutation() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = private_temp();
    let mut store = TaskStore::open(directory.path(), BusyControl::default()).unwrap();
    let task = create(&mut store);

    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(store.set_pinned(&task.id, true), Err(TaskStoreError::UnsafePath(_))));
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!store.get(&task.id).unwrap().unwrap().pinned);

    let database = directory.path().join(DATABASE_FILE);
    let original = directory.path().join("original.sqlite3");
    fs::rename(&database, &original).unwrap();
    fs::copy(&original, &database).unwrap();
    fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(store.set_pinned(&task.id, true), Err(TaskStoreError::UnsafePath(_))));
    fs::remove_file(&database).unwrap();
    fs::rename(&original, &database).unwrap();
    assert!(!store.get(&task.id).unwrap().unwrap().pinned);
}

#[test]
fn limits_reject_oversized_pages_children_and_noncanonical_references() {
    let (_directory, mut store) = store();
    assert!(matches!(store.list(0, None), Err(TaskStoreError::Limit(_))));
    assert!(matches!(store.list(101, None), Err(TaskStoreError::Limit(_))));
    assert!(matches!(
        store.create(NewTask {
            input: InputReference {
                schema_version: 1,
                input_fingerprint: "secret-not-a-digest".into(),
                options_fingerprint: "c".repeat(64),
                byte_len: 1,
                recovery_token: "a".repeat(32),
            },
            configuration: ConfigurationSnapshot::default(),
        }),
        Err(TaskStoreError::Limit(_))
    ));
    let task = create(&mut store);
    let oversized_artifact = ArtifactReference {
        storage_key: "d".repeat(32),
        kind: ArtifactKind::Asset,
        byte_len: u64::MAX,
        sha256: "e".repeat(64),
        asset_id: Some("asset-1".into()),
        filename: Some("asset.bin".into()),
        media_type: Some("application/octet-stream".into()),
    };
    assert!(matches!(
        store.transition(
            &task.id,
            TaskTransition {
                expected: TaskStatus::Pending,
                next: TaskStatus::Running,
                progress_millionths: 1,
                diagnostics: vec![],
                artifacts: vec![oversized_artifact],
            }
        ),
        Err(TaskStoreError::Limit(_))
    ));
    let task = create(&mut store);
    let diagnostics =
        (0..65).map(|_| TaskDiagnostic { code: DiagnosticCode::ConversionFailed }).collect();
    assert!(matches!(
        store.transition(
            &task.id,
            TaskTransition {
                expected: TaskStatus::Pending,
                next: TaskStatus::Failed,
                progress_millionths: 0,
                diagnostics,
                artifacts: vec![],
            }
        ),
        Err(TaskStoreError::Limit(_))
    ));

    let oversized = private_temp();
    drop(TaskStore::open(oversized.path(), BusyControl::default()).unwrap());
    let file =
        fs::OpenOptions::new().write(true).open(oversized.path().join(DATABASE_FILE)).unwrap();
    file.set_len(258 * 1024 * 1024).unwrap();
    assert!(matches!(
        TaskStore::open(oversized.path(), BusyControl::default()),
        Err(TaskStoreError::Limit(_))
    ));
}

#[test]
fn terminal_transaction_accepts_the_complete_128_artifact_boundary() {
    let (_directory, mut store) = store();
    let task = create(&mut store);
    store.transition(&task.id, transition(TaskStatus::Pending, TaskStatus::Running, 1)).unwrap();
    store
        .transition(&task.id, transition(TaskStatus::Running, TaskStatus::Converted, 900_000))
        .unwrap();
    let mut completed = succeeded_transition();
    completed.artifacts.try_reserve_exact(124).unwrap();
    for index in 0..124 {
        completed.artifacts.push(ArtifactReference {
            storage_key: format!("{:032x}", index + 4),
            kind: ArtifactKind::Asset,
            byte_len: 1,
            sha256: format!("{:064x}", index + 4),
            asset_id: Some(format!("asset-{index}")),
            filename: Some(format!("asset-{index}.bin")),
            media_type: Some("application/octet-stream".into()),
        });
    }
    let succeeded = store.transition(&task.id, completed).unwrap();
    assert_eq!(succeeded.status, TaskStatus::Succeeded);
    assert_eq!(succeeded.artifacts.len(), 128);
}

#[test]
fn committed_transaction_survives_process_abort_fixture() {
    const CHILD: &str = "INTO_MD_TASK_STORE_ABORT_CHILD";
    if let Ok(root) = std::env::var(CHILD) {
        let mut store = TaskStore::open(root, BusyControl::default()).unwrap();
        create(&mut store);
        std::process::abort();
    }
    let directory = private_temp();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::committed_transaction_survives_process_abort_fixture"])
        .arg("--nocapture")
        .env(CHILD, directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let store = TaskStore::open(directory.path(), BusyControl::default()).unwrap();
    assert_eq!(store.list(10, None).unwrap().len(), 1);
}

#[test]
fn uncommitted_wal_transaction_is_rolled_back_after_abort() {
    const CHILD: &str = "INTO_MD_TASK_STORE_UNCOMMITTED_ABORT_CHILD";
    if let Ok(root) = std::env::var(CHILD) {
        let mut store = TaskStore::open(root, BusyControl::default()).unwrap();
        let task = create(&mut store);
        store.connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        store
            .connection
            .execute("UPDATE tasks SET pinned=1 WHERE id=?1", [task.id.as_str()])
            .unwrap();
        std::process::abort();
    }
    let directory = private_temp();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::uncommitted_wal_transaction_is_rolled_back_after_abort"])
        .arg("--nocapture")
        .env(CHILD, directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let store = TaskStore::open(directory.path(), BusyControl::default()).unwrap();
    let tasks = store.list(10, None).unwrap();
    assert_eq!(tasks.len(), 1);
    assert!(!tasks[0].pinned);
    verify_integrity(&store.connection).unwrap();
}

#[test]
fn migration_abort_rolls_back_and_next_open_upgrades_cleanly() {
    const CHILD: &str = "INTO_MD_TASK_STORE_MIGRATION_ABORT_CHILD";
    if let Ok(root) = std::env::var(CHILD) {
        let _ = TaskStore::open(root, BusyControl::default());
        unreachable!();
    }
    let directory = private_temp();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::migration_abort_rolls_back_and_next_open_upgrades_cleanly"])
        .arg("--nocapture")
        .env(CHILD, directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let store = TaskStore::open(directory.path(), BusyControl::default()).unwrap();
    let version: i64 =
        store.connection.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    verify_integrity(&store.connection).unwrap();
}

#[test]
fn backup_abort_never_publishes_partial_destination() {
    const CHILD: &str = "INTO_MD_TASK_STORE_BACKUP_ABORT_CHILD";
    let backup_id = TaskId::parse("9".repeat(32)).unwrap();
    if let Ok(root) = std::env::var(CHILD) {
        let mut store = TaskStore::open(root, BusyControl::default()).unwrap();
        create(&mut store);
        let _ = store.backup(&backup_id);
        unreachable!();
    }
    let directory = private_temp();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::backup_abort_never_publishes_partial_destination"])
        .arg("--nocapture")
        .env(CHILD, directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!directory.path().join(format!("backup-{}.sqlite3", backup_id.as_str())).exists());
    let orphan_count = fs::read_dir(directory.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".backup-"))
        .count();
    assert_eq!(orphan_count, 1);
}

use super::*;

#[test]
fn receipt_identity_reconciles_transport_and_retains_one_task() {
    let temp = tempfile::tempdir().unwrap();
    let backend = WebTaskBackend::open(temp.path().join("data")).unwrap();
    let id = "a".repeat(32);
    let request = WebTaskRequest::default();
    assert_eq!(
        backend.create_receipt(&id, "note.txt", 5, request.clone()).unwrap().state,
        "waiting"
    );
    assert!(backend.create_receipt(&id, "different.txt", 5, request.clone()).is_err());
    let (mut receipt, mut upload) = backend.receive_upload(&id).unwrap();
    assert!(backend.receive_upload(&id).is_err());
    upload.write_chunk(b"hello").unwrap();
    upload.seal().unwrap();
    backend.change_receipt(&mut receipt, "receiving", None, None).unwrap();
    let record = upload.finish().unwrap();
    assert_eq!(backend.receipt(&id).unwrap().task_id, Some(record.id.clone()));
    assert_eq!(
        backend.create_receipt(&id, "note.txt", 5, request).unwrap().task_id,
        Some(record.id.clone())
    );
    assert_eq!(wait_terminal(&backend, &record.id).status, TaskStatus::Succeeded);
    drop(backend);
    let reopened = WebTaskBackend::open(temp.path().join("data")).unwrap();
    assert_eq!(reopened.receipt(&id).unwrap().task_id, Some(record.id.clone()));
    reopened.delete(&record.id).unwrap();
    assert!(matches!(reopened.receipt(&id), Err(WebTaskError::NotFound)));
}

#[test]
fn receipt_cancellation_stops_upload_and_releases_quota() {
    let temp = tempfile::tempdir().unwrap();
    let backend = WebTaskBackend::open(temp.path().join("data")).unwrap();
    let id = "b".repeat(32);
    backend.create_receipt(&id, "note.txt", 10, WebTaskRequest::default()).unwrap();
    let (_, mut upload) = backend.receive_upload(&id).unwrap();
    upload.write_chunk(b"hello").unwrap();
    backend.cancel_receipt(&id).unwrap();
    assert!(matches!(upload.write_chunk(b"world"), Err(WebTaskError::Cancelled)));
    drop(upload);
    assert_eq!(backend.test_reserved_bytes(), 0);
    assert_eq!(backend.receipt(&id).unwrap().state, "cancelled");
}

#[test]
fn indexed_history_and_preview_survive_restart_without_copying_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let backend = WebTaskBackend::open(temp.path().join("data")).unwrap();
    let batch = "c".repeat(32);
    let mut request = WebTaskRequest::default();
    request.batch_id = Some(batch.clone());
    let source = format!("# 标题\n{}", "汉".repeat(100_000));
    let mut upload =
        backend.begin_upload_configured("报告.txt", Some(source.len() as u64), request).unwrap();
    for chunk in source.as_bytes().chunks(1234) {
        upload.write_chunk(chunk).unwrap();
    }
    let task = wait_terminal(&backend, &upload.finish().unwrap().id);
    assert_eq!(task.status, TaskStatus::Succeeded);
    let artifact = task.artifacts.iter().find(|a| a.kind == ArtifactKind::Markdown).unwrap();
    let preview =
        backend.preview(&task.id, &artifact.storage_key, &CancellationToken::new()).unwrap();
    let wire = serde_json::to_value(&preview).unwrap();
    assert!(wire["truncated"].as_bool().unwrap());
    assert!(!wire["text"].as_str().unwrap().contains('\u{fffd}'));
    let page = backend
        .query(10, None, None, None, Some(&batch), Some("conversion"), "报告", Some(false))
        .unwrap();
    assert_eq!(page.tasks.len(), 1);
    let summaries = serde_json::to_value(backend.summaries(&[task.id.clone()]).unwrap()).unwrap();
    assert_eq!(summaries[0]["artifacts"].as_array().unwrap().len(), 0);
    assert_eq!(backend.test_reserved_bytes(), 0);
    drop(backend);
    let reopened = WebTaskBackend::open(temp.path().join("data")).unwrap();
    let again = serde_json::to_value(
        reopened.preview(&task.id, &artifact.storage_key, &CancellationToken::new()).unwrap(),
    )
    .unwrap();
    assert_eq!(wire, again);
}

#[test]
fn streaming_fingerprint_matches_memory_and_validates_length() {
    let options = ConversionOptions::default();
    let hint = FormatHint::default();
    let bytes = "流式摘要".repeat(20000).into_bytes();
    assert_eq!(
        Engine::recoverable_fingerprints(&bytes, Some("示例.txt"), &hint, &options).unwrap(),
        Engine::recoverable_fingerprints_reader(
            std::io::Cursor::new(&bytes),
            bytes.len() as u64,
            Some("示例.txt"),
            &hint,
            &options
        )
        .unwrap()
    );
    assert!(
        Engine::recoverable_fingerprints_reader(
            std::io::Cursor::new(&bytes),
            bytes.len() as u64 - 1,
            None,
            &hint,
            &options
        )
        .is_err()
    );
}

#[test]
fn stale_receipt_updates_preserve_cancellation() {
    let temp = tempfile::tempdir().unwrap();
    let backend = WebTaskBackend::open(temp.path().join("data")).unwrap();
    let id = "d".repeat(32);
    let mut stale = backend.create_receipt(&id, "note.txt", 5, WebTaskRequest::default()).unwrap();
    backend.cancel_receipt(&id).unwrap();
    assert!(backend.change_receipt(&mut stale, "uploading", None, None).is_err());
    assert_eq!(stale.state, "waiting");
    assert_eq!(backend.receipt(&id).unwrap().state, "cancelled");
}

#[test]
fn completed_event_history_is_bounded_and_evicted_cursors_recover_monotonically() {
    let generation = "aa".repeat(16);
    let hub = EventHub::new(generation.clone());
    let first = event_record(TaskStatus::Succeeded, 1);
    let cursor = hub.subscribe(&first, None).replay.pop_front().unwrap().sequence;
    for index in 1..600 {
        let mut record = event_record(TaskStatus::Succeeded, index);
        record.id = TaskId::parse(format!("{index:032x}")).unwrap();
        record.updated_at_ms = i64::from(index) + 3;
        hub.publish_snapshot(&record);
    }
    assert!(lock(&hub.state).logs.len() <= 256);
    let recovered = hub.subscribe(&first, Some((&generation, cursor))).replay.pop_front().unwrap();
    assert!(recovered.sequence > cursor);
    assert!(recovered.terminal);
}

#[test]
fn recent_status_queries_stay_available_during_slow_database_work() {
    let temp = tempfile::tempdir().unwrap();
    let backend = WebTaskBackend::open(temp.path().join("data")).unwrap();
    let mut upload = backend.begin_upload("note.txt", Some(5)).unwrap();
    upload.write_chunk(b"hello").unwrap();
    let task = wait_terminal(&backend, &upload.finish().unwrap().id);
    backend.set_pinned(&task.id, true).unwrap();
    let database = lock(&backend.owner.shared.task_store);
    let reader = backend.clone();
    let id = task.id.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        send.send(reader.summaries(&[id])).unwrap();
    });
    let response = receive.recv_timeout(Duration::from_secs(1));
    drop(database);
    worker.join().unwrap();
    let wire = serde_json::to_value(response.unwrap().unwrap()).unwrap();
    assert_eq!(wire[0]["pinned"], true);
    backend.delete(&task.id).unwrap();
    assert!(backend.summaries(&[task.id]).unwrap().is_empty());
}

//! Upload transport, task observations and bounded result reads.
use super::*;

pub(super) async fn create_upload_receipt(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    request: Request,
) -> Response {
    let headers = request.headers();
    let Some(name) = single_ascii_header(headers, TASK_FILENAME_HEADER.clone())
        .and_then(|v| URL_SAFE_NO_PAD.decode(v).ok())
        .and_then(|v| String::from_utf8(v).ok())
    else {
        return rejection(StatusCode::BAD_REQUEST, "invalidFilename");
    };
    let Some(size) = single_ascii_header(headers, HeaderName::from_static("x-into-md-size"))
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return rejection(StatusCode::BAD_REQUEST, "invalidUploadSize");
    };
    let options = match single_ascii_header(headers, TASK_REQUEST_HEADER.clone())
        .and_then(|v| URL_SAFE_NO_PAD.decode(v).ok())
        .map(|v| decode_web_task_request(&v))
    {
        Some(Ok(value)) => value,
        _ => return rejection(StatusCode::BAD_REQUEST, "invalidTaskOptions"),
    };
    match tokio::task::spawn_blocking(move || state.tasks.create_receipt(&id, &name, size, options))
        .await
    {
        Ok(Ok(receipt)) => Json(receipt.wire()).into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}
pub(super) async fn upload_receipt(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match tokio::task::spawn_blocking(move || state.tasks.receipt(&id)).await {
        Ok(Ok(receipt)) => Json(receipt.wire()).into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}
pub(super) async fn cancel_upload_receipt(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match tokio::task::spawn_blocking(move || state.tasks.cancel_receipt(&id)).await {
        Ok(Ok(receipt)) => Json(receipt.wire()).into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}
struct ReceiptTransportGuard {
    backend: WebTaskBackend,
    id: String,
}
impl Drop for ReceiptTransportGuard {
    fn drop(&mut self) {
        if let Ok(mut receipt) = self.backend.receipt(&self.id) {
            if receipt.state == "uploading" {
                let _ = self.backend.change_receipt(
                    &mut receipt,
                    "interrupted",
                    None,
                    Some("uploadDisconnected"),
                );
            }
        }
    }
}
pub(super) async fn receive_upload_body(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    request: Request,
) -> Response {
    let permit = match state.upload_gate.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return rejection(StatusCode::SERVICE_UNAVAILABLE, "uploadBusy"),
    };
    let backend = state.tasks.clone();
    let (mut receipt, mut upload) =
        match tokio::task::spawn_blocking(move || backend.receive_upload(&id)).await {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return web_task_rejection(error),
            Err(_) => return rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
        };
    let _transport = ReceiptTransportGuard { backend: state.tasks.clone(), id: receipt.id.clone() };
    let upload_started = std::time::Instant::now();
    let mut stream = request.into_body().into_data_stream();
    let deadline = Instant::now() + REQUEST_TOTAL_TIMEOUT;
    let mut received = 0u64;
    loop {
        let next = tokio::time::timeout_at(
            deadline.min(Instant::now() + REQUEST_IDLE_TIMEOUT),
            stream.next(),
        )
        .await;
        let chunk = match next {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(None) => break,
            _ => {
                let _ = state.tasks.change_receipt(
                    &mut receipt,
                    "interrupted",
                    None,
                    Some("uploadDisconnected"),
                );
                return rejection(StatusCode::REQUEST_TIMEOUT, "uploadDisconnected");
            }
        };
        received = received.saturating_add(chunk.len() as u64);
        if received > receipt.size {
            let _ =
                state.tasks.change_receipt(&mut receipt, "failed", None, Some("invalidUploadSize"));
            return rejection(StatusCode::BAD_REQUEST, "invalidUploadSize");
        }
        upload = match tokio::task::spawn_blocking(move || {
            upload.write_chunk(&chunk)?;
            Ok::<_, WebTaskError>(upload)
        })
        .await
        {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                let _ =
                    state.tasks.change_receipt(&mut receipt, "failed", None, Some("uploadFailed"));
                return web_task_rejection(error);
            }
            Err(_) => return rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
        };
    }
    if received != receipt.size {
        let _ = state.tasks.change_receipt(
            &mut receipt,
            "interrupted",
            None,
            Some("invalidUploadSize"),
        );
        return rejection(StatusCode::BAD_REQUEST, "invalidUploadSize");
    }
    upload = match tokio::task::spawn_blocking(move || {
        upload.seal()?;
        Ok::<_, WebTaskError>(upload)
    })
    .await
    {
        Ok(Ok(upload)) => upload,
        Ok(Err(error)) => return web_task_rejection(error),
        Err(_) => return rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    };
    if let Err(error) = state.tasks.change_receipt(&mut receipt, "receiving", None, None) {
        return web_task_rejection(error);
    }
    crate::web_tasks::trace_upload(&receipt.id, upload_started.elapsed());
    let wire = receipt.wire();
    prepare_received(state.tasks, receipt, upload, permit);
    (StatusCode::ACCEPTED, Json(wire)).into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryRequest {
    ids: Vec<String>,
}

pub(super) async fn task_summaries(
    State(state): State<AppState>,
    Json(input): Json<SummaryRequest>,
) -> Response {
    if input.ids.len() > 100 {
        return rejection(StatusCode::BAD_REQUEST, "invalidTaskIds");
    }
    let ids =
        input.ids.into_iter().map(into_markdown::TaskId::parse).collect::<Result<Vec<_>, _>>();
    let Ok(ids) = ids else {
        return rejection(StatusCode::BAD_REQUEST, "invalidTaskIds");
    };
    match tokio::task::spawn_blocking(move || state.tasks.summaries(&ids)).await {
        Ok(Ok(tasks)) => Json(serde_json::json!({"schemaVersion":1,"tasks":tasks})).into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}

struct CancelPreview(into_markdown::CancellationToken);
impl Drop for CancelPreview {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(super) async fn task_preview(
    State(state): State<AppState>,
    AxumPath((id, key)): AxumPath<(String, String)>,
) -> Response {
    let Ok(id) = into_markdown::TaskId::parse(id) else {
        return rejection(StatusCode::BAD_REQUEST, "invalidTaskId");
    };
    let permit = match state.preview_gate.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return rejection(StatusCode::SERVICE_UNAVAILABLE, "previewBusy"),
    };
    let cancellation = into_markdown::CancellationToken::new();
    let _guard = CancelPreview(cancellation.clone());
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        state.tasks.preview(&id, &key, &cancellation)
    })
    .await
    {
        Ok(Ok(preview)) => Json(preview).into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}

pub(super) async fn task_detail(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Ok(id) = into_markdown::TaskId::parse(id) else {
        return rejection(StatusCode::BAD_REQUEST, "invalidTaskId");
    };
    match tokio::task::spawn_blocking(move || state.tasks.detail(&id)).await {
        Ok(Ok(record)) => Json(record).into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}

pub(super) fn schedule_task_maintenance(state: &AppState) {
    let maintenance = state.tasks.clone();
    let requested = maintenance.maintenance_signal();
    let mut maintenance_shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        loop {
            if *maintenance_shutdown.borrow() {
                break;
            }
            let backend = maintenance.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(i64::MAX as u128) as i64;
                backend.cleanup(RetentionPolicy::default(), now)
            })
            .await;
            tokio::select! { _ = tokio::time::sleep(Duration::from_secs(300)) => {}, _ = requested.notified() => {}, _ = maintenance_shutdown.changed() => break }
        }
    });
}

pub(super) fn task_flow_routes() -> Router<AppState> {
    Router::new()
        .route("/tasks", get(list_tasks).post(upload_task).fallback(api_method_not_allowed))
        .route("/tasks/{id}", get(task_status).delete(cancel_task).fallback(api_method_not_allowed))
        .route("/tasks/{id}/cancel", post(cancel_task).fallback(api_method_not_allowed))
        .route("/tasks/{id}/retry", post(retry_task).fallback(api_method_not_allowed))
        .route("/tasks/{id}/pin", post(pin_task).fallback(api_method_not_allowed))
        .route(
            "/uploads/{id}",
            post(create_upload_receipt)
                .get(upload_receipt)
                .put(receive_upload_body)
                .delete(cancel_upload_receipt)
                .fallback(api_method_not_allowed),
        )
        .route("/tasks/summaries", post(task_summaries).fallback(api_method_not_allowed))
        .route("/tasks/{id}/previews/{key}", get(task_preview).fallback(api_method_not_allowed))
        .route("/tasks/{id}/detail", get(task_detail).fallback(api_method_not_allowed))
}

fn prepare_received(
    backend: WebTaskBackend,
    mut receipt: crate::web_tasks::UploadReceipt,
    upload: crate::web_tasks::Upload,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let started = std::time::Instant::now();
        if let Err(error) = upload.finish() {
            let _ = backend.change_receipt(
                &mut receipt,
                "failed",
                None,
                Some(match error {
                    WebTaskError::Cancelled => "cancelled",
                    _ => "uploadFailed",
                }),
            );
        }
        eprintln!(
            "web stage=receivePrepare submission={} elapsed_ms={}",
            receipt.id,
            started.elapsed().as_millis()
        );
    });
}
pub(super) async fn list_tasks(State(state): State<AppState>, request: Request) -> Response {
    let Ok(query) = parse_task_list_query(request.uri().query()) else {
        return rejection(StatusCode::BAD_REQUEST, "invalidHistoryQuery");
    };
    let headers = request.headers();
    if headers.contains_key(header::CONTENT_TYPE) || !request_body_is_empty(headers) {
        return rejection(StatusCode::BAD_REQUEST, "requestBodyNotAllowed");
    }
    let after = match (query.after_updated_at_ms, query.after_id) {
        (None, None) => None,
        (Some(updated_at_ms), Some(id)) => match into_markdown::TaskId::parse(id) {
            Ok(id) => Some(into_markdown::TaskCursor { updated_at_ms, id }),
            Err(_) => return rejection(StatusCode::BAD_REQUEST, "invalidCursor"),
        },
        _ => return rejection(StatusCode::BAD_REQUEST, "invalidCursor"),
    };
    match tokio::task::spawn_blocking(move || {
        let backend = state.tasks;
        let page = backend.query(
            query.limit.unwrap_or(25),
            after.as_ref(),
            query.status,
            query.pinned,
            query.batch_id.as_deref(),
            query.workflow.as_deref(),
            query.search.as_deref().unwrap_or(""),
            query.active,
        )?;
        let tasks = page
            .tasks
            .into_iter()
            .map(|record| backend.web_record(record))
            .collect::<Result<Vec<_>, _>>()?;
        Ok::<_, WebTaskError>((tasks, page.next))
    })
    .await
    {
        Ok(Ok((tasks, next))) => Json(TaskListDto {
            schema_version: 1,
            tasks,
            next_cursor: next
                .map(|cursor| TaskCursorDto { updated_at_ms: cursor.updated_at_ms, id: cursor.id }),
        })
        .into_response(),
        Ok(Err(error)) => web_task_rejection(error),
        Err(_) => rejection(StatusCode::INTERNAL_SERVER_ERROR, "backendWorkerFailed"),
    }
}

pub(super) fn parse_task_list_query(value: Option<&str>) -> Result<TaskListQuery, ()> {
    let mut query = TaskListQuery {
        limit: None,
        after_updated_at_ms: None,
        after_id: None,
        status: None,
        pinned: None,
        batch_id: None,
        workflow: None,
        search: None,
        active: None,
    };
    let Some(value) = value else { return Ok(query) };
    for field in value.split('&') {
        let (name, value) = field.split_once('=').ok_or(())?;
        if name == "search" && query.search.is_none() {
            let decoded = URL_SAFE_NO_PAD.decode(value).map_err(|_| ())?;
            let decoded = String::from_utf8(decoded).map_err(|_| ())?;
            if decoded.len() > 255 {
                return Err(());
            }
            query.search = Some(decoded);
            continue;
        }
        if value.is_empty() || value.contains('%') || value.contains('+') {
            return Err(());
        }
        match name {
            "workflow"
                if query.workflow.is_none()
                    && matches!(value, "conversion" | "meetingTranscript") =>
            {
                query.workflow = Some(value.to_owned())
            }
            "active" if query.active.is_none() => {
                query.active = Some(match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err(()),
                })
            }
            "limit" if query.limit.is_none() => query.limit = Some(value.parse().map_err(|_| ())?),
            "afterUpdatedAtMs" if query.after_updated_at_ms.is_none() => {
                query.after_updated_at_ms = Some(value.parse().map_err(|_| ())?);
            }
            "afterId" if query.after_id.is_none() => query.after_id = Some(value.to_owned()),
            "pinned" if query.pinned.is_none() => {
                query.pinned = Some(match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err(()),
                });
            }
            "batchId"
                if query.batch_id.is_none()
                    && value.len() == 32
                    && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && !value.bytes().any(|byte| byte.is_ascii_uppercase()) =>
            {
                query.batch_id = Some(value.to_owned());
            }
            "status" if query.status.is_none() => {
                query.status = Some(match value {
                    "pending" => into_markdown::TaskStatus::Pending,
                    "running" => into_markdown::TaskStatus::Running,
                    "converted" => into_markdown::TaskStatus::Converted,
                    "succeeded" => into_markdown::TaskStatus::Succeeded,
                    "failed" => into_markdown::TaskStatus::Failed,
                    "interrupted" => into_markdown::TaskStatus::Interrupted,
                    "cancelled" => into_markdown::TaskStatus::Cancelled,
                    _ => return Err(()),
                });
            }
            _ => return Err(()),
        }
    }
    Ok(query)
}

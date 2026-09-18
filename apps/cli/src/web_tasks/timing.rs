//! Optional correlation-only timings; source content and credentials are excluded.
use super::*;

pub(super) struct Stage {
    stage: &'static str,
    task: Option<TaskId>,
    submission: Option<String>,
    generation: u64,
    started: Instant,
}
impl Stage {
    pub(super) fn new(
        stage: &'static str,
        task: Option<&TaskId>,
        submission: Option<&str>,
        generation: u64,
    ) -> Self {
        Self {
            stage,
            task: task.cloned(),
            submission: submission.map(str::to_owned),
            generation,
            started: Instant::now(),
        }
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        emit(
            self.stage,
            self.task.as_ref(),
            self.submission.as_deref(),
            self.generation,
            self.started.elapsed(),
        );
    }
}
fn emit(
    stage: &str,
    task: Option<&TaskId>,
    submission: Option<&str>,
    generation: u64,
    elapsed: Duration,
) {
    if std::env::var_os("INTO_MD_WEB_TIMINGS").is_some() {
        eprintln!(
            "webTiming {}",
            serde_json::json!({ "stage": stage, "taskId": task, "submissionId": submission, "resultGeneration": generation, "elapsedMs": elapsed.as_millis() })
        );
    }
}
pub(super) fn accepted(task: &TaskId, submission: Option<&str>) {
    emit("accepted", Some(task), submission, 0, Duration::ZERO);
}
pub(super) fn transition(record: &TaskRecord, elapsed: Duration) {
    let stage = match record.status {
        TaskStatus::Pending => "queue",
        TaskStatus::Running => "conversion",
        TaskStatus::Converted => "publish",
        _ => return,
    };
    emit(stage, Some(&record.id), None, record.artifact_generation, elapsed);
}

pub(crate) fn trace_upload(submission: &str, elapsed: Duration) {
    emit("upload", None, Some(submission), 0, elapsed);
}

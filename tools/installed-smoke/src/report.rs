//! Stable machine-readable smoke report.

use serde::Serialize;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// Outcome of one smoke case.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseResult {
    /// Stable case ID.
    pub id: String,
    /// Outcome.
    pub status: CaseStatus,
    /// Stable result or error code.
    pub code: String,
    /// Sanitized detail without local paths or command output.
    pub detail: String,
}

/// Case status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CaseStatus {
    /// Contract passed.
    Passed,
    /// Contract failed.
    Failed,
}

/// Installed optional-runtime capability observed by `doctor`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityResult {
    /// Catalog component ID.
    pub id: String,
    /// Stable doctor status.
    pub status: String,
    /// Whether both catalog and doctor supplied actionable guidance.
    pub install_hint_present: bool,
}

impl CaseResult {
    pub(crate) fn passed(id: &str, detail: &str) -> Self {
        Self { id: id.into(), status: CaseStatus::Passed, code: "ok".into(), detail: detail.into() }
    }

    pub(crate) fn failed(id: &str, code: &str, detail: &str) -> Self {
        Self {
            id: id.into(),
            status: CaseStatus::Failed,
            code: code.into(),
            detail: sanitize(detail),
        }
    }
}

/// Cleanup outcome.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupResult {
    /// Whether all owned resources were removed.
    pub clean: bool,
    /// Sanitized status.
    pub detail: String,
}

impl CleanupResult {
    pub(crate) fn clean() -> Self {
        Self { clean: true, detail: "all runner-owned resources removed".into() }
    }

    pub(crate) fn failed(detail: &str) -> Self {
        Self { clean: false, detail: sanitize(detail) }
    }
}

/// Complete installed smoke report.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmokeReport {
    /// Wire schema version.
    pub schema_version: u64,
    /// Target platform.
    pub platform: String,
    /// Target architecture.
    pub architecture: String,
    /// Hash of the tested release archive.
    pub archive_sha256: String,
    /// Overall result.
    pub passed: bool,
    /// Ordered case outcomes.
    pub cases: Vec<CaseResult>,
    /// Optional runtime capabilities discovered from the authoritative catalog.
    pub capabilities: Vec<CapabilityResult>,
    /// Resource cleanup result.
    pub cleanup: CleanupResult,
    /// Bounded elapsed milliseconds.
    pub elapsed_millis: u64,
}

impl SmokeReport {
    pub(crate) fn new(
        platform: &str,
        architecture: &str,
        archive_sha256: String,
        cases: Vec<CaseResult>,
        capabilities: Vec<CapabilityResult>,
        cleanup: CleanupResult,
        elapsed: Duration,
    ) -> Self {
        let passed = cleanup.clean && cases.iter().all(|case| case.status == CaseStatus::Passed);
        Self {
            schema_version: 1,
            platform: platform.into(),
            architecture: architecture.into(),
            archive_sha256,
            passed,
            cases,
            capabilities,
            cleanup,
            elapsed_millis: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        }
    }

    pub(crate) fn write(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("cannot encode report: {error}"))?;
        let parent = path.parent().ok_or_else(|| "report path has no parent".to_owned())?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| format!("cannot create report temporary: {error}"))?;
        temporary
            .write_all(&bytes)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|error| format!("cannot write report: {error}"))?;
        temporary
            .persist_noclobber(path)
            .map(drop)
            .map_err(|error| format!("cannot commit report: {}", error.error))
    }
}

fn sanitize(detail: &str) -> String {
    let normalized = detail.replace(['\r', '\n'], " ");
    let mut value = normalized
        .split_whitespace()
        .map(|token| {
            if token.starts_with('/')
                || token.starts_with("\\\\")
                || token.as_bytes().get(1) == Some(&b':')
            {
                "<path>"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    value.truncate(240);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_detail_redacts_local_paths() {
        let value = sanitize("failed /Users/alice/secret C:\\Users\\alice\\secret");
        assert_eq!(value, "failed <path> <path>");
    }

    #[test]
    fn report_never_overwrites_an_existing_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("report.json");
        std::fs::write(&path, b"existing").unwrap();
        let report = SmokeReport::new(
            "macos",
            "aarch64",
            "a".repeat(64),
            vec![],
            vec![],
            CleanupResult::clean(),
            Duration::ZERO,
        );
        assert!(report.write(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
    }
}

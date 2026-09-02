//! Central policy for bounded partial delivery after resource failures.
//!
//! Recovery is deliberately capability based: a caller must prove that the
//! failed unit can be rolled back, has a stable source locator, and leaves a
//! useful fallback before a resource error may become a warning.

use crate::{ConversionError, Diagnostic, DiagnosticSeverity, ErrorPolicy, SourceLocator};

/// Smallest independently recoverable content unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResourceUnitKind {
    /// One PDF or paginated document page.
    Page,
    /// One embedded or standalone image.
    Image,
    /// One frame in a multi-frame raster.
    Frame,
    /// One EPUB or document chapter.
    Chapter,
    /// One presentation slide.
    Slide,
    /// One workbook sheet.
    Sheet,
    /// One table or bounded table region.
    Table,
    /// One optional attachment or relationship.
    Attachment,
    /// One independently authenticated archive member.
    ArchiveMember,
}

impl ResourceUnitKind {
    /// Stable lower-camel-case name used in diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Image => "image",
            Self::Frame => "frame",
            Self::Chapter => "chapter",
            Self::Slide => "slide",
            Self::Sheet => "sheet",
            Self::Table => "table",
            Self::Attachment => "attachment",
            Self::ArchiveMember => "archiveMember",
        }
    }
}

/// Scope in which the failure occurred. The same limit can be local in one
/// scope and request-wide in another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResourceFailureScope {
    /// OCR normalization, recognition, or OCR result materialization for one visual.
    VisualRecognition,
    /// One parser-owned unit that has not yet been committed.
    ContentUnit,
    /// A bounded sequence after already committed units.
    Sequence,
    /// Input admission, root parsing, aggregate rendering, or output commit.
    Request,
}

/// Source of a soft resource allowance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceLimitSource {
    /// Product-selected allowance that a local caller may adapt once.
    Default,
    /// Caller-selected allowance that must never be raised implicitly.
    Explicit,
}

/// Facts required to safely recover at a transaction boundary.
#[derive(Debug, Clone, Copy)]
pub struct ResourceRecoveryBoundary<'a> {
    /// Scope in which the error occurred.
    pub scope: ResourceFailureScope,
    /// Smallest affected content unit.
    pub unit: ResourceUnitKind,
    /// Stable location for a nearby warning or placeholder.
    pub locator: Option<&'a SourceLocator>,
    /// Whether all uncommitted work and temporary resources were released.
    pub rollback_complete: bool,
    /// Whether useful native content, a visual, alt text, or a placeholder remains.
    pub fallback_retained: bool,
    /// Units durably completed before a sequence failure.
    pub committed_units: u64,
    /// Units omitted by the proposed action.
    pub omitted_units: u64,
    /// Whether the active allowance came from product defaults or the caller.
    pub limit_source: ResourceLimitSource,
    /// Exact preflight requirement, when known.
    pub precise_required: Option<u64>,
    /// Validated one-time raised allowance, when available.
    pub raised_limit: Option<u64>,
}

/// Bounded recovery action selected by [`classify_resource_recovery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceRecoveryAction {
    /// Retry once with an already validated soft allowance.
    RetryWithRaisedLimit {
        /// Validated soft allowance for the single retry.
        new_limit: u64,
    },
    /// Omit one independently recoverable unit.
    OmitUnit,
    /// Keep committed units and stop the remaining bounded sequence.
    TruncateSequence,
    /// Preserve terminal request semantics.
    Fail,
}

/// Classify a failure without parsing display text or inferring transaction
/// safety. Request-wide failures and failures without rollback stay terminal.
#[must_use]
pub fn classify_resource_recovery(
    policy: ErrorPolicy,
    error: &ConversionError,
    boundary: ResourceRecoveryBoundary<'_>,
) -> ResourceRecoveryAction {
    if policy != ErrorPolicy::BestEffort
        || boundary.scope == ResourceFailureScope::Request
        || !boundary.rollback_complete
    {
        return ResourceRecoveryAction::Fail;
    }

    let Some(limit) = recoverable_limit(error, boundary.scope) else {
        return ResourceRecoveryAction::Fail;
    };

    if boundary.limit_source == ResourceLimitSource::Default
        && let (Some(required), Some(raised)) = (boundary.precise_required, boundary.raised_limit)
        && required > 0
        && raised >= required
        && limit != "max_memory_bytes"
    {
        return ResourceRecoveryAction::RetryWithRaisedLimit { new_limit: raised };
    }

    if boundary.scope == ResourceFailureScope::Sequence
        && boundary.committed_units > 0
        && boundary.omitted_units > 0
        && boundary.locator.is_some()
    {
        return ResourceRecoveryAction::TruncateSequence;
    }

    if boundary.locator.is_some() && boundary.fallback_retained && boundary.omitted_units > 0 {
        return ResourceRecoveryAction::OmitUnit;
    }

    ResourceRecoveryAction::Fail
}

/// Resource identifier accepted at a local recovery boundary.
#[must_use]
pub fn recoverable_limit(
    error: &ConversionError,
    scope: ResourceFailureScope,
) -> Option<&'static str> {
    match error {
        ConversionError::OcrRecognitionMemory { .. }
            if matches!(scope, ResourceFailureScope::VisualRecognition) =>
        {
            Some("ocrRecognitionMemory")
        }
        ConversionError::ResourceLimit { limit, .. } => match (scope, *limit) {
            (
                ResourceFailureScope::VisualRecognition,
                "max_memory_bytes"
                | "ocrRecognitionMemory"
                | "recognitionMemory"
                | "recognitionCropMemory"
                | "recognitionOutputMemory"
                | "recognitionWidth"
                | "recognitionCropPixels"
                | "recognitionTensorElements"
                | "recognitionOutputElements"
                | "recognitionRegions"
                | "recognitionDecodedBytes"
                | "ocrWidthLimit"
                | "ocrPixelLimit"
                | "ocrTensorLimit"
                | "ocrStructureLimit",
            ) => Some(limit),
            (ResourceFailureScope::ContentUnit, "max_memory_bytes") => Some("max_memory_bytes"),
            (ResourceFailureScope::ContentUnit | ResourceFailureScope::Sequence, limit)
                if is_soft_sequence_limit(limit) =>
            {
                Some(limit)
            }
            _ => None,
        },
        _ => None,
    }
}

fn is_soft_sequence_limit(limit: &str) -> bool {
    matches!(
        limit,
        "max_pages"
            | "max_archive_entries"
            | "max_total_asset_bytes"
            | "max_asset_bytes"
            | "max_table_rows"
            | "max_table_columns"
            | "max_table_cells"
            | "max_decompressed_bytes"
            | "max_temp_bytes"
            | "max_temporary_bytes"
    )
}

/// Build a stable, localized diagnostic for a selected recovery action.
#[must_use]
pub fn recovery_diagnostic(
    error: &ConversionError,
    action: ResourceRecoveryAction,
    boundary: ResourceRecoveryBoundary<'_>,
    configured_limit: Option<u64>,
) -> Option<Diagnostic> {
    let limit = recoverable_limit(error, boundary.scope)?;
    let (suffix, severity, action_text) = match action {
        ResourceRecoveryAction::RetryWithRaisedLimit { new_limit } => {
            ("limitRaised", DiagnosticSeverity::Info, format!("raised to {new_limit}"))
        }
        ResourceRecoveryAction::OmitUnit => (
            "unitOmitted",
            DiagnosticSeverity::Warning,
            format!("omitted {} {}", boundary.omitted_units, boundary.unit.as_str()),
        ),
        ResourceRecoveryAction::TruncateSequence => (
            "sequenceTruncated",
            DiagnosticSeverity::Warning,
            format!(
                "kept {} units and omitted {} subsequent units",
                boundary.committed_units, boundary.omitted_units
            ),
        ),
        ResourceRecoveryAction::Fail => return None,
    };
    let configured = configured_limit.map_or_else(|| "unknown".into(), |value| value.to_string());
    let observed =
        boundary.precise_required.map_or_else(|| "unknown".into(), |value| value.to_string());
    Some(Diagnostic {
        code: format!("resource.{limit}.{suffix}"),
        severity,
        message: format!(
            "resource limit {limit}: configured={configured}, observed={observed}, action={action_text}"
        ),
        locator: boundary.locator.cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boundary(locator: &SourceLocator) -> ResourceRecoveryBoundary<'_> {
        ResourceRecoveryBoundary {
            scope: ResourceFailureScope::VisualRecognition,
            unit: ResourceUnitKind::Page,
            locator: Some(locator),
            rollback_complete: true,
            fallback_retained: true,
            committed_units: 0,
            omitted_units: 1,
            limit_source: ResourceLimitSource::Explicit,
            precise_required: Some(200),
            raised_limit: None,
        }
    }

    #[test]
    fn localized_ocr_memory_can_omit_a_page_in_best_effort() {
        let locator = SourceLocator { page: Some(9), ..SourceLocator::default() };
        let error =
            ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: "fixture".into() };
        let facts = boundary(&locator);
        assert_eq!(
            classify_resource_recovery(ErrorPolicy::BestEffort, &error, facts),
            ResourceRecoveryAction::OmitUnit
        );
        let diagnostic =
            recovery_diagnostic(&error, ResourceRecoveryAction::OmitUnit, facts, Some(100))
                .unwrap();
        assert_eq!(diagnostic.code, "resource.max_memory_bytes.unitOmitted");
        assert_eq!(diagnostic.locator.unwrap().page, Some(9));
        assert!(diagnostic.message.contains("configured=100"));
        assert!(diagnostic.message.contains("observed=200"));
    }

    #[test]
    fn strict_request_and_untyped_provider_failures_stay_terminal() {
        let locator = SourceLocator::default();
        let facts = boundary(&locator);
        let memory = ConversionError::OcrRecognitionMemory {
            provider: "fixture".into(),
            detail: "fixture".into(),
        };
        assert_eq!(
            classify_resource_recovery(ErrorPolicy::Strict, &memory, facts),
            ResourceRecoveryAction::Fail
        );
        let process = ConversionError::ProviderProcess {
            provider: "fixture".into(),
            detail: "fixture".into(),
        };
        assert_eq!(
            classify_resource_recovery(ErrorPolicy::BestEffort, &process, facts),
            ResourceRecoveryAction::Fail
        );
        let request = ResourceRecoveryBoundary { scope: ResourceFailureScope::Request, ..facts };
        assert_eq!(
            classify_resource_recovery(ErrorPolicy::BestEffort, &memory, request),
            ResourceRecoveryAction::Fail
        );
    }

    #[test]
    fn only_default_soft_limits_can_raise_once() {
        let locator = SourceLocator { part: Some("chapter/4".into()), ..Default::default() };
        let error = ConversionError::ResourceLimit { limit: "max_pages", detail: "fixture".into() };
        let facts = ResourceRecoveryBoundary {
            scope: ResourceFailureScope::Sequence,
            unit: ResourceUnitKind::Chapter,
            locator: Some(&locator),
            rollback_complete: true,
            fallback_retained: true,
            committed_units: 3,
            omitted_units: 2,
            limit_source: ResourceLimitSource::Default,
            precise_required: Some(5),
            raised_limit: Some(5),
        };
        assert_eq!(
            classify_resource_recovery(ErrorPolicy::BestEffort, &error, facts),
            ResourceRecoveryAction::RetryWithRaisedLimit { new_limit: 5 }
        );
        assert_eq!(
            classify_resource_recovery(
                ErrorPolicy::BestEffort,
                &error,
                ResourceRecoveryBoundary { limit_source: ResourceLimitSource::Explicit, ..facts }
            ),
            ResourceRecoveryAction::TruncateSequence
        );
    }

    #[test]
    fn committed_content_unit_memory_can_be_omitted_but_sequence_memory_stays_terminal() {
        let locator =
            SourceLocator { part: Some("attachment/image.png".into()), ..Default::default() };
        let error =
            ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: "fixture".into() };
        let facts = ResourceRecoveryBoundary {
            scope: ResourceFailureScope::ContentUnit,
            unit: ResourceUnitKind::Attachment,
            locator: Some(&locator),
            rollback_complete: true,
            fallback_retained: true,
            committed_units: 0,
            omitted_units: 1,
            limit_source: ResourceLimitSource::Explicit,
            precise_required: None,
            raised_limit: None,
        };
        assert_eq!(
            classify_resource_recovery(ErrorPolicy::BestEffort, &error, facts),
            ResourceRecoveryAction::OmitUnit
        );
        assert_eq!(
            classify_resource_recovery(
                ErrorPolicy::BestEffort,
                &error,
                ResourceRecoveryBoundary {
                    scope: ResourceFailureScope::Sequence,
                    committed_units: 1,
                    ..facts
                }
            ),
            ResourceRecoveryAction::Fail
        );
    }
}

use super::{
    non_linear_spine_diagnostic, omitted_resources_diagnostic, rights_metadata_diagnostic,
};
use into_markdown_core::{ConversionOutcome, DiagnosticSeverity, conversion_outcome};

#[test]
fn compatibility_audit_is_complete_but_omitted_chapter_is_degraded() {
    let compatibility = rights_metadata_diagnostic();
    assert_eq!(compatibility.severity, DiagnosticSeverity::Info);
    assert_eq!(conversion_outcome(&[compatibility]), ConversionOutcome::Complete);

    let omitted_chapter = non_linear_spine_diagnostic(1, "OPS/package.opf");
    assert_eq!(omitted_chapter.severity, DiagnosticSeverity::Warning);
    assert_eq!(conversion_outcome(&[omitted_chapter]), ConversionOutcome::Degraded);

    let omitted_resources = omitted_resources_diagnostic(1, 1, 1, "OPS/package.opf");
    assert_eq!(omitted_resources.severity, DiagnosticSeverity::Warning);
    assert_eq!(conversion_outcome(&[omitted_resources]), ConversionOutcome::Degraded);
}

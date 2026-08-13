use super::support::{NS, context_with, convert, package};
use crate::odf::convert_odf;
use crate::odf::package::Package;
use into_markdown_core::{
    CancellationToken, ConversionError, ConversionOptions, ExecutionContext, ExecutionOptions,
    InputFormat, ResourceLimits, estimate_retained_output,
};

#[test]
fn cancellation_and_retained_memory_exact_boundary_are_enforced() {
    let content = format!(
        "<office:document-content {NS}><office:body><office:text><text:p>bounded</text:p></office:text></office:body></office:document-content>"
    );
    let bytes = package(InputFormat::Odt, &content, &[]);
    let output = convert(&bytes, InputFormat::Odt, ResourceLimits::default()).unwrap();
    let retained =
        estimate_retained_output(&output.document, &output.assets, &output.diagnostics).unwrap();
    drop(output);
    let inspection = context_with(ResourceLimits::default());
    let package_peak = Package::open(
        &bytes,
        InputFormat::Odt,
        &ConversionOptions::default(),
        &inspection,
        inspection.available_memory_bytes(),
    )
    .unwrap()
    .logical_peak;
    let peak = package_peak.max(retained);
    let options = ConversionOptions {
        limits: ResourceLimits { max_memory_bytes: peak, ..ResourceLimits::default() },
        ..ConversionOptions::default()
    };
    let exact = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let exact_output = convert_odf(&bytes, InputFormat::Odt, &options, &exact).unwrap();
    assert_eq!(exact.available_memory_bytes(), peak - retained);
    drop(exact_output);
    assert_eq!(exact.available_memory_bytes(), peak);
    let low_options = ConversionOptions {
        limits: ResourceLimits { max_memory_bytes: peak - 1, ..ResourceLimits::default() },
        ..ConversionOptions::default()
    };
    let low = ExecutionContext::new(ExecutionOptions::default(), low_options.limits.clone());
    assert!(matches!(
        convert_odf(&bytes, InputFormat::Odt, &low_options, &low),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));

    let token = CancellationToken::new();
    token.cancel();
    let cancelled = ExecutionContext::new(
        ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    assert!(matches!(
        convert_odf(&bytes, InputFormat::Odt, &ConversionOptions::default(), &cancelled),
        Err(ConversionError::Cancelled)
    ));

    let timed_out = ExecutionContext::new(
        ExecutionOptions {
            timeout: Some(std::time::Duration::ZERO),
            ..ExecutionOptions::default()
        },
        ResourceLimits::default(),
    );
    assert!(matches!(
        convert_odf(&bytes, InputFormat::Odt, &ConversionOptions::default(), &timed_out),
        Err(ConversionError::Timeout)
    ));
}

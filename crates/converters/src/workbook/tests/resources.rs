use super::support::{context, image_xlsx, limited_context, stale_dimension_xlsb};
use crate::workbook::calamine_adapter::convert_xlsb;
use crate::workbook::orchestrator::convert_workbook;
use crate::workbook::preflight::preflight_package;
use base64::Engine as _;
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};

#[test]
fn cancellation_after_large_xlsb_preflight_stops_at_calamine_boundary() {
    let bytes = stale_dimension_xlsb();
    let options = ConversionOptions::default();
    let cancellation = into_markdown_core::CancellationToken::new();
    let root = ExecutionContext::new(
        into_markdown_core::ExecutionOptions {
            cancellation: cancellation.clone(),
            ..into_markdown_core::ExecutionOptions::default()
        },
        options.limits.clone(),
    );
    let available = root.available_memory_bytes();
    let mut parent = root.reserve_memory(available).unwrap();
    let credit = root.with_memory_credit(&mut parent).unwrap();
    let (preflight, permit) =
        preflight_package(&bytes, &options, &credit, credit.available_memory_bytes()).unwrap();
    assert_eq!(preflight.inventory.xlsb_formula_preallocation_cells, 1_000_000);
    cancellation.cancel();
    assert!(matches!(
        convert_xlsb(
            &bytes,
            &preflight.sheet_parts,
            &preflight.sheet_bounds,
            &preflight.extras,
            &options,
            &credit,
        ),
        Err(ConversionError::Cancelled)
    ));
    drop(permit);
    drop(credit);
    drop(parent);
    assert_eq!(root.reserved_memory_bytes(), 0);
}

#[test]
fn authenticated_workbook_credit_is_exact_and_recovers_on_error_and_unwind() {
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .unwrap();
    let bytes = image_xlsx(&png);
    let mut corrupt_png = png.clone();
    let last = corrupt_png.len() - 1;
    corrupt_png[last] ^= 1;
    let corrupt = image_xlsx(&corrupt_png);
    let options = ConversionOptions::default();
    let analysis = context();
    let available = analysis.available_memory_bytes();
    let mut analysis_parent = analysis.reserve_memory(available).unwrap();
    let peak = {
        let credit = analysis.with_memory_credit(&mut analysis_parent).unwrap();
        preflight_package(&bytes, &options, &credit, available).unwrap().0.memory_peak
    };
    drop(analysis_parent);
    assert_eq!(analysis.reserved_memory_bytes(), 0);

    let exact = limited_context(peak);
    let mut parent = exact.reserve_memory(peak).unwrap();
    {
        let credit = exact.with_memory_credit(&mut parent).unwrap();
        let output = convert_workbook(&bytes, &options, &credit).unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(credit.reserved_memory_bytes(), 0, "sequential image scopes leaked credit");
    }
    assert_eq!(exact.reserved_memory_bytes(), peak);
    drop(parent);
    assert_eq!(exact.reserved_memory_bytes(), 0);

    let low = limited_context(peak - 1);
    let mut low_parent = low.reserve_memory(peak - 1).unwrap();
    {
        let credit = low.with_memory_credit(&mut low_parent).unwrap();
        assert!(matches!(
            convert_workbook(&bytes, &options, &credit),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        assert!(matches!(
            convert_workbook(&corrupt, &options, &credit),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        assert_eq!(credit.reserved_memory_bytes(), 0);
    }
    drop(low_parent);
    assert_eq!(low.reserved_memory_bytes(), 0);

    let error_context = context();
    let error_plan = error_context.available_memory_bytes();
    let mut error_parent = error_context.reserve_memory(error_plan).unwrap();
    {
        let credit = error_context.with_memory_credit(&mut error_parent).unwrap();
        assert!(matches!(
            convert_workbook(&corrupt, &options, &credit),
            Err(ConversionError::Malformed { .. })
        ));
        assert_eq!(credit.reserved_memory_bytes(), 0);
    }
    drop(error_parent);
    assert_eq!(error_context.reserved_memory_bytes(), 0);

    let unwind_context = limited_context(peak);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut unwind_parent = unwind_context.reserve_memory(peak).unwrap();
        let credit = unwind_context.with_memory_credit(&mut unwind_parent).unwrap();
        let _ = convert_workbook(&bytes, &options, &credit).unwrap();
        panic!("exercise workbook post-conversion unwind");
    }));
    assert!(result.is_err());
    assert_eq!(unwind_context.reserved_memory_bytes(), 0);
}

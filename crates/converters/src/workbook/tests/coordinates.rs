use super::support::context;
use crate::workbook::calamine_adapter::assert_range_indexes_for_test;
use crate::workbook::cell::{cell_name, parse_cell_ref};
use into_markdown_core::ExecutionContext;

#[test]
fn cell_coordinates_round_trip() {
    assert_eq!(parse_cell_ref("A1").unwrap(), (0, 0));
    assert_eq!(parse_cell_ref("$XFD$1048576").unwrap(), (1_048_575, 16_383));
    assert_eq!(cell_name(0, 0), "A1");
    assert_eq!(cell_name(1_048_575, 16_383), "XFD1048576");
    assert!(parse_cell_ref("XFE1").is_err());
    assert!(parse_cell_ref("A0").is_err());
}

#[test]
fn merge_and_hyperlink_sweeps_are_range_bounded_and_cancellable() {
    let context = context();
    let cancellation = into_markdown_core::CancellationToken::new();
    cancellation.cancel();
    let cancelled = ExecutionContext::new(
        into_markdown_core::ExecutionOptions {
            cancellation,
            ..into_markdown_core::ExecutionOptions::default()
        },
        into_markdown_core::ResourceLimits::default(),
    );
    assert_range_indexes_for_test(&context, &cancelled);
}

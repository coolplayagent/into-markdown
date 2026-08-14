use super::{HyperlinkIndex, MergeIndex};
use crate::workbook::model::Hyperlink;
use calamine::Dimensions;
use into_markdown_core::{ConversionError, ExecutionContext};

pub(super) fn assert_range_indexes(context: &ExecutionContext, cancelled: &ExecutionContext) {
    let huge = Dimensions { start: (0, 0), end: (1_048_575, 16_383) };
    let mut merges = MergeIndex::new(&[huge], huge.end.0, huge.end.1, context).unwrap();
    assert_eq!(merges.starts.values().map(Vec::len).sum::<usize>(), 1);
    assert_eq!(merges.ends.values().map(Vec::len).sum::<usize>(), 1);
    merges.prepare_row(0, context).unwrap();
    assert_eq!(merges.at(0, 0).unwrap().end, huge.end);
    assert_eq!(merges.at(500_000, 8_000).unwrap().start, huge.start);

    let overlapping =
        [Dimensions { start: (0, 0), end: (3, 3) }, Dimensions { start: (2, 2), end: (4, 4) }];
    let mut merges = MergeIndex::new(&overlapping, 4, 4, context).unwrap();
    merges.prepare_row(0, context).unwrap();
    assert!(matches!(merges.prepare_row(2, context), Err(ConversionError::Malformed { .. })));

    let links = vec![Hyperlink {
        start: (0, 0),
        end: (1_048_575, 16_383),
        target: "https://example.invalid".into(),
        label: None,
    }];
    let mut links = HyperlinkIndex::new(&links, context).unwrap();
    assert_eq!(links.starts.values().map(Vec::len).sum::<usize>(), 1);
    links.prepare_row(0, context).unwrap();
    assert_eq!(links.at(16_383).unwrap().target, "https://example.invalid");

    assert!(matches!(
        MergeIndex::new(&[huge], huge.end.0, huge.end.1, cancelled),
        Err(ConversionError::Cancelled)
    ));
}

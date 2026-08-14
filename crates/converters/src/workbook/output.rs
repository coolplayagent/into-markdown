use crate::workbook::PROVIDER_ID;
use calamine::Data;
use into_markdown_core::{CellRef, NodeId, Provenance, ProvenanceKind, SourceLocator};
use std::borrow::Cow;

pub(super) fn provenance(sheet: &str, row: Option<u32>, column: Option<u32>) -> Provenance {
    Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: PROVIDER_ID.into(),
        locator: SourceLocator {
            sheet: Some(sheet.into()),
            cell: row.zip(column).map(|(row, column)| CellRef { row, column }),
            ..SourceLocator::default()
        },
        confidence: Some(1.0),
    }
}

pub(super) fn data_text(value: &Data) -> Cow<'_, str> {
    match value {
        Data::Int(value) => Cow::Owned(value.to_string()),
        Data::Float(value) => Cow::Owned(value.to_string()),
        Data::String(value) | Data::DateTimeIso(value) | Data::DurationIso(value) => {
            Cow::Borrowed(value)
        }
        Data::Bool(value) => Cow::Owned(value.to_string()),
        Data::DateTime(value) => Cow::Owned(
            value.as_datetime().map_or_else(|| value.to_string(), |value| value.to_string()),
        ),
        Data::Error(value) => Cow::Owned(format!("#{value:?}")),
        Data::Empty => Cow::Borrowed(""),
    }
}

pub(super) fn stable_id(prefix: &str, sheet_index: usize, row: u32, column: u32) -> NodeId {
    NodeId(format!("workbook-{prefix}-{sheet_index}-{row}-{column}"))
}

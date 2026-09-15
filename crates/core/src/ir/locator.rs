use super::{CellRef, IrError, IrErrorCode, Rect, TimeRange};
use serde::{Deserialize, Serialize};

/// Location of extracted content in the source.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceLocator {
    /// Inclusive byte offset in the original encoded source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_start: Option<u64>,
    /// Exclusive byte offset in the original encoded source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_end: Option<u64>,
    /// One-based page number.
    pub page: Option<u32>,
    /// One-based slide number.
    pub slide: Option<u32>,
    /// Worksheet name.
    pub sheet: Option<String>,
    /// Spreadsheet cell.
    pub cell: Option<CellRef>,
    /// Bounding rectangle.
    pub bounds: Option<Rect>,
    /// Zero-based source character index, when this node represents one PDF
    /// character or another independently addressable text unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character_index: Option<u32>,
    /// Best-effort source font name. This is a clue, not a trusted identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_name: Option<String>,
    /// Source font size in source coordinate units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// Source text baseline on the minor axis of the displayed text direction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_baseline: Option<f32>,
    /// Clockwise source rotation in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_degrees: Option<f32>,
    /// Page width in source coordinate units, when the locator addresses a page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_width: Option<f32>,
    /// Page height in source coordinate units, when the locator addresses a page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_height: Option<f32>,
    /// Media time range.
    pub time: Option<TimeRange>,
    /// Safe container-relative part name, using `/` separators.
    pub part: Option<String>,
}

pub(super) fn validate_geometry(locator: &SourceLocator, path: &str) -> Result<(), IrError> {
    for (field, value) in [
        ("fontSize", locator.font_size),
        ("textBaseline", locator.text_baseline),
        ("rotationDegrees", locator.rotation_degrees),
        ("pageWidth", locator.page_width),
        ("pageHeight", locator.page_height),
    ] {
        if value.is_some_and(|value| !value.is_finite()) {
            return Err(IrError::new(
                IrErrorCode::InvalidLocator,
                format!("{path}.{field}"),
                "source geometry must be finite",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_baselines_round_trip_and_reject_nonfinite_coordinates() {
        let value = SourceLocator { text_baseline: Some(123.5), ..Default::default() };
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["textBaseline"], 123.5);
        assert_eq!(serde_json::from_value::<SourceLocator>(json).unwrap(), value);
        let mut legacy = serde_json::to_value(SourceLocator::default()).unwrap();
        legacy.as_object_mut().unwrap().remove("textBaseline");
        assert_eq!(serde_json::from_value::<SourceLocator>(legacy).unwrap().text_baseline, None);
        for baseline in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(
                validate_geometry(
                    &SourceLocator { text_baseline: Some(baseline), ..Default::default() },
                    "source"
                )
                .is_err()
            );
        }
    }
}

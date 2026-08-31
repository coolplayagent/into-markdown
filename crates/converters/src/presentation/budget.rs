//! Central numeric limits and conservative allocation charges for `PresentationML` parsing.

pub(super) const MAX_GEOMETRY_COMPARISONS: usize = 5_000_000;
pub(super) const MAX_ASSET_DIGEST_CANDIDATES: usize = 64;
pub(super) const ASSET_HASH_CHUNK_BYTES: usize = 64 * 1024;
pub(super) const ZIP_READ_CHUNK_BYTES: usize = 8 * 1024;
pub(super) const ASSET_INDEX_ENTRY_CHARGE: u64 = 512;
pub(super) const MAX_EXACT_EMU: i64 = 9_007_199_254_740_991;
pub(super) const PRESENTATION_ALLOCATION_BASE: u64 = 256 * 1024;
pub(super) const ZIP_METADATA_BYTES_PER_ENTRY: u64 = 384;

/// One logical scan of a complete XML part, including uninterpreted MCE branches.
pub(super) struct XmlEventBudget {
    events: u64,
    maximum: u64,
}

impl XmlEventBudget {
    pub(super) fn new(
        maximum: u64,
        part: &str,
    ) -> Result<Self, into_markdown_core::ConversionError> {
        if maximum == 0 {
            return Err(super::error::limit(
                "max_presentation_xml_events",
                format!("XML part {part}: non-EOF event budget must be greater than zero"),
            ));
        }
        Ok(Self { events: 0, maximum })
    }

    pub(super) fn charge(&mut self, part: &str) -> Result<(), into_markdown_core::ConversionError> {
        self.events = self.events.checked_add(1).ok_or_else(|| {
            super::error::limit(
                "max_presentation_xml_events",
                format!("XML part {part}: non-EOF event count overflow"),
            )
        })?;
        if self.events > self.maximum {
            return Err(super::error::limit(
                "max_presentation_xml_events",
                format!("XML part {part}: {} non-EOF events > {}", self.events, self.maximum),
            ));
        }
        Ok(())
    }
}

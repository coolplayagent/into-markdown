//! Central numeric limits and conservative allocation charges for `PresentationML` parsing.

pub(super) const MAX_XML_WIDTH: usize = 100_000;
pub(super) const MAX_XML_EVENTS: usize = 2_000_000;
pub(super) const MAX_GEOMETRY_COMPARISONS: usize = 5_000_000;
pub(super) const MAX_ASSET_DIGEST_CANDIDATES: usize = 64;
pub(super) const ASSET_HASH_CHUNK_BYTES: usize = 64 * 1024;
pub(super) const ZIP_READ_CHUNK_BYTES: usize = 8 * 1024;
pub(super) const ASSET_INDEX_ENTRY_CHARGE: u64 = 512;
pub(super) const MAX_EXACT_EMU: i64 = 9_007_199_254_740_991;
pub(super) const PRESENTATION_ALLOCATION_BASE: u64 = 256 * 1024;
pub(super) const ZIP_METADATA_BYTES_PER_ENTRY: u64 = 384;

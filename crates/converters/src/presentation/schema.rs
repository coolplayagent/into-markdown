use into_markdown_core::InputFormat;

pub(super) const FORMATS: &[InputFormat] = &[InputFormat::Pptx];
pub(super) const PROVIDER_ID: &str = "builtin.converter.presentationml";
pub(super) const P_NS: &[u8] = b"http://schemas.openxmlformats.org/presentationml/2006/main";
pub(super) const A_NS: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/main";
pub(super) const C_NS: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/chart";
pub(super) const M_NS: &[u8] = b"http://schemas.openxmlformats.org/officeDocument/2006/math";
pub(super) const MC_NS: &[u8] = b"http://schemas.openxmlformats.org/markup-compatibility/2006";
pub(super) const R_NS: &[u8] =
    b"http://schemas.openxmlformats.org/officeDocument/2006/relationships";
pub(super) const REL_NS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/relationships";
pub(super) const TYPES_NS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/content-types";
pub(super) const OFFICE_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
#[cfg(test)]
pub(super) const REL_PREFIX: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";
pub(super) const SLIDE_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";
pub(super) const LAYOUT_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout";
pub(super) const MASTER_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster";
pub(super) const THEME_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme";
pub(super) const NOTES_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide";
pub(super) const IMAGE_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
pub(super) const CHART_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart";
pub(super) const RELATIONSHIPS_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-package.relationships+xml";
pub(super) const SEEN_TRANSFORM: u8 = 1 << 0;
pub(super) const SEEN_OFFSET: u8 = 1 << 1;
pub(super) const SEEN_EXTENT: u8 = 1 << 2;
pub(super) const SEEN_CHILD_OFFSET: u8 = 1 << 3;
pub(super) const SEEN_CHILD_EXTENT: u8 = 1 << 4;
pub(super) const SEEN_PLACEHOLDER: u8 = 1 << 3;
pub(super) const SEEN_TABLE: u8 = 1 << 4;
pub(super) const EXPLICIT_LIST_LEVEL: u8 = 1 << 0;
pub(super) const EXPLICIT_BULLET: u8 = 1 << 1;
pub(super) const GEOMETRY_OFFSET: u8 = 1 << 0;
pub(super) const GEOMETRY_EXTENT: u8 = 1 << 1;
pub(super) const GEOMETRY_ROTATION: u8 = 1 << 2;
pub(super) const GEOMETRY_FLIP_H: u8 = 1 << 3;
pub(super) const GEOMETRY_FLIP_V: u8 = 1 << 4;
pub(super) const COMPOUND_FILE_SIGNATURE: &[u8; 8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";

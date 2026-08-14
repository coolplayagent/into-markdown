//! Workbook format constants shared by the strict package scanners.

pub(super) const CONTENT_TYPES_NS: &[u8] =
    b"http://schemas.openxmlformats.org/package/2006/content-types";
pub(super) const PACKAGE_REL_NS: &[u8] =
    b"http://schemas.openxmlformats.org/package/2006/relationships";
pub(super) const PACKAGE_REL_CT: &str = "application/vnd.openxmlformats-package.relationships+xml";
pub(super) const SPREADSHEET_NS: &[u8] =
    b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";
pub(super) const SPREADSHEET_STRICT_NS: &[u8] = b"http://purl.oclc.org/ooxml/spreadsheetml/main";
pub(super) const OFFICE_REL_NS: &[u8] =
    b"http://schemas.openxmlformats.org/officeDocument/2006/relationships";
pub(super) const OFFICE_REL_STRICT_NS: &[u8] =
    b"http://purl.oclc.org/ooxml/officeDocument/relationships";
pub(super) const SPREADSHEET_DRAWING_NS: &[u8] =
    b"http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing";
pub(super) const DRAWINGML_NS: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/main";
pub(super) const CHART_NS: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/chart";
pub(super) const SPREADSHEET_MAIN: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml";
pub(super) const SPREADSHEET_MACRO_MAIN: &str =
    "application/vnd.ms-excel.sheet.macroEnabled.main+xml";
pub(super) const SPREADSHEET_BINARY_MAIN: &str =
    "application/vnd.ms-excel.sheet.binary.macroEnabled.main";
pub(super) const ROOT_OFFICE_DOCUMENT: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
pub(super) const ROOT_OFFICE_DOCUMENT_STRICT: &str =
    "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument";
pub(super) const XML_WORKSHEET_CT: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml";
pub(super) const XLSB_WORKSHEET_CT: &str = "application/vnd.ms-excel.worksheet";
pub(super) const XML_CHARTSHEET_CT: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.chartsheet+xml";
pub(super) const XLSB_CHARTSHEET_CT: &str = "application/vnd.ms-excel.chartsheet";
pub(super) const XML_DIALOGSHEET_CT: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.dialogsheet+xml";
pub(super) const XLSB_DIALOGSHEET_CT: &str = "application/vnd.ms-excel.dialogsheet";
pub(super) const XML_MACROSHEET_CT: &str = "application/vnd.ms-excel.macrosheet+xml";
pub(super) const XLSB_MACROSHEET_CT: &str = "application/vnd.ms-excel.macrosheet";
pub(super) const XML_STYLES_CT: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml";
pub(super) const XLSB_STYLES_CT: &str = "application/vnd.ms-excel.styles";
pub(super) const XML_SHARED_STRINGS_CT: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml";
pub(super) const XLSB_SHARED_STRINGS_CT: &str = "application/vnd.ms-excel.sharedStrings";
pub(super) const XML_COMMENTS_CT: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.comments+xml";
pub(super) const XLSB_COMMENTS_CT: &str = "application/vnd.ms-excel.comments";
pub(super) const DRAWING_CT: &str = "application/vnd.openxmlformats-officedocument.drawing+xml";
pub(super) const CHART_CT: &str =
    "application/vnd.openxmlformats-officedocument.drawingml.chart+xml";
pub(super) const MAX_EXCEL_ROWS: u32 = 1_048_576;
pub(super) const MAX_EXCEL_COLUMNS: u32 = 16_384;

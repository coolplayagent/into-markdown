use crate::workbook::error::limit;
use into_markdown_core::{Asset, AssetId, ConversionError, Diagnostic, InlineMark};
use std::collections::BTreeMap;

pub(super) type CellCoordinate = (u32, u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkbookKind {
    Xml,
    Binary,
}

#[derive(Debug)]
pub(super) struct PackagePreflight {
    pub(super) kind: WorkbookKind,
    pub(super) macro_present: bool,
    pub(super) media_bytes: u64,
    pub(super) sheet_parts: BTreeMap<String, String>,
    /// Logical worksheet order from workbook.xml. ZIP and relationship order
    /// are not presentation order.
    pub(super) sheet_order: Vec<String>,
    /// Authenticated `SpreadsheetML` sheets and their physical cell counts.
    /// The native adapter uses physical records rather than the untrusted
    /// worksheet dimension to plan bounded regions.
    pub(super) xml_sheets: BTreeMap<String, (String, u64)>,
    pub(super) xml_layouts: BTreeMap<String, crate::workbook::xlsx::sheet_index::SheetLayout>,
    pub(super) xml_regions: BTreeMap<String, Vec<crate::workbook::xlsx::regions::SparseRegion>>,
    pub(super) sheet_bounds: BTreeMap<String, CellCoordinate>,
    pub(super) extras: BTreeMap<String, SheetExtras>,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) memory_peak: u64,
    pub(super) inventory: WorkbookInventory,
    pub(super) assets: Vec<Asset>,
}

#[derive(Debug, Default)]
pub(super) struct WorkbookInventory {
    pub(super) cells: u64,
    pub(super) formulas: u64,
    pub(super) formula_bytes: u64,
    pub(super) max_formula_bytes: u64,
    pub(super) shared_strings: u64,
    pub(super) shared_string_bytes: u64,
    pub(super) max_shared_string_bytes: u64,
    pub(super) max_shared_string_index: Option<u64>,
    pub(super) styles: u64,
    pub(super) fonts: u64,
    pub(super) number_formats: u64,
    pub(super) style_format_bytes: u64,
    pub(super) max_style_index: Option<u64>,
    pub(super) cell_value_bytes: u64,
    pub(super) max_cell_value_bytes: u64,
    pub(super) merge_ranges: u64,
    pub(super) hyperlink_ranges: u64,
    pub(super) record_bytes: u64,
    pub(super) defined_names: u64,
    pub(super) defined_name_bytes: u64,
    pub(super) external_sheet_slots: u64,
    pub(super) max_formula_reference_bytes: u64,
    pub(super) shared_formula_slots: u64,
    pub(super) xlsb_formula_preallocation_cells: u64,
}

impl WorkbookInventory {
    pub(super) fn text_bytes(&self) -> Result<u64, ConversionError> {
        self.formula_bytes
            .checked_add(self.shared_string_bytes)
            .and_then(|value| value.checked_add(self.style_format_bytes))
            .and_then(|value| value.checked_add(self.defined_name_bytes))
            .and_then(|value| value.checked_add(self.cell_value_bytes))
            .ok_or_else(|| limit("max_memory_bytes", "workbook text budget overflow"))
    }

    pub(super) fn formula_materialized_bytes(&self) -> Result<u64, ConversionError> {
        self.formulas
            .checked_mul(self.max_formula_bytes)
            .and_then(|value| value.checked_mul(64))
            .and_then(|value| {
                self.formula_bytes
                    .checked_mul(self.max_formula_reference_bytes)
                    .and_then(|references| value.checked_add(references))
            })
            .ok_or_else(|| limit("max_memory_bytes", "formula materialization model overflow"))
    }
}

pub(super) fn max_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (value @ Some(_), None) | (None, value @ Some(_)) => value,
        (None, None) => None,
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct BinaryFormulaContext {
    pub(super) external_sheets: usize,
    pub(super) defined_names: usize,
}

#[derive(Debug, Clone)]
pub(super) struct BinaryHyperlink {
    pub(super) start: CellCoordinate,
    pub(super) end: CellCoordinate,
    pub(super) relationship_id: Option<String>,
    pub(super) location: String,
    pub(super) tooltip: String,
    pub(super) display: String,
}

#[derive(Debug)]
pub(super) struct WorkbookParts {
    pub(super) sheets: BTreeMap<String, String>,
    pub(super) sheet_order: Vec<String>,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) inventory: WorkbookInventory,
    pub(super) binary_formula_context: BinaryFormulaContext,
}

#[derive(Debug, Clone)]
pub(super) struct Hyperlink {
    pub(super) start: (u32, u32),
    pub(super) end: (u32, u32),
    pub(super) target: String,
    pub(super) label: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct Annotation {
    pub(super) cell: (u32, u32),
    pub(super) text: String,
    pub(super) author: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ImageAnchor {
    pub(super) cell: CellCoordinate,
    pub(super) end: CellCoordinate,
    pub(super) asset: AssetId,
    pub(super) alt: Option<String>,
    /// `DrawingML` part containing the physical cell anchor and r:id.
    pub(super) part: String,
    pub(super) target: String,
    pub(super) relationship_id: String,
}

#[derive(Debug, Clone)]
pub(super) struct ChartTitle {
    pub(super) cell: CellCoordinate,
    pub(super) end: CellCoordinate,
    pub(super) title: String,
    /// `DrawingML` part containing the physical cell anchor and r:id.
    pub(super) part: String,
    pub(super) target: String,
    pub(super) relationship_id: String,
}

#[derive(Debug, Clone, Default)]
pub(super) struct SheetExtras {
    pub(super) hyperlinks: Vec<Hyperlink>,
    pub(super) annotations: Vec<Annotation>,
    pub(super) chart_titles: Vec<ChartTitle>,
    pub(super) images: Vec<ImageAnchor>,
    pub(super) cell_marks: BTreeMap<CellCoordinate, Vec<InlineMark>>,
    pub(super) hidden_rows: Vec<(u32, u32)>,
    pub(super) hidden_columns: Vec<(u32, u32)>,
}

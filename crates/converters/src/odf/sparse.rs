use super::model::{ParseState, TABLE_NS, limit, part_locator};
use super::tables::parse_repeat;
use super::xml::{XmlContent, XmlNode};
use into_markdown_core::{ConversionError, ConversionOptions};

/// Only a terminal run of contentless, unmerged rows is padding. Interior gaps,
/// values, formulas, covered cells and row spans still follow the normal counters.
pub(super) fn trim_empty_tail(
    node: &mut XmlNode,
    options: &ConversionOptions,
    state: &mut ParseState,
) -> Result<(), ConversionError> {
    if !node.is(TABLE_NS, "table") {
        return Ok(());
    }
    let mut end = node.content.len();
    let mut skipped = 0_u64;
    for value in node.content.iter().rev() {
        let XmlContent::Node(row) = value else {
            if matches!(value, XmlContent::Text(text) if text.trim().is_empty()) {
                end -= 1;
                continue;
            }
            break;
        };
        if !pure_padding(row) {
            break;
        }
        let count = parse_repeat(
            row.attr(TABLE_NS, "number-rows-repeated"),
            "table:number-rows-repeated",
            u64::from(u32::MAX),
        )?;
        let mut width = 0_u64;
        for cell in row.children() {
            width = width
                .checked_add(parse_repeat(
                    cell.attr(TABLE_NS, "number-columns-repeated"),
                    "table:number-columns-repeated",
                    options.limits.max_table_columns,
                )?)
                .ok_or_else(|| limit("max_table_columns", "empty padding width overflow"))?;
        }
        if width > options.limits.max_table_columns {
            return Err(limit("max_table_columns", "empty padding exceeds logical column limit"));
        }
        skipped = skipped
            .checked_add(count)
            .filter(|total| u32::try_from(*total).is_ok())
            .ok_or_else(|| limit("max_table_rows", "empty padding row coordinate overflow"))?;
        end -= 1;
    }
    if skipped != 0 {
        node.content.truncate(end);
        state.warning("odf.emptyRowPadding", format!("{skipped} trailing empty padding rows not materialized; valued/formula/merged rows remain resource-counted"), part_locator("content.xml"));
    }
    Ok(())
}

fn pure_padding(row: &XmlNode) -> bool {
    row.is(TABLE_NS, "table-row")
        && row.attrs.iter().all(|attr| {
            attr.name.ns == TABLE_NS
                && matches!(attr.name.local.as_str(), "style-name" | "number-rows-repeated")
        })
        && row.content.iter().all(|value| match value {
            XmlContent::Text(text) => text.trim().is_empty(),
            XmlContent::Node(cell) => {
                cell.is(TABLE_NS, "table-cell")
                    && cell.content.is_empty()
                    && cell.attrs.iter().all(|attr| {
                        attr.name.ns == TABLE_NS
                            && matches!(
                                attr.name.local.as_str(),
                                "style-name" | "number-columns-repeated"
                            )
                    })
            }
        })
}

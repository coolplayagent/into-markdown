use crate::odf::model::{DRAW_NS, OFFICE_NS, TABLE_NS, TEXT_NS, limit, malformed};
use crate::odf::xml::XmlNode;
use into_markdown_core::{ConversionError, ConversionOptions};

pub(super) fn cell_has_content(node: &XmlNode) -> bool {
    node.children().any(|child| {
        child.is(TEXT_NS, "p")
            || child.is(TEXT_NS, "h")
            || child.is(TEXT_NS, "list")
            || child.is(TABLE_NS, "table")
            || child.is(DRAW_NS, "frame")
    })
}

#[derive(Clone, Debug, Default)]
pub(super) struct CellSemanticValue {
    pub(super) cached: Option<String>,
    pub(super) formula: Option<String>,
    pub(super) formula_language: Option<String>,
}

pub(super) fn cell_semantic_value(
    node: &XmlNode,
    options: &ConversionOptions,
) -> Result<CellSemanticValue, ConversionError> {
    let value_type = node.attr(OFFICE_NS, "value-type");
    let candidates = [
        ("string-value", node.attr(OFFICE_NS, "string-value")),
        ("value", node.attr(OFFICE_NS, "value")),
        ("date-value", node.attr(OFFICE_NS, "date-value")),
        ("time-value", node.attr(OFFICE_NS, "time-value")),
        ("boolean-value", node.attr(OFFICE_NS, "boolean-value")),
    ];
    let present: Vec<_> = candidates.iter().filter(|(_, value)| value.is_some()).collect();
    let formula = node.attr(TABLE_NS, "formula");
    if value_type.is_none() {
        if present.is_empty() && formula.is_none() {
            return Ok(CellSemanticValue::default());
        }
        return Err(malformed(
            Some("content.xml"),
            "typed or formula cell is missing office:value-type",
        ));
    }
    let value_type = value_type.unwrap_or_default();
    let (expected_attr, require_value) = match value_type {
        "string" => (Some("string-value"), false),
        "float" | "percentage" => (Some("value"), true),
        "currency" => {
            if node.attr(OFFICE_NS, "currency").is_none() {
                return Err(malformed(
                    Some("content.xml"),
                    "currency cell is missing office:currency",
                ));
            }
            (Some("value"), true)
        }
        "boolean" => (Some("boolean-value"), true),
        "date" => (Some("date-value"), true),
        "time" => (Some("time-value"), true),
        "void" => (None, false),
        _ => {
            return Err(malformed(
                Some("content.xml"),
                format!("unsupported office:value-type {value_type}"),
            ));
        }
    };
    if present.iter().any(|(name, _)| Some(*name) != expected_attr)
        || require_value && !present.iter().any(|(name, _)| Some(*name) == expected_attr)
        || expected_attr.is_none() && !present.is_empty()
        || value_type != "currency" && node.attr(OFFICE_NS, "currency").is_some()
    {
        return Err(malformed(
            Some("content.xml"),
            "office:value-type disagrees with its cached value attribute",
        ));
    }
    let cached = expected_attr.and_then(|expected| {
        candidates
            .iter()
            .find(|(name, _)| *name == expected)
            .and_then(|(_, value)| value.map(str::to_owned))
    });
    let mut formula_language = None;
    let formula = formula
        .map(|value| {
            if !value.starts_with("of:=")
                && value.split_once(":=").is_some_and(|(_, expression)| !expression.is_empty())
            {
                // xml_node binds this prefix to the producer formula namespace before here.
                super::recovery::require_best_effort(
                    options,
                    "content.xml",
                    "producer formula retained as inert source, without evaluation",
                )?;
                return Ok(value.to_owned());
            }
            formula_language = Some("openformula".into());
            value
                .strip_prefix("of:=")
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    malformed(
                        Some("content.xml"),
                        "formula must be a non-empty OpenFormula of:= expression",
                    )
                })
        })
        .transpose()?;
    Ok(CellSemanticValue { cached, formula, formula_language })
}

pub(super) fn parse_repeat(
    value: Option<&str>,
    name: &str,
    maximum: u64,
) -> Result<u64, ConversionError> {
    let value = value
        .unwrap_or("1")
        .parse::<u64>()
        .map_err(|_| malformed(Some("content.xml"), format!("invalid {name}")))?;
    if value == 0 {
        return Err(malformed(Some("content.xml"), format!("{name} must be positive")));
    }
    if value > maximum {
        return Err(limit(
            match name {
                "table:number-rows-repeated" => "max_table_rows",
                "table:number-columns-repeated" => "max_table_columns",
                _ => "max_field_bytes",
            },
            format!("{name} {value} > {maximum}"),
        ));
    }
    Ok(value)
}

pub(super) fn parse_odf_bool(value: Option<&str>, name: &str) -> Result<bool, ConversionError> {
    match value {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(malformed(Some("content.xml"), format!("invalid boolean {name}"))),
    }
}

pub(super) fn parse_span(value: Option<&str>, name: &str) -> Result<u32, ConversionError> {
    let value = value
        .unwrap_or("1")
        .parse::<u32>()
        .map_err(|_| malformed(Some("content.xml"), format!("invalid {name}")))?;
    if value == 0 {
        Err(malformed(Some("content.xml"), format!("{name} must be positive")))
    } else {
        Ok(value)
    }
}

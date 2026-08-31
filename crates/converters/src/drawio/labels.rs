use super::{budget::limit, model::Cell, output::Output};
use into_markdown_core::{
    Block, BlockNode, ConversionError, Inline, MAX_DOCUMENT_INLINES, MAX_DOCUMENT_NODES,
    SourceLocator, estimate_retained_output,
};

pub(super) fn label(
    cell: &Cell,
    loc: &SourceLocator,
    out: &mut Output<'_>,
) -> Result<Vec<Inline>, ConversionError> {
    let value = expand_placeholders(cell, loc, out)?;
    let mut content = text_label(&value, cell.style("html") == Some("1"), loc, out)?;
    for (name, uri) in [("link", Some(cell.attr("link"))), ("image", cell.style("image"))] {
        if let Some(uri) = uri.filter(|s| !s.is_empty()) {
            if crate::html::safe_link_target(uri) {
                out.charge(uri.len() + 512)?;
                content.push(out.inline("; ")?);
                content.push(Inline::Link {
                    target: uri.to_owned(),
                    content: vec![out.inline(name)?],
                });
            } else {
                out.warning(
                    "drawio.unsafeReference",
                    format!("Unsafe {name} reference omitted"),
                    loc,
                )?;
            }
        }
    }
    Ok(content)
}

fn expand_placeholders(
    cell: &Cell,
    loc: &SourceLocator,
    out: &mut Output<'_>,
) -> Result<String, ConversionError> {
    let mut value = String::new();
    let mut rest = cell.label();
    while !rest.is_empty() {
        out.context.checkpoint()?;
        let (prefix, tail) = if cell.attr("placeholders") == "1" {
            rest.split_once('%').unwrap_or((rest, ""))
        } else {
            (rest, "")
        };
        out.charge(prefix.len() * 2 + 8)?;
        value.push_str(prefix);
        if prefix.len() == rest.len() {
            break;
        }
        if let Some((key, next)) = tail.split_once('%') {
            if let Some(replacement) = cell.attrs.get(key) {
                out.charge(replacement.len() * 2 + 8)?;
                value.push_str(replacement);
            } else {
                out.charge(key.len() * 2 + 16)?;
                value.push('%');
                value.push_str(key);
                value.push('%');
                out.warning(
                    "drawio.unresolvedPlaceholder",
                    "Unresolved label placeholder retained verbatim".into(),
                    loc,
                )?;
            }
            rest = next;
        } else {
            out.charge(tail.len() * 2 + 8)?;
            value.push('%');
            value.push_str(tail);
            break;
        }
        if value.len() as u64 > out.options.limits.max_field_bytes {
            return Err(limit("max_field_bytes", "expanded Drawio label exceeds field budget"));
        }
    }
    Ok(value)
}

fn text_label(
    value: &str,
    html: bool,
    loc: &SourceLocator,
    out: &mut Output<'_>,
) -> Result<Vec<Inline>, ConversionError> {
    if value.is_empty() {
        return Ok(vec![out.inline("(unlabeled)")?]);
    }
    if !html {
        let mut result = Vec::new();
        for (i, line) in value.lines().enumerate() {
            if i > 0 {
                out.charge(256)?;
                result.push(Inline::LineBreak);
            }
            result.push(out.inline(line)?);
        }
        return Ok(result);
    }
    let mut html_budget = crate::html::FeedHtmlBudget::new(
        out.options.limits.max_decompressed_bytes,
        MAX_DOCUMENT_NODES,
        out.options.limits.max_memory_bytes,
        out.context,
    )?;
    let parsed = crate::html::convert_feed_html_fragment(
        value,
        None,
        out.options,
        out.context,
        &mut html_budget,
    )?;
    let retained = estimate_retained_output(&parsed.document, &parsed.assets, &parsed.diagnostics)?;
    // Account transferred HTML strings/marks before their fragment-owned lease is released.
    out.charge(
        usize::try_from(retained)
            .map_err(|_| limit("max_memory_bytes", "HTML retained size overflow"))?,
    )?;
    for diagnostic in &parsed.diagnostics {
        out.warning(&diagnostic.code, diagnostic.message.clone(), loc)?;
    }
    let mut inlines = Vec::new();
    for block in parsed.document.blocks {
        flatten(block, &parsed.assets, loc, out, &mut inlines)?;
    }
    if inlines.last() == Some(&Inline::LineBreak) {
        inlines.pop();
    }
    if inlines.is_empty() {
        inlines.push(out.inline("(unlabeled)")?);
    }
    Ok(inlines)
}

fn flatten(
    node: BlockNode,
    assets: &[into_markdown_core::Asset],
    loc: &SourceLocator,
    out: &mut Output<'_>,
    target: &mut Vec<Inline>,
) -> Result<(), ConversionError> {
    out.context.checkpoint()?;
    match node.block {
        Block::Paragraph(values)
        | Block::Heading { content: values, .. }
        | Block::TimedSegment { content: values, .. } => {
            for value in values {
                push(value, out, target)?;
            }
            out.charge(256)?;
            target.push(Inline::LineBreak);
        }
        Block::List { items, .. } => {
            for item in items {
                for block in item.blocks {
                    flatten(block, assets, loc, out, target)?;
                }
            }
        }
        Block::Table { rows, .. } => {
            for row in rows {
                for cell in row.cells {
                    for block in cell.blocks {
                        flatten(block, assets, loc, out, target)?;
                    }
                }
            }
        }
        Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. }
        | Block::Footnote { blocks, .. } => {
            for block in blocks {
                flatten(block, assets, loc, out, target)?;
            }
        }
        Block::Code { text, .. } | Block::Formula(text) => {
            let inline = out.inline(&text)?;
            target.push(inline);
        }
        Block::Image { asset, alt } => {
            let label = alt.as_deref().filter(|s| !s.is_empty()).unwrap_or("image");
            let content = vec![out.inline(label)?];
            if let Some(uri) = assets
                .iter()
                .find(|a| a.id == asset)
                .and_then(|a| a.external_uri.as_deref())
                .filter(|s| crate::html::safe_link_target(s))
            {
                out.charge(uri.len() + 256)?;
                target.push(Inline::Link { target: uri.into(), content });
            } else {
                target.extend(content);
                out.warning("drawio.imageReferenceOmitted", "Image retains its alternative text; its URI cannot be retained under the link safety policy".into(), loc)?;
            }
        }
        Block::Rule => {
            out.charge(256)?;
            target.push(Inline::LineBreak);
        }
        _ => {
            return Err(ConversionError::Unsupported {
                detail: "HTML label emitted an unsupported block".into(),
            });
        }
    }
    Ok(())
}

fn push(
    value: Inline,
    out: &mut Output<'_>,
    target: &mut Vec<Inline>,
) -> Result<(), ConversionError> {
    out.inlines += 1;
    if out.inlines > MAX_DOCUMENT_INLINES {
        return Err(limit("documentInlines", "Drawio HTML labels exceed inline budget"));
    }
    out.charge(1024)?;
    let value = match value {
        Inline::Text { value, mut marks }
        | Inline::SourceText { value, mut marks, .. }
        | Inline::OcrText { value, mut marks, .. } => {
            marks.sort_unstable();
            marks.dedup();
            Inline::Text { value, marks }
        }
        Inline::Link { target: uri, content } => {
            let mut values = Vec::new();
            for item in content {
                push(item, out, &mut values)?;
            }
            if uri.trim().is_empty() || !crate::html::safe_link_target(&uri) {
                target.extend(values);
                return Ok(());
            }
            Inline::Link { target: uri, content: values }
        }
        other => other,
    };
    target.push(value);
    Ok(())
}

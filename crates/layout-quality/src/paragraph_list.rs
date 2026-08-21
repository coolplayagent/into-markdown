use into_markdown_core::{Block, ConversionError, ExecutionContext, Inline, ListKind};
use unicode_normalization::UnicodeNormalization as _;

pub(crate) fn kind(block: &Block) -> String {
    match block {
        Block::Paragraph(_) => "paragraph".into(),
        Block::Heading { level, .. } => format!("heading:{level}"),
        Block::List { kind, start, .. } => {
            let marker = match kind {
                ListKind::Bullet => "bullet",
                ListKind::Ordered => "ordered",
                ListKind::Task => "task",
            };
            format!("list:{marker}:{start}")
        }
        Block::Table { .. } => "table".into(),
        Block::Code { language, .. } => {
            format!("code:{}", language.as_deref().unwrap_or_default())
        }
        Block::Formula(_) => "formula".into(),
        Block::Footnote { label, .. } => format!("footnote:{label}"),
        Block::Image { .. } => "image".into(),
        Block::Page { number, .. } => format!("page:{number}"),
        Block::Slide { number, .. } => format!("slide:{number}"),
        Block::Sheet { name, .. } => format!("sheet:{name}"),
        Block::TimedSegment { .. } => "timedSegment".into(),
        Block::Rule => "rule".into(),
        _ => "unknown".into(),
    }
}

pub(crate) fn text(block: &Block, execution: &ExecutionContext) -> Result<String, ConversionError> {
    Ok(match block {
        Block::Paragraph(content)
        | Block::Heading { content, .. }
        | Block::TimedSegment { content, .. } => inline_text(content, execution)?,
        Block::Code { text, .. } | Block::Formula(text) => normalize(text),
        Block::Footnote { label, .. } => normalize(label),
        Block::Image { alt, .. } => alt.as_deref().map_or_else(String::new, normalize),
        Block::Slide { title, .. } => title.as_deref().map_or_else(String::new, normalize),
        Block::Sheet { name, .. } => normalize(name),
        _ => String::new(),
    })
}

pub(crate) fn inline_text(
    inlines: &[Inline],
    context: &ExecutionContext,
) -> Result<String, ConversionError> {
    let mut raw = String::new();
    append_inline_text(inlines, &mut raw, context)?;
    Ok(normalize(&raw))
}

fn append_inline_text(
    inlines: &[Inline],
    output: &mut String,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    for inline in inlines {
        context.checkpoint()?;
        match inline {
            Inline::Text { value, .. }
            | Inline::SourceText { value, .. }
            | Inline::OcrText { value, .. }
            | Inline::Code(value)
            | Inline::Formula(value) => output.push_str(value),
            Inline::Link { content, .. } => append_inline_text(content, output, context)?,
            Inline::FootnoteReference(label) => output.push_str(label),
            Inline::LineBreak => output.push('\n'),
            _ => {}
        }
    }
    Ok(())
}

fn normalize(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut pending_space = false;
    for character in value.nfc() {
        if character.is_whitespace() {
            pending_space = !normalized.is_empty();
        } else {
            if pending_space {
                normalized.push(' ');
                pending_space = false;
            }
            normalized.push(character);
        }
    }
    normalized
}

mod lzfu;

use super::budget::{MsgBudget, malformed};
use super::properties::Properties;
use into_markdown_core::{
    Block, BlockNode, ConversionError, ConversionOptions, ConverterOutput, Diagnostic, Document,
    ExecutionContext, Inline, NodeId, Provenance, ProvenanceKind, SourceLocator,
};

const PR_BODY: u16 = 0x1000;
const PR_RTF_COMPRESSED: u16 = 0x1009;
const PR_HTML: u16 = 0x1013;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BodyKind {
    Html,
    Rtf,
    Plain,
    Empty,
}

pub(super) struct SelectedBody {
    pub(super) kind: BodyKind,
    pub(super) output: ConverterOutput,
}

pub(super) trait BodyAdapter {
    fn html(
        &self,
        bytes: &[u8],
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<ConverterOutput, ConversionError>;

    fn rtf(
        &self,
        bytes: &[u8],
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<ConverterOutput, ConversionError>;
}

pub(super) struct BuiltinBodyAdapter;

impl BodyAdapter for BuiltinBodyAdapter {
    fn html(
        &self,
        bytes: &[u8],
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<ConverterOutput, ConversionError> {
        crate::html::convert_embedded_html(bytes, options, context)
    }

    fn rtf(
        &self,
        bytes: &[u8],
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<ConverterOutput, ConversionError> {
        crate::rtf::convert_rtf_bytes(bytes, options, context)
    }
}

pub(super) fn select(
    properties: &Properties,
    adapter: &dyn BodyAdapter,
    options: &ConversionOptions,
    context: &ExecutionContext,
    budget: &mut MsgBudget<'_>,
) -> Result<SelectedBody, ConversionError> {
    if let Some(html) = properties.binary(PR_HTML).filter(|value| !value.is_empty()) {
        let source = required_source(properties, PR_HTML)?;
        let output = adapter.html(html, options, context)?;
        return Ok(remap(BodyKind::Html, output, &source));
    }
    if let Some(html) = properties.text(PR_HTML).filter(|value| !value.is_empty()) {
        let source = required_source(properties, PR_HTML)?;
        let output = adapter.html(html.as_bytes(), options, context)?;
        return Ok(remap(BodyKind::Html, output, &source));
    }
    if let Some(compressed) = properties.binary(PR_RTF_COMPRESSED).filter(|value| !value.is_empty())
    {
        let source = required_source(properties, PR_RTF_COMPRESSED)?;
        let raw = lzfu::decompress(compressed, &source, budget)?;
        let output = adapter.rtf(&raw, options, context)?;
        return Ok(remap(BodyKind::Rtf, output, &source));
    }
    if let Some(plain) = properties.text(PR_BODY).filter(|value| !value.is_empty()) {
        let source = required_source(properties, PR_BODY)?;
        return Ok(SelectedBody { kind: BodyKind::Plain, output: plain_document(plain, &source) });
    }
    Ok(SelectedBody {
        kind: BodyKind::Empty,
        output: ConverterOutput::new(Document::default(), Vec::new(), Vec::new()),
    })
}

fn remap(kind: BodyKind, mut output: ConverterOutput, source: &str) -> SelectedBody {
    for block in &mut output.document.blocks {
        remap_block(block, source);
    }
    SelectedBody { kind, output }
}

fn remap_block(block: &mut BlockNode, source: &str) {
    block.provenance.locator.part = Some(source.to_owned());
    match &mut block.block {
        Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. }
        | Block::Footnote { blocks, .. } => {
            for child in blocks {
                remap_block(child, source);
            }
        }
        Block::List { items, .. } => {
            for item in items {
                for child in &mut item.blocks {
                    remap_block(child, source);
                }
            }
        }
        Block::Table { rows, .. } => {
            for row in rows {
                for cell in &mut row.cells {
                    for child in &mut cell.blocks {
                        remap_block(child, source);
                    }
                }
            }
        }
        _ => {}
    }
}

fn plain_document(text: &str, source: &str) -> ConverterOutput {
    let provenance = Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: "builtin.converter.msg".into(),
        locator: SourceLocator { part: Some(source.into()), ..SourceLocator::default() },
        confidence: Some(1.0),
    };
    let content = lines(text);
    let document = Document {
        blocks: vec![BlockNode {
            id: NodeId("msg-body-1".into()),
            block: Block::Paragraph(content),
            provenance,
        }],
        ..Document::default()
    };
    ConverterOutput::new(document, Vec::new(), Vec::<Diagnostic>::new())
}

fn lines(text: &str) -> Vec<Inline> {
    let mut output = Vec::new();
    for (index, line) in text.replace("\r\n", "\n").replace('\r', "\n").split('\n').enumerate() {
        if index > 0 {
            output.push(Inline::LineBreak);
        }
        if !line.is_empty() {
            output.push(Inline::Text { value: line.to_owned(), marks: Vec::new() });
        }
    }
    output
}

fn required_source(properties: &Properties, id: u16) -> Result<String, ConversionError> {
    properties
        .source(id)
        .map(str::to_owned)
        .ok_or_else(|| malformed("msg/body", "selected body has no property provenance"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;
    impl BodyAdapter for Fake {
        fn html(
            &self,
            _: &[u8],
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<ConverterOutput, ConversionError> {
            Ok(ConverterOutput::new(Document::default(), Vec::new(), Vec::new()))
        }
        fn rtf(
            &self,
            _: &[u8],
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<ConverterOutput, ConversionError> {
            Ok(ConverterOutput::new(Document::default(), Vec::new(), Vec::new()))
        }
    }

    #[test]
    fn fake_adapter_is_object_safe() {
        let _: &dyn BodyAdapter = &Fake;
    }
}

//! Offline Drawio semantic conversion.
//!
//! The ordered page/cell/edge extraction pipeline is adapted from Cathryn Lavery's
//! diagram-design `drawio_extract.py` at cc2f51f3fd215536cbfc0cf376ea3b513478e9cb.
//! Copyright (c) 2025 Cathryn Lavery. MIT license: third_party/licenses/diagram-design-MIT.txt.
//! Resource accounting, IR projection and recovery follow into-markdown contracts.

mod budget;
mod decode;
mod graph;
mod labels;
mod model;
mod output;
mod pages;
mod render;
#[cfg(test)]
mod tests;
mod xml;

use budget::Budget;
use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput, ExecutionContext,
    FormatCandidate, InputFormat, ProbeOutcome, ResolvedInput, Services,
};
pub(crate) use xml::evidence;
const PROVIDER: &str = "builtin.converter.drawio";

/// Built-in Drawio page, group, node and relationship converter.
#[derive(Debug, Default)]
pub struct DrawioConverter;

impl Converter for DrawioConverter {
    fn id(&self) -> &'static str {
        PROVIDER
    }
    fn priority(&self) -> i32 {
        220
    }
    fn supported_formats(&self) -> &'static [InputFormat] {
        &[InputFormat::Drawio]
    }
    fn probe<'a>(
        &'a self,
        _: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            Ok(if candidate.format == InputFormat::Drawio {
                ProbeOutcome::Match { confidence: 1.0 }
            } else {
                ProbeOutcome::NotApplicable
            })
        })
    }
    fn planned_output_bytes(
        &self,
        _: &ResolvedInput,
        _: &FormatCandidate,
        _: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        Ok(context.available_memory_bytes())
    }
    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            convert_named(&input.bytes, options, context, input.metadata.name.as_deref())
        })
    }
}

#[cfg(test)]
fn convert(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    convert_named(bytes, options, context, None)
}

fn convert_named(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
    name: Option<&str>,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    if bytes.len() as u64 > options.limits.max_input_bytes {
        return Err(budget::limit("max_input_bytes", "Drawio input exceeds request limit"));
    }
    let mut budget = Budget::new(options, context);
    let pages = pages::read(bytes, &mut budget)?;
    let mut out = output::Output::new(options, context)?;
    let extension_format = name
        .and_then(|n| std::path::Path::new(n).extension())
        .and_then(std::ffi::OsStr::to_str)
        .and_then(InputFormat::from_extension);
    if let Some(format) =
        extension_format.filter(|f| !matches!(f, InputFormat::Drawio | InputFormat::Xml))
    {
        out.warning(
            "drawio.extensionMismatch",
            format!(
                "Filename suggests {}; Drawio graph structure is being converted",
                format.as_str()
            ),
            &into_markdown_core::SourceLocator::default(),
        )?;
    }
    for (i, page) in pages.pages.iter().enumerate() {
        context.checkpoint()?;
        let number = u32::try_from(i + 1)
            .map_err(|_| budget::limit("max_pages", "Drawio page count overflow"))?;
        let mark = out.mark();
        let converted = convert_page(bytes, page, number, &mut budget, &mut out);
        match converted {
            Ok(blocks) => out.document.blocks.extend(blocks),
            Err(ConversionError::Malformed { part, detail })
                if options.error_policy == into_markdown_core::ErrorPolicy::Strict =>
            {
                return Err(ConversionError::Malformed {
                    part: part
                        .filter(|p| p != "drawio")
                        .or_else(|| render::page_locator(page, number).part),
                    detail,
                });
            }
            Err(ConversionError::Malformed { detail, .. }) => {
                out.rewind(mark)?;
                out.defect(
                    "drawio.pageOmitted",
                    format!("Page {number} omitted: {detail}"),
                    &render::page_locator(page, number),
                )?;
            }
            Err(error) => return Err(error),
        }
    }
    if out.document.blocks.is_empty() {
        return Err(budget::malformed("Drawio has no recoverable pages"));
    }
    out.finish()
}

fn convert_page(
    bytes: &[u8],
    page: &pages::Page,
    number: u32,
    budget: &mut Budget<'_>,
    out: &mut output::Output<'_>,
) -> Result<Vec<into_markdown_core::BlockNode>, ConversionError> {
    if let Some(error) = page.error {
        return Err(budget::malformed(error));
    }
    if let Some(range) = &page.model {
        if !page.payload.trim().is_empty() {
            return Err(budget::malformed("diagram contains both model and encoded payload"));
        }
        budget.expand(range.len())?;
        let model = model::parse(&bytes[range.clone()], budget)?;
        render::render(&model, page, number, out)
    } else {
        let decoded = decode::decode(&page.payload, budget)?;
        let model = model::parse(&decoded.bytes, budget)?;
        render::render(&model, page, number, out)
    }
}

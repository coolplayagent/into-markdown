mod attachments;
mod body;
mod budget;
mod merge;
mod ole;
mod properties;
mod recipients;

use attachments::ParsedAttachment;
use body::{BodyAdapter, BuiltinBodyAdapter};
use budget::MsgBudget;
use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput, ExecutionContext,
    FormatCandidate, InputFormat, ProbeOutcome, ResolvedInput, Services,
};
use merge::AttachmentOutput;
use ole::{CompoundFile, Storage};

const FORMATS: &[InputFormat] = &[InputFormat::OutlookMsg];
const PROVIDER_ID: &str = "builtin.converter.msg";
const CFB_SIGNATURE: &[u8; 8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";

/// Strict, offline Outlook MSG converter.
#[derive(Debug, Default)]
pub struct MsgConverter;

impl Converter for MsgConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        250
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        FORMATS
    }

    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            if candidate.format != InputFormat::OutlookMsg {
                return Ok(ProbeOutcome::NotApplicable);
            }
            Ok(
                if candidate.explicit
                    || candidate.detector_id == "builtin.detector.hints"
                    || input.bytes.starts_with(CFB_SIGNATURE)
                {
                    ProbeOutcome::Match { confidence: 1.0 }
                } else {
                    ProbeOutcome::NotApplicable
                },
            )
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
        Box::pin(async move { convert_msg(&input.bytes, options, context, &BuiltinBodyAdapter) })
    }
}

fn convert_msg(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
    adapter: &dyn BodyAdapter,
) -> Result<ConverterOutput, ConversionError> {
    let mut budget = MsgBudget::new(bytes.len(), options, context)?;
    let file = CompoundFile::open(bytes, &mut budget)?;
    convert_storage(file.root(), 0, "msg-root", options, context, adapter, &mut budget)
}

fn convert_storage(
    storage: Storage<'_>,
    depth: u16,
    prefix: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
    adapter: &dyn BodyAdapter,
    budget: &mut MsgBudget<'_>,
) -> Result<ConverterOutput, ConversionError> {
    budget.depth(depth, &storage.path())?;
    let properties = properties::Properties::parse(storage, true, budget)?;
    let sender = recipients::sender(&properties)?;
    let recipient_list = recipients::parse_all(storage, budget)?;
    if properties.recipient_count() != Some(u32::try_from(recipient_list.len()).unwrap_or(u32::MAX))
    {
        return Err(budget::malformed(
            storage.path(),
            "declared MSG recipient count does not match recipient storages",
        ));
    }
    let parsed_attachments = attachments::parse_all(storage, budget)?;
    if properties.attachment_count()
        != Some(u32::try_from(parsed_attachments.len()).unwrap_or(u32::MAX))
    {
        return Err(budget::malformed(
            storage.path(),
            "declared MSG attachment count does not match attachment storages",
        ));
    }
    let selected_body =
        body::select(&properties, &parsed_attachments, adapter, options, context, budget)?;
    let mut completed = Vec::with_capacity(parsed_attachments.len());
    for (index, parsed) in parsed_attachments.into_iter().enumerate() {
        let ParsedAttachment { asset, nested, content_id, safe_image, filename, source } = parsed;
        let nested_output = nested
            .map(|nested| {
                let next_depth = depth.checked_add(1).ok_or_else(|| {
                    budget::limit("max_nesting_depth", "MSG attachment depth overflowed")
                })?;
                convert_storage(
                    nested,
                    next_depth,
                    &format!("{prefix}-nested-{}", index + 1),
                    options,
                    context,
                    adapter,
                    budget,
                )
            })
            .transpose()?;
        completed.push(AttachmentOutput {
            asset,
            content_id,
            safe_image,
            filename,
            source,
            nested: nested_output,
        });
    }
    let output = merge::assemble(
        &properties,
        sender.as_ref(),
        &recipient_list,
        selected_body,
        completed,
        prefix,
        context,
    )?;
    output.document.validate().map_err(|error| ConversionError::Internal {
        detail: format!(
            "MSG merger returned invalid document IR ({} at {}): {}",
            error.code.as_str(),
            error.path,
            error.detail
        ),
    })?;
    Ok(output)
}

#[cfg(test)]
mod tests;

//! Sequential, converter-local quality evidence for legacy Office changes.

use futures::executor::block_on;
use into_markdown_converters::LegacyOfficeConverter;
use into_markdown_core::{
    ConversionOptions, Converter, ExecutionContext, ExecutionOptions, FormatCandidate, InputFormat,
    MarkdownRenderer, ResolvedInput, Services, SourceMetadata,
};
use into_markdown_render_markdown::GfmRenderer;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, sync::Arc, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let source = PathBuf::from(args.next().ok_or("missing input")?);
    let destination = PathBuf::from(args.next().ok_or("missing output JSON")?);
    let format = match args.next().and_then(|value| value.into_string().ok()).as_deref() {
        Some("doc") => InputFormat::Doc,
        Some("ppt") => InputFormat::Ppt,
        _ => return Err("usage: legacy_office_probe INPUT OUTPUT_JSON doc|ppt".into()),
    };
    if args.next().is_some() || destination.exists() {
        return Err("unexpected argument or output already exists".into());
    }
    let bytes = fs::read(&source)?;
    let source_sha = format!("{:x}", Sha256::digest(&bytes));
    let input = ResolvedInput {
        metadata: SourceMetadata { size: bytes.len() as u64, ..SourceMetadata::default() },
        bytes: Arc::from(bytes),
    };
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let start = Instant::now();
    let converted = block_on(LegacyOfficeConverter.convert(
        &input,
        &FormatCandidate::new(format, 1.0, "explicit regression format"),
        &options,
        &Services::default(),
        &context,
    ));
    let mut record = match converted {
        Ok(output) => {
            let validation = output.document.validate();
            let rendered =
                block_on(GfmRenderer.render(&output.document, &output.assets, &options, &context));
            let valid = validation.is_ok() && rendered.is_ok();
            let assets = output
                .assets
                .iter()
                .map(|asset| json!({"id": asset.id, "mediaType": asset.media_type}))
                .collect::<Vec<_>>();
            json!({
                "status": if valid { "success" } else { "failed" },
                "errorCode": if valid { None } else { Some("internal") },
                "validationError": validation.err().map(|error| error.to_string()),
                "renderError": rendered.as_ref().err().map(ToString::to_string),
                "markdown": rendered.ok(),
                "document": output.document,
                "assets": assets,
                "diagnostics": output.diagnostics,
            })
        }
        Err(error) => json!({
            "status": "failed", "errorCode": error.code().as_str(),
            "message": error.to_string(),
        }),
    };
    record["durationMs"] = json!(start.elapsed().as_secs_f64() * 1000.0);
    record["sourceSha256"] = json!(source_sha);
    record["memoryLeaseAfterBytes"] = json!(context.reserved_memory_bytes());
    record["temporaryLeaseAfterBytes"] = json!(context.reserved_temporary_bytes());
    fs::write(destination, serde_json::to_vec(&record)?)?;
    Ok(())
}

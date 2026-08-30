//! Test-only DOCX corpus probe for issue evidence.

use futures::executor::block_on;
use into_markdown_converters::DocxConverter;
use into_markdown_core::{
    ConversionOptions, Converter, ExecutionContext, ExecutionOptions, FormatCandidate, InputFormat,
    MarkdownRenderer, ResolvedInput, ResourceLimits, Services, SourceMetadata,
};
use into_markdown_render_markdown::GfmRenderer;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn temporary_snapshot(root: &Path) -> std::io::Result<(u64, u64)> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                files = files.saturating_add(1);
                bytes = bytes.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok((files, bytes))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let source = PathBuf::from(arguments.next().ok_or("missing DOCX path")?);
    let temporary = PathBuf::from(arguments.next().ok_or("missing temporary directory")?);
    if arguments.next().is_some() || !source.is_file() || !temporary.is_dir() {
        return Err("usage: docx_corpus_probe DOCX EXISTING_TEMP_DIRECTORY".into());
    }
    let bytes = fs::read(&source)?;
    let options = ConversionOptions::default();
    let context = ExecutionContext::new_with_temporary_directory(
        ExecutionOptions::default(),
        ResourceLimits::default(),
        temporary.clone(),
    );
    let input = ResolvedInput {
        bytes: Arc::from(bytes),
        metadata: SourceMetadata {
            name: source.file_name().and_then(|name| name.to_str()).map(str::to_owned),
            media_type: Some(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document".into(),
            ),
            size: source.metadata()?.len(),
            ..SourceMetadata::default()
        },
    };
    let before_temporary = temporary_snapshot(&temporary)?;
    let before_memory = context.reserved_memory_bytes();
    let before_temporary_lease = context.reserved_temporary_bytes();
    let converted = block_on(DocxConverter.convert(
        &input,
        &FormatCandidate::new(InputFormat::Docx, 1.0, "corpus manifest"),
        &options,
        &Services::default(),
        &context,
    ));
    let output = match converted {
        Ok(output) => output,
        Err(error) => {
            let after_temporary = temporary_snapshot(&temporary)?;
            println!(
                "{}",
                json!({
                    "status": "failed",
                    "errorCode": error.code().as_str(),
                    "memoryLeaseBeforeBytes": before_memory,
                    "memoryLeaseAfterBytes": context.reserved_memory_bytes(),
                    "temporaryLeaseBeforeBytes": before_temporary_lease,
                    "temporaryLeaseAfterBytes": context.reserved_temporary_bytes(),
                    "temporaryFilesBefore": before_temporary.0,
                    "temporaryBytesBefore": before_temporary.1,
                    "temporaryFilesAfter": after_temporary.0,
                    "temporaryBytesAfter": after_temporary.1,
                })
            );
            std::process::exit(2);
        }
    };
    let renderer = GfmRenderer;
    let markdown = block_on(renderer.render(&output.document, &output.assets, &options, &context))?;
    let markdown_bytes = markdown.len();
    let markdown_sha256 = format!("{:x}", Sha256::digest(markdown.as_bytes()));
    let held_memory = context.reserved_memory_bytes();
    let held_temporary = context.reserved_temporary_bytes();
    let diagnostic_codes =
        output.diagnostics.iter().map(|diagnostic| diagnostic.code.clone()).collect::<Vec<_>>();
    let blocks = output.document.blocks.len();
    let assets = output.assets.len();
    drop(markdown);
    drop(output);
    let after_temporary = temporary_snapshot(&temporary)?;
    println!(
        "{}",
        json!({
            "status": "success",
            "blocks": blocks,
            "assets": assets,
            "diagnosticCodes": diagnostic_codes,
            "markdownBytes": markdown_bytes,
            "markdownSha256": markdown_sha256,
            "memoryLeaseBeforeBytes": before_memory,
            "memoryLeaseHeldBytes": held_memory,
            "memoryLeaseAfterBytes": context.reserved_memory_bytes(),
            "temporaryLeaseBeforeBytes": before_temporary_lease,
            "temporaryLeaseHeldBytes": held_temporary,
            "temporaryLeaseAfterBytes": context.reserved_temporary_bytes(),
            "temporaryFilesBefore": before_temporary.0,
            "temporaryBytesBefore": before_temporary.1,
            "temporaryFilesAfter": after_temporary.0,
            "temporaryBytesAfter": after_temporary.1,
        })
    );
    Ok(())
}

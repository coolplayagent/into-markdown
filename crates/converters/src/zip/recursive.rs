use super::archive::{Archive, EntryMeta};
use super::budget::ArchiveBudget;
use super::entry_policy::EntryKind;
use super::merge::MergeState;
use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, ConverterOutput, ErrorCode, FormatHint,
    InputFormat, NestedConversionRequest, ResolvedInput, Services, SourceMetadata,
};
use std::sync::Arc;

const ZIP_CONVERTER_ID: &str = "builtin.converter.zip";
const EXCLUDED_ZIP: &[&str] = &[ZIP_CONVERTER_ID];

pub(super) async fn convert(
    bytes: &[u8],
    options: &ConversionOptions,
    services: &Services,
    context: &into_markdown_core::ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let mut budget = ArchiveBudget::new(options, context);
    let mut merge = MergeState::new(context)?;
    walk_archive(bytes, 1, "", options, services, &mut budget, &mut merge).await?;
    merge.finish()
}

fn walk_archive<'a>(
    bytes: &'a [u8],
    depth: u16,
    prefix: &'a str,
    options: &'a ConversionOptions,
    services: &'a Services,
    budget: &'a mut ArchiveBudget<'_>,
    merge: &'a mut MergeState<'_>,
) -> BoxFuture<'a, Result<(), ConversionError>> {
    Box::pin(async move {
        let mut archive = Archive::open(bytes, depth, budget)?;
        let entries = archive.entries().to_vec();
        for entry in entries {
            budget.context().checkpoint()?;
            if entry.kind == EntryKind::Directory {
                continue;
            }
            let path = joined_path(prefix, &entry.name);
            let data = match archive.read_entry(&entry, budget) {
                Ok(data) => data,
                Err(error) if is_terminal(&error) => return Err(error),
                Err(error) => {
                    merge.failure(&path, &error);
                    continue;
                }
            };
            if is_explicit_zip(&entry) {
                let next_depth =
                    depth.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                        limit: "max_archive_depth",
                        detail: "archive depth overflowed".into(),
                    })?;
                match walk_archive(&data.bytes, next_depth, &path, options, services, budget, merge)
                    .await
                {
                    Ok(()) => {}
                    Err(error) if is_terminal(&error) => return Err(error),
                    Err(error) => merge.failure(&path, &error),
                }
                continue;
            }
            let result = convert_member(&entry, &data.bytes, options, services, budget).await;
            match result {
                Ok(output) => merge.append(&path, output)?,
                Err(error) if looks_like_zip(&data.bytes) && is_no_match(&error) => {
                    let next_depth =
                        depth.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                            limit: "max_archive_depth",
                            detail: "archive depth overflowed".into(),
                        })?;
                    match walk_archive(
                        &data.bytes,
                        next_depth,
                        &path,
                        options,
                        services,
                        budget,
                        merge,
                    )
                    .await
                    {
                        Ok(()) => {}
                        Err(error) if is_terminal(&error) => return Err(error),
                        Err(error) => merge.failure(&path, &error),
                    }
                }
                Err(error) if is_terminal(&error) => return Err(error),
                Err(error) => merge.failure(&path, &error),
            }
        }
        Ok(())
    })
}

async fn convert_member(
    entry: &EntryMeta,
    bytes: &[u8],
    options: &ConversionOptions,
    services: &Services,
    budget: &ArchiveBudget<'_>,
) -> Result<ConverterOutput, ConversionError> {
    let nested = services.nested.as_ref().ok_or_else(|| ConversionError::ComponentUnavailable {
        component: "nested-conversion".into(),
        detail: "the engine did not provide container-member dispatch".into(),
    })?;
    let extension = entry.name.rsplit_once('.').map(|(_, extension)| extension.to_owned());
    let hint =
        FormatHint { filename: Some(entry.name.clone()), extension, ..FormatHint::default() };
    let size = u64::try_from(bytes.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_archive_entry_bytes",
        detail: format!("archive member {} size overflowed", entry.name),
    })?;
    let input = ResolvedInput {
        bytes: Arc::from(bytes),
        metadata: SourceMetadata {
            name: Some(entry.name.clone()),
            media_type: None,
            uri: None,
            size,
        },
    };
    nested
        .convert(
            NestedConversionRequest {
                input: &input,
                hint: &hint,
                options,
                excluded_converter_ids: EXCLUDED_ZIP,
            },
            budget.context(),
        )
        .await
}

fn is_explicit_zip(entry: &EntryMeta) -> bool {
    entry.name.rsplit_once('.').is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("zip"))
}

fn looks_like_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
}

fn is_no_match(error: &ConversionError) -> bool {
    matches!(error.code(), ErrorCode::Unsupported | ErrorCode::NoConverter)
}

fn is_terminal(error: &ConversionError) -> bool {
    matches!(
        error.code(),
        ErrorCode::Cancelled | ErrorCode::Timeout | ErrorCode::Internal | ErrorCode::ResourceLimit
    )
}

fn joined_path(prefix: &str, member: &str) -> String {
    if prefix.is_empty() { member.to_owned() } else { format!("{prefix}/{member}") }
}

#[allow(dead_code)]
fn _assert_format_exists() -> InputFormat {
    InputFormat::Zip
}

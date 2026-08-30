use super::archive::{Archive, EntryMeta};
use super::budget::ArchiveBudget;
use super::entry_policy::EntryKind;
use super::merge::MergeState;
use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, ConverterOutput, ErrorCode, FormatHint,
    NestedConversionRequest, ResolvedInput, Services, SourceContentEvidence, SourceMetadata,
};
use std::sync::Arc;

const ZIP_CONVERTER_ID: &str = "builtin.converter.zip";
const EXCLUDED_ZIP: &[&str] = &[ZIP_CONVERTER_ID];

pub(super) async fn convert<'a>(
    bytes: &[u8],
    options: &'a ConversionOptions,
    services: &'a Services,
    context: &'a into_markdown_core::ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let mut walker = RecursiveConverter {
        options,
        services,
        budget: ArchiveBudget::new(options, context),
        merge: MergeState::new(context)?,
        stats: WalkStats::default(),
    };
    walker.walk_archive(bytes, 1, "").await?;
    if walker.stats.leaves != 0 && walker.stats.converted == 0 {
        return Err(walker.stats.first_failure.take().unwrap_or_else(|| {
            ConversionError::Unsupported { detail: "ZIP contains no convertible members".into() }
        }));
    }
    let has_leaf_content = walker.stats.leaves != 0;
    let output = walker.merge.finish()?;
    Ok(if has_leaf_content {
        output
    } else {
        output.with_source_content_evidence(SourceContentEvidence::Empty)
    })
}

struct RecursiveConverter<'a> {
    options: &'a ConversionOptions,
    services: &'a Services,
    budget: ArchiveBudget<'a>,
    merge: MergeState<'a>,
    stats: WalkStats,
}

impl RecursiveConverter<'_> {
    fn walk_archive<'a>(
        &'a mut self,
        bytes: &'a [u8],
        depth: u16,
        prefix: &'a str,
    ) -> BoxFuture<'a, Result<(), ConversionError>> {
        Box::pin(async move {
            let mut archive = Archive::open(bytes, depth, &mut self.budget)?;
            let entries = archive.take_entries();
            for entry in entries {
                self.budget.context().checkpoint()?;
                if entry.kind == EntryKind::Directory {
                    continue;
                }
                let (path, _path_memory) = joined_path(prefix, &entry.name, self.budget.context())?;
                let data = match archive.read_entry(&entry, &mut self.budget) {
                    Ok(data) => data,
                    Err(error) if is_terminal(&error) => return Err(error),
                    Err(error) => {
                        self.merge.failure(&path, &error)?;
                        self.stats.failure(error);
                        continue;
                    }
                };
                if is_explicit_zip(&entry) {
                    let next_depth =
                        depth.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                            limit: "max_archive_depth",
                            detail: "archive depth overflowed".into(),
                        })?;
                    match self.walk_archive(&data.bytes, next_depth, &path).await {
                        Ok(()) => {}
                        Err(error) if is_terminal(&error) => return Err(error),
                        Err(error) => {
                            self.merge.failure(&path, &error)?;
                            self.stats.failure(error);
                        }
                    }
                    continue;
                }
                let result =
                    convert_member(&entry, &data.bytes, self.options, self.services, &self.budget)
                        .await;
                match result {
                    Ok(output) => {
                        self.merge.append(&path, output)?;
                        self.stats.success();
                    }
                    Err(error) if looks_like_zip(&data.bytes) && is_no_match(&error) => {
                        let next_depth =
                            depth.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                                limit: "max_archive_depth",
                                detail: "archive depth overflowed".into(),
                            })?;
                        match self.walk_archive(&data.bytes, next_depth, &path).await {
                            Ok(()) => {}
                            Err(error) if is_terminal(&error) => return Err(error),
                            Err(error) => {
                                self.merge.failure(&path, &error)?;
                                self.stats.failure(error);
                            }
                        }
                    }
                    Err(error) if is_terminal(&error) => return Err(error),
                    Err(error) => {
                        self.merge.failure(&path, &error)?;
                        self.stats.failure(error);
                    }
                }
            }
            Ok(())
        })
    }
}

#[derive(Default)]
struct WalkStats {
    leaves: u64,
    converted: u64,
    first_failure: Option<ConversionError>,
}

impl WalkStats {
    fn success(&mut self) {
        self.leaves = self.leaves.saturating_add(1);
        self.converted = self.converted.saturating_add(1);
    }

    fn failure(&mut self, error: ConversionError) {
        self.leaves = self.leaves.saturating_add(1);
        if self.first_failure.is_none() {
            self.first_failure = Some(error);
        }
    }
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
    let size = u64::try_from(bytes.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_archive_entry_bytes",
        detail: format!("archive member {} size overflowed", entry.name),
    })?;
    let arc_overhead = u64::try_from(std::mem::size_of::<usize>() * 2).unwrap_or(u64::MAX);
    let shared_plan = size
        .checked_add(arc_overhead)
        .ok_or_else(|| memory_limit("archive member shared-buffer size overflowed"))?;
    let mut shared_memory = budget.context().reserve_memory(shared_plan)?;
    let extension = entry
        .name
        .rsplit_once('.')
        .map(|(_, extension)| try_owned(extension, "archive member extension", &mut shared_memory))
        .transpose()?;
    let hint = FormatHint {
        filename: Some(try_owned(&entry.name, "archive member hint", &mut shared_memory)?),
        extension,
        ..FormatHint::default()
    };
    let input = ResolvedInput {
        bytes: Arc::from(bytes),
        metadata: SourceMetadata {
            name: Some(try_owned(&entry.name, "archive member metadata", &mut shared_memory)?),
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
    // Request-wide policy/invariant failures stop the tree. Content/provider
    // failures stay scoped to one member so siblings can still be returned.
    matches!(
        error.code(),
        ErrorCode::Cancelled | ErrorCode::Timeout | ErrorCode::Internal | ErrorCode::ResourceLimit
    )
}

fn joined_path(
    prefix: &str,
    member: &str,
    context: &into_markdown_core::ExecutionContext,
) -> Result<(String, into_markdown_core::ResourceReservation), ConversionError> {
    let separator = usize::from(!prefix.is_empty());
    let length = prefix
        .len()
        .checked_add(separator)
        .and_then(|value| value.checked_add(member.len()))
        .ok_or_else(|| memory_limit("archive member path size overflowed"))?;
    let mut memory = context.reserve_memory(u64::try_from(length).unwrap_or(u64::MAX))?;
    let mut path = String::new();
    path.try_reserve_exact(length)
        .map_err(|error| memory_limit(format!("archive member path allocation failed: {error}")))?;
    let actual = u64::try_from(path.capacity()).unwrap_or(u64::MAX);
    let planned = u64::try_from(length).unwrap_or(u64::MAX);
    if actual > planned {
        memory.grow(actual - planned)?;
    } else if planned > actual {
        memory.shrink(planned - actual)?;
    }
    if !prefix.is_empty() {
        path.push_str(prefix);
        path.push('/');
    }
    path.push_str(member);
    Ok((path, memory))
}

fn try_owned(
    value: &str,
    label: &str,
    memory: &mut into_markdown_core::ResourceReservation,
) -> Result<String, ConversionError> {
    let planned = u64::try_from(value.len()).unwrap_or(u64::MAX);
    memory.grow(planned)?;
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|error| memory_limit(format!("{label} allocation failed: {error}")))?;
    let actual = u64::try_from(output.capacity()).unwrap_or(u64::MAX);
    if actual > planned {
        memory.grow(actual - planned)?;
    } else if planned > actual {
        memory.shrink(planned - actual)?;
    }
    output.push_str(value);
    Ok(output)
}

fn memory_limit(detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}

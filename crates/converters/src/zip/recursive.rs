use super::archive::{Archive, EntryMeta};
use super::budget::ArchiveBudget;
use super::entry_policy::EntryKind;
use super::merge::MergeState;
use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, ConverterOutput, ErrorCode, FormatHint,
    NestedConversionRequest, ResolvedInput, ResourceFailureScope, ResourceLimitSource,
    ResourceRecoveryAction, ResourceRecoveryBoundary, ResourceUnitKind, Services,
    SourceContentEvidence, SourceLocator, SourceMetadata, classify_resource_recovery,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
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
        ancestors: BTreeSet::new(),
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
    ancestors: BTreeSet<[u8; 32]>,
}

impl RecursiveConverter<'_> {
    fn walk_archive<'a>(
        &'a mut self,
        bytes: &'a [u8],
        depth: u16,
        prefix: &'a str,
    ) -> BoxFuture<'a, Result<(), ConversionError>> {
        Box::pin(async move {
            self.budget.context().checkpoint()?;
            let hash: [u8; 32] = Sha256::digest(bytes).into();
            if self.ancestors.contains(&hash) {
                return self.retain_cycle(bytes, prefix);
            }
            let _identity_memory = self.budget.context().reserve_memory(128)?;
            self.ancestors.insert(hash);
            let result = self.walk_members(bytes, depth, prefix).await;
            self.ancestors.remove(&hash);
            if matches!(result, Err(ConversionError::Encrypted))
                && !prefix.is_empty()
                && self.options.error_policy == into_markdown_core::ErrorPolicy::BestEffort
            {
                self.retain_archive(bytes, prefix, "This nested archive requires a password. Its complete original is attached; readable sibling members continue converting.")?;
                self.stats.failure(ConversionError::Encrypted);
                return Ok(());
            }
            result
        })
    }

    fn retain_cycle(&mut self, bytes: &[u8], path: &str) -> Result<(), ConversionError> {
        let reason = "Nested archive repeats an ancestor byte for byte. The complete original member is attached; its already visited contents are retained once.";
        if self.options.error_policy == into_markdown_core::ErrorPolicy::Strict {
            return Err(ConversionError::Malformed {
                part: Some(path.into()),
                detail: reason.into(),
            });
        }
        self.retain_archive(bytes, path, reason)?;
        self.stats.success();
        Ok(())
    }

    fn retain_archive(
        &mut self,
        bytes: &[u8],
        path: &str,
        reason: &str,
    ) -> Result<(), ConversionError> {
        let context = self.budget.context();
        let _input_memory = context.reserve_memory(
            (bytes.len() as u64).saturating_add(path.len() as u64).saturating_add(128),
        )?;
        let input = ResolvedInput {
            bytes: Arc::from(bytes),
            metadata: SourceMetadata {
                name: Some(if path.is_empty() { "original.zip".into() } else { path.into() }),
                ..SourceMetadata::default()
            },
        };
        let output = crate::media::source_attachment::empty_body(
            &input,
            self.options,
            context,
            "application/zip",
            reason,
            ConverterOutput::default(),
        )?;
        self.merge.append(path, output)?;
        Ok(())
    }

    fn open_members<'a>(
        &mut self,
        bytes: &'a [u8],
        depth: u16,
        prefix: &str,
    ) -> Result<(Archive<'a>, Vec<EntryMeta>), ConversionError> {
        let mut archive = Archive::open_recovering(
            bytes,
            depth,
            &mut self.budget,
            self.options.error_policy == into_markdown_core::ErrorPolicy::BestEffort,
        )?;
        let entries = archive.take_entries();
        if entries.iter().any(|entry| entry.encrypted) {
            self.retain_archive(bytes, prefix,
                "Encrypted members require a password. Readable members are converted below; the complete original archive retains the encrypted content.")?;
        }
        Ok((archive, entries))
    }

    fn walk_members<'a>(
        &'a mut self,
        bytes: &'a [u8],
        depth: u16,
        prefix: &'a str,
    ) -> BoxFuture<'a, Result<(), ConversionError>> {
        Box::pin(async move {
            let (mut archive, entries) = self.open_members(bytes, depth, prefix)?;
            for entry in entries {
                self.budget.context().checkpoint()?;
                if entry.kind == EntryKind::Directory {
                    continue;
                }
                let (path, _path_memory) = joined_path(prefix, &entry.name, self.budget.context())?;
                if entry.encrypted {
                    self.merge.encrypted_member(&path)?;
                    self.stats.failure(ConversionError::Encrypted);
                    continue;
                }
                let data = match archive.read_entry(&entry, &mut self.budget) {
                    Ok(data) => data,
                    Err(error) if is_terminal(&error) => return Err(error),
                    Err(error) => {
                        self.merge.failure(&path, &error)?;
                        self.stats.failure(error);
                        continue;
                    }
                };
                if is_explicit_zip(&entry)
                    && into_markdown_core::RarSignature::detect(&data.bytes).is_none()
                {
                    let next_depth =
                        depth.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                            limit: "max_archive_depth",
                            detail: "archive depth overflowed".into(),
                        })?;
                    match self.walk_archive(&data.bytes, next_depth, &path).await {
                        Ok(()) => {}
                        Err(error)
                            if is_terminal(&error)
                                || self.options.error_policy
                                    == into_markdown_core::ErrorPolicy::Strict =>
                        {
                            return Err(error);
                        }
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
                    Err(error) if recover_member_resource(self.options, &path, &error) => {
                        self.merge.resource_failure(
                            &path,
                            &error,
                            configured_limit(self.options, &error),
                        )?;
                        self.stats.failure(error);
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

fn recover_member_resource(
    options: &ConversionOptions,
    path: &str,
    error: &ConversionError,
) -> bool {
    let locator = SourceLocator { part: Some(path.into()), ..SourceLocator::default() };
    let boundary = ResourceRecoveryBoundary {
        scope: ResourceFailureScope::ContentUnit,
        unit: ResourceUnitKind::ArchiveMember,
        locator: Some(&locator),
        rollback_complete: true,
        fallback_retained: true,
        committed_units: 0,
        omitted_units: 1,
        limit_source: ResourceLimitSource::Explicit,
        precise_required: None,
        raised_limit: None,
    };
    classify_resource_recovery(options.error_policy, error, boundary)
        == ResourceRecoveryAction::OmitUnit
}

fn configured_limit(options: &ConversionOptions, error: &ConversionError) -> Option<u64> {
    let ConversionError::ResourceLimit { limit, .. } = error else {
        return None;
    };
    match *limit {
        "max_pages" => Some(u64::from(options.limits.max_pages)),
        "max_archive_entries" => Some(u64::from(options.limits.max_archive_entries)),
        "max_asset_bytes" => Some(options.limits.max_asset_bytes),
        "max_total_asset_bytes" => Some(options.limits.max_total_asset_bytes),
        "max_table_rows" => Some(options.limits.max_table_rows),
        "max_table_columns" => Some(options.limits.max_table_columns),
        "max_table_cells" => Some(options.limits.max_table_cells),
        "max_decompressed_bytes" => Some(options.limits.max_decompressed_bytes),
        "max_temporary_bytes" | "max_temp_bytes" => Some(options.limits.max_temporary_bytes),
        _ => None,
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
                enrich_output: true,
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

#[cfg(test)]
mod cycle_tests {
    use super::*;
    use into_markdown_core::{
        ConversionOutcome, ErrorPolicy, ExecutionContext, ExecutionOptions, conversion_outcome,
    };

    #[test]
    fn repeated_ancestor_retains_one_original_and_strict_rejects_cycle() {
        let bytes = b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
        for policy in [ErrorPolicy::BestEffort, ErrorPolicy::Strict] {
            let options = ConversionOptions { error_policy: policy, ..Default::default() };
            let context =
                ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
            let services = Services::default();
            let hash: [u8; 32] = Sha256::digest(bytes).into();
            let mut walker = RecursiveConverter {
                options: &options,
                services: &services,
                budget: ArchiveBudget::new(&options, &context),
                merge: MergeState::new(&context).unwrap(),
                stats: WalkStats::default(),
                ancestors: BTreeSet::from([hash]),
            };
            let result = futures::executor::block_on(walker.walk_archive(bytes, 2, "inner.zip"));
            if policy == ErrorPolicy::Strict {
                assert!(matches!(result, Err(ConversionError::Malformed { .. })));
            } else {
                result.unwrap();
                assert_eq!(walker.stats.converted, 1);
                let output = walker.merge.finish().unwrap();
                assert_eq!(output.assets.len(), 1);
                assert_eq!(output.assets[0].bytes, bytes);
                assert_eq!(conversion_outcome(&output.diagnostics), ConversionOutcome::Degraded);
                assert_eq!(
                    output.diagnostics[0].locator.as_ref().unwrap().part.as_deref(),
                    Some("inner.zip")
                );
                output.document.validate().unwrap();
            }
        }
    }

    #[test]
    fn identical_siblings_are_traversed_without_cycle_or_retained_identity() {
        let bytes = b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let services = Services::default();
        let mut walker = RecursiveConverter {
            options: &options,
            services: &services,
            budget: ArchiveBudget::new(&options, &context),
            merge: MergeState::new(&context).unwrap(),
            stats: WalkStats::default(),
            ancestors: BTreeSet::new(),
        };
        for path in ["one.zip", "two.zip"] {
            futures::executor::block_on(walker.walk_archive(bytes, 2, path)).unwrap();
            assert!(walker.ancestors.is_empty());
        }
        let output = walker.merge.finish().unwrap();
        assert!(output.assets.is_empty());
        assert!(output.diagnostics.is_empty());
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

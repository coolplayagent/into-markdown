//! Stable spine selection and nested HTML conversion.

use super::budget::EpubBudget;
use super::package::{ManifestItem, Package};
use super::xhtml::{self, Footnote};
use crate::zip_converter::archive_api::{OwnedEntry, SafeArchive};
use into_markdown_core::{
    ConversionError, ConversionOptions, ConverterOutput, Diagnostic, DiagnosticSeverity,
    ErrorPolicy, ExecutionContext, FormatHint, InputFormat, NestedConversionRequest, ResolvedInput,
    ResourceReservation, Services, SourceLocator, SourceMetadata,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

const EPUB_ID: &str = "builtin.converter.epub";
const ZIP_ID: &str = "builtin.converter.zip";
const EXCLUDED: &[&str] = &[EPUB_ID, ZIP_ID];

pub(super) struct Chapter {
    pub(super) path: String,
    pub(super) output: ConverterOutput,
    pub(super) references: BTreeMap<String, String>,
    pub(super) internal_targets: BTreeSet<String>,
    pub(super) anchors: BTreeSet<String>,
    pub(super) footnotes: Vec<Footnote>,
    pub(super) resource_paths: BTreeSet<String>,
}

pub(super) struct SpineResult {
    pub(super) chapters: Vec<Chapter>,
    pub(super) skipped_non_linear: usize,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) omitted_paths: BTreeSet<String>,
}

pub(super) async fn convert(
    package: &Package,
    archive: &mut SafeArchive<'_, '_>,
    mut navigation_entry: Option<(String, OwnedEntry)>,
    options: &ConversionOptions,
    services: &Services,
    budget: &mut EpubBudget<'_>,
    context: &ExecutionContext,
) -> Result<SpineResult, ConversionError> {
    let nested = services.nested.as_ref().ok_or_else(|| ConversionError::ComponentUnavailable {
        component: "nested-conversion".into(),
        detail: "the engine did not provide EPUB chapter dispatch".into(),
    })?;
    let mut chapters = Vec::new();
    let mut selected_paths = BTreeSet::new();
    let mut skipped_non_linear = 0;
    let mut diagnostics = Vec::new();
    let mut omitted_paths = BTreeSet::new();
    let mut offline = options.clone();
    offline.network.enabled = false;
    offline.network.allowed_hosts.clear();
    for itemref in &package.spine {
        budget.checkpoint()?;
        if !itemref.linear {
            skipped_non_linear += 1;
            continue;
        }
        let Some(item) =
            select_spine_item(package, &itemref.idref, options.error_policy, &mut diagnostics)?
        else {
            continue;
        };
        handle_scripted_item(item, options.error_policy, &mut diagnostics)?;
        if !selected_paths.insert(item.path.clone()) {
            return Err(ConversionError::Malformed {
                part: Some(item.path.clone()),
                detail: "multiple spine entries resolve to the same XHTML resource".into(),
            });
        }
        let entry = if navigation_entry.as_ref().is_some_and(|(path, _)| path == &item.path) {
            navigation_entry
                .take()
                .ok_or_else(|| ConversionError::Internal {
                    detail: "EPUB navigation chapter entry was lost before spine conversion".into(),
                })?
                .1
        } else {
            archive.read(&item.path)?
        };
        let prepared = match xhtml::prepare(&item.path, &entry.bytes, archive, budget, context) {
            Ok(prepared) => prepared,
            Err(error) if options.error_policy == ErrorPolicy::BestEffort && error.is_syntax() => {
                drop(entry);
                let error = locate_chapter_error(error.into_conversion_error(), &item.path);
                push_chapter_omitted(&mut diagnostics, &item.path, error.to_string())?;
                omitted_paths.insert(item.path.clone());
                continue;
            }
            Err(error) => {
                return Err(locate_chapter_error(error.into_conversion_error(), &item.path));
            }
        };
        drop(entry);
        let (input, hint, _shared_memory) = chapter_input(item, &prepared, context)?;
        let output = match nested
            .convert(
                NestedConversionRequest {
                    input: &input,
                    hint: &hint,
                    options: &offline,
                    excluded_converter_ids: EXCLUDED,
                },
                context,
            )
            .await
        {
            Ok(output) => output,
            Err(error)
                if options.error_policy == ErrorPolicy::BestEffort
                    && matches!(
                        error,
                        ConversionError::Unsupported { .. } | ConversionError::Malformed { .. }
                    ) =>
            {
                push_chapter_omitted(&mut diagnostics, &item.path, error.to_string())?;
                omitted_paths.insert(item.path.clone());
                continue;
            }
            Err(error) => return Err(error),
        };
        chapters.push(Chapter {
            path: item.path.clone(),
            output,
            references: prepared.references,
            internal_targets: prepared.internal_targets,
            anchors: prepared.anchors,
            footnotes: prepared.footnotes,
            resource_paths: prepared.resource_paths,
        });
    }
    if chapters.is_empty() {
        return Err(ConversionError::Malformed {
            part: Some(package.path.clone()),
            detail: "EPUB spine has no linear XHTML content".into(),
        });
    }
    Ok(SpineResult { chapters, skipped_non_linear, diagnostics, omitted_paths })
}

fn select_spine_item<'a>(
    package: &'a Package,
    idref: &str,
    policy: ErrorPolicy,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Option<&'a ManifestItem>, ConversionError> {
    match select_xhtml(package, idref) {
        Ok(item) => Ok(Some(item)),
        Err(error)
            if policy == ErrorPolicy::BestEffort
                && matches!(error, ConversionError::Unsupported { .. }) =>
        {
            push_chapter_omitted(
                diagnostics,
                &package.path,
                format!("spine item {idref} was omitted: {error}"),
            )?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn handle_scripted_item(
    item: &ManifestItem,
    policy: ErrorPolicy,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), ConversionError> {
    if !item.properties.contains("scripted") {
        return Ok(());
    }
    if policy == ErrorPolicy::Strict {
        return Err(ConversionError::Unsupported {
            detail: format!("scripted EPUB spine item {} is not supported", item.path),
        });
    }
    push_spine_diagnostic(
        diagnostics,
        "epub.spine.activeContentRemoved",
        &item.path,
        "active script content was removed while preserving static chapter content".into(),
    )
}

fn push_chapter_omitted(
    diagnostics: &mut Vec<Diagnostic>,
    part: &str,
    message: String,
) -> Result<(), ConversionError> {
    push_spine_diagnostic(diagnostics, "epub.spine.chapterOmitted", part, message)
}

fn push_spine_diagnostic(
    diagnostics: &mut Vec<Diagnostic>,
    code: &'static str,
    part: &str,
    message: String,
) -> Result<(), ConversionError> {
    diagnostics
        .try_reserve(1)
        .map_err(|error| memory_limit(format!("cannot reserve EPUB spine diagnostic: {error}")))?;
    diagnostics.push(Diagnostic {
        code: code.into(),
        severity: DiagnosticSeverity::Warning,
        message,
        locator: Some(SourceLocator { part: Some(part.into()), ..SourceLocator::default() }),
    });
    Ok(())
}

fn select_xhtml<'a>(package: &'a Package, id: &str) -> Result<&'a ManifestItem, ConversionError> {
    package
        .fallback_chain(id)?
        .into_iter()
        .find(|item| matches!(item.media_type.as_str(), "application/xhtml+xml" | "text/html"))
        .ok_or_else(|| ConversionError::Unsupported {
            detail: format!("EPUB spine item {id:?} has no HTML/XHTML fallback"),
        })
}

fn memory_limit(detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}

fn locate_chapter_error(error: ConversionError, path: &str) -> ConversionError {
    match error {
        ConversionError::Malformed { part: None, detail } => {
            ConversionError::Malformed { part: Some(path.into()), detail }
        }
        error => error,
    }
}

fn chapter_input(
    item: &ManifestItem,
    prepared: &xhtml::PreparedXhtml,
    context: &ExecutionContext,
) -> Result<(ResolvedInput, FormatHint, ResourceReservation), ConversionError> {
    let size = u64::try_from(prepared.bytes.len()).unwrap_or(u64::MAX);
    let shared_plan = size
        .checked_add(u64::try_from(std::mem::size_of::<usize>() * 2).unwrap_or(u64::MAX))
        .ok_or_else(|| memory_limit("EPUB chapter Arc size overflowed"))?;
    let shared_memory = context.reserve_memory(shared_plan)?;
    let input = ResolvedInput {
        bytes: Arc::from(prepared.bytes.as_slice()),
        metadata: SourceMetadata {
            name: Some(item.path.clone()),
            media_type: Some(item.media_type.clone()),
            uri: Some(super::path::synthetic_document_url(&item.path)?),
            size,
        },
    };
    let hint = FormatHint {
        format: Some(InputFormat::Html),
        filename: Some(item.path.clone()),
        extension: item.path.rsplit_once('.').map(|(_, extension)| extension.to_owned()),
        media_type: Some(item.media_type.clone()),
        charset: Some("utf-8".into()),
    };
    Ok((input, hint, shared_memory))
}

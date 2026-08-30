//! Stable spine selection and nested HTML conversion.

use super::budget::EpubBudget;
use super::package::{ManifestItem, Package};
use super::xhtml::{self, Footnote};
use crate::zip_converter::archive_api::SafeArchive;
use into_markdown_core::{
    ConversionError, ConversionOptions, ConverterOutput, ExecutionContext, FormatHint, InputFormat,
    NestedConversionRequest, ResolvedInput, Services, SourceMetadata,
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
}

pub(super) async fn convert(
    package: &Package,
    archive: &mut SafeArchive<'_, '_>,
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
    let mut offline = options.clone();
    offline.network.enabled = false;
    offline.network.allowed_hosts.clear();
    for itemref in &package.spine {
        budget.checkpoint()?;
        if !itemref.linear {
            skipped_non_linear += 1;
            continue;
        }
        let item = select_xhtml(package, &itemref.idref)?;
        if item.properties.contains("scripted") {
            return Err(ConversionError::Unsupported {
                detail: format!("scripted EPUB spine item {} is not supported", item.path),
            });
        }
        if !selected_paths.insert(item.path.clone()) {
            return Err(ConversionError::Malformed {
                part: Some(item.path.clone()),
                detail: "multiple spine entries resolve to the same XHTML resource".into(),
            });
        }
        let entry = archive.read(&item.path)?;
        let prepared = xhtml::prepare(&item.path, &entry.bytes, archive, budget, context)?;
        drop(entry);
        let size = u64::try_from(prepared.bytes.len()).unwrap_or(u64::MAX);
        let shared_plan = size
            .checked_add(u64::try_from(std::mem::size_of::<usize>() * 2).unwrap_or(u64::MAX))
            .ok_or_else(|| memory_limit("EPUB chapter Arc size overflowed"))?;
        let _shared_memory = context.reserve_memory(shared_plan)?;
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
        let output = nested
            .convert(
                NestedConversionRequest {
                    input: &input,
                    hint: &hint,
                    options: &offline,
                    excluded_converter_ids: EXCLUDED,
                },
                context,
            )
            .await?;
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
    Ok(SpineResult { chapters, skipped_non_linear })
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

//! Stable spine selection and nested HTML conversion.

use super::budget::EpubBudget;
use super::navigation::{self, Navigation};
use super::package::{ManifestItem, Package};
use super::xhtml::{self, Footnote};
use super::xhtml_security;
use crate::zip_converter::archive_api::SafeArchive;
use into_markdown_core::{
    Block, BlockNode, ConversionError, ConversionOptions, ConverterOutput, Diagnostic,
    DiagnosticSeverity, Document, ErrorPolicy, ExecutionContext, FormatHint, Inline, InputFormat,
    NestedConversionRequest, NestedConversionService, NodeId, Provenance, ProvenanceKind,
    ResolvedInput, ResourceFailureScope, ResourceLimitSource, ResourceRecoveryAction,
    ResourceRecoveryBoundary, ResourceReservation, ResourceUnitKind, Services, SourceLocator,
    SourceMetadata, classify_resource_recovery,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
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
    pub(super) path_resolutions: BTreeMap<String, Option<String>>,
    pub(super) recovery_memory: ResourceReservation,
    pub(super) navigation: Option<Navigation>,
}

struct ChapterDispatch<'a> {
    options: &'a ConversionOptions,
    nested: &'a dyn NestedConversionService,
    context: &'a ExecutionContext,
}

impl ChapterDispatch<'_> {
    async fn convert(
        &self,
        item: &ManifestItem,
        prepared: xhtml::PreparedXhtml,
        recovery: &mut RecoveryInventory,
    ) -> Result<Option<Chapter>, ConversionError> {
        let (input, hint, shared_memory) = chapter_input(item, &prepared, self.context)?;
        let converted = self
            .nested
            .convert(
                NestedConversionRequest {
                    input: &input,
                    hint: &hint,
                    options: self.options,
                    excluded_converter_ids: EXCLUDED,
                },
                self.context,
            )
            .await;
        drop(input);
        drop(shared_memory);
        let mut output = match converted {
            Ok(output) => output,
            Err(error) if recover_chapter_resource(self.options, &item.path, &error) => {
                recovery.diagnostic(
                    "resource.max_memory_bytes.unitOmitted",
                    DiagnosticSeverity::Warning,
                    &item.path,
                    format_args!(
                        "resource limit max_memory_bytes: configured={}, observed=unknown, action=omitted 1 chapter; placeholder retained: {error}",
                        self.options.limits.max_memory_bytes
                    ),
                )?;
                return Ok(Some(omitted_placeholder_chapter(item, prepared, "resource limit")));
            }
            Err(error)
                if self.options.error_policy == ErrorPolicy::BestEffort
                    && matches!(
                        error,
                        ConversionError::Unsupported { .. } | ConversionError::Malformed { .. }
                    ) =>
            {
                recovery.diagnostic(
                    "epub.spine.chapterOmitted",
                    DiagnosticSeverity::Warning,
                    &item.path,
                    &error,
                )?;
                return Ok(Some(omitted_placeholder_chapter(
                    item,
                    prepared,
                    "unsupported or malformed chapter content",
                )));
            }
            Err(error) => return Err(error),
        };
        normalize_chapter_diagnostics(&mut output);
        Ok(Some(Chapter {
            path: item.path.clone(),
            output,
            references: prepared.references,
            internal_targets: prepared.internal_targets,
            anchors: prepared.anchors,
            footnotes: prepared.footnotes,
            resource_paths: prepared.resource_paths,
        }))
    }
}

fn recover_chapter_resource(
    options: &ConversionOptions,
    path: &str,
    error: &ConversionError,
) -> bool {
    if !matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }) {
        return false;
    }
    let locator = SourceLocator { part: Some(path.into()), ..SourceLocator::default() };
    classify_resource_recovery(
        options.error_policy,
        error,
        ResourceRecoveryBoundary {
            scope: ResourceFailureScope::ContentUnit,
            unit: ResourceUnitKind::Chapter,
            locator: Some(&locator),
            rollback_complete: true,
            fallback_retained: true,
            committed_units: 0,
            omitted_units: 1,
            limit_source: ResourceLimitSource::Explicit,
            precise_required: None,
            raised_limit: None,
        },
    ) == ResourceRecoveryAction::OmitUnit
}

fn omitted_placeholder_chapter(
    item: &ManifestItem,
    prepared: xhtml::PreparedXhtml,
    reason: &str,
) -> Chapter {
    let path = item.path.clone();
    let locator = SourceLocator { part: Some(path.clone()), ..SourceLocator::default() };
    let placeholder = BlockNode {
        id: NodeId(format!("epub-resource-omitted-{}", path.replace('/', "-"))),
        block: Block::Paragraph(vec![Inline::Text {
            value: format!(
                "[Chapter content omitted ({reason}); source location retained: {path}]"
            ),
            marks: Vec::new(),
        }]),
        provenance: Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: EPUB_ID.into(),
            locator,
            confidence: Some(1.0),
        },
    };
    Chapter {
        path,
        output: ConverterOutput::new(
            Document { blocks: vec![placeholder], ..Document::default() },
            Vec::new(),
            Vec::new(),
        ),
        references: prepared.references,
        internal_targets: prepared.internal_targets,
        anchors: prepared.anchors,
        footnotes: prepared.footnotes,
        resource_paths: prepared.resource_paths,
    }
}

struct RecoveryInventory {
    diagnostics: Vec<Diagnostic>,
    path_resolutions: BTreeMap<String, Option<String>>,
    selected_paths: BTreeSet<String>,
    memory: ResourceReservation,
}

impl RecoveryInventory {
    fn new(context: &ExecutionContext) -> Result<Self, ConversionError> {
        Ok(Self {
            diagnostics: Vec::new(),
            path_resolutions: BTreeMap::new(),
            selected_paths: BTreeSet::new(),
            memory: context.reserve_memory(0)?,
        })
    }

    fn select(&mut self, path: &str) -> Result<bool, ConversionError> {
        self.charge(path.len(), 0, 512)?;
        Ok(self.selected_paths.insert(path.into()))
    }

    fn resolve(&mut self, original: &str, selected: Option<&str>) -> Result<(), ConversionError> {
        let selected_bytes = selected.map_or(0, str::len);
        self.charge(original.len(), selected_bytes, 768)?;
        let next = selected.map(str::to_owned);
        if let Some(previous) = self.path_resolutions.insert(original.into(), next.clone())
            && previous != next
        {
            return Err(ConversionError::Malformed {
                part: Some(original.into()),
                detail: "one EPUB spine path resolves inconsistently".into(),
            });
        }
        Ok(())
    }

    fn omit(&mut self, original: &str, selected: Option<&str>) -> Result<(), ConversionError> {
        self.omit_path(original)?;
        if let Some(selected) = selected
            && selected != original
        {
            self.omit_path(selected)?;
        }
        Ok(())
    }

    fn omit_path(&mut self, path: &str) -> Result<(), ConversionError> {
        self.charge(path.len(), 0, 768)?;
        self.path_resolutions.insert(path.into(), None);
        Ok(())
    }

    fn diagnostic(
        &mut self,
        code: &'static str,
        severity: DiagnosticSeverity,
        part: &str,
        message: impl fmt::Display,
    ) -> Result<(), ConversionError> {
        let mut count = ByteCount::default();
        write!(&mut count, "{message}")
            .map_err(|_| memory_limit("EPUB recovery diagnostic size overflowed"))?;
        self.charge(code.len() + part.len(), count.bytes, 1_024)?;
        let mut rendered = String::new();
        rendered.try_reserve_exact(count.bytes).map_err(|error| {
            memory_limit(format!("cannot reserve EPUB spine diagnostic message: {error}"))
        })?;
        write!(&mut rendered, "{message}").map_err(|_| ConversionError::Internal {
            detail: "EPUB recovery diagnostic formatting failed after preflight".into(),
        })?;
        self.diagnostics.try_reserve_exact(1).map_err(|error| {
            memory_limit(format!("cannot reserve EPUB spine diagnostic: {error}"))
        })?;
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            severity,
            message: rendered,
            locator: Some(SourceLocator { part: Some(part.into()), ..SourceLocator::default() }),
        });
        Ok(())
    }

    fn charge(
        &mut self,
        first: usize,
        second: usize,
        overhead: usize,
    ) -> Result<(), ConversionError> {
        let bytes = first
            .checked_add(second)
            .and_then(|bytes| bytes.checked_add(overhead))
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(|| memory_limit("EPUB recovery inventory memory plan overflowed"))?;
        self.memory.grow(bytes)
    }
}

#[derive(Default)]
struct ByteCount {
    bytes: usize,
}

impl fmt::Write for ByteCount {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.bytes = self.bytes.checked_add(value.len()).ok_or(fmt::Error)?;
        Ok(())
    }
}

pub(super) async fn convert(
    package: &Package,
    archive: &mut SafeArchive<'_, '_>,
    mut deferred_navigation_path: Option<String>,
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
    let skipped_non_linear = package.spine.iter().filter(|item| !item.linear).count();
    let mut recovery = RecoveryInventory::new(context)?;
    let mut navigation = None;
    let offline = offline_options(options);
    let dispatch = ChapterDispatch { options: &offline, nested: nested.as_ref(), context };
    let selected_linear = chapter_limit(package, options, &mut recovery)?;
    for (linear_index, itemref) in package.spine.iter().filter(|item| item.linear).enumerate() {
        budget.checkpoint()?;
        let original = package.item(&itemref.idref)?;
        if linear_index >= selected_linear {
            recovery.omit(&original.path, None)?;
            continue;
        }
        let Some(item) = select_spine_item(
            package,
            original,
            &itemref.idref,
            options.error_policy,
            &mut recovery,
        )?
        else {
            continue;
        };
        recovery.resolve(&original.path, Some(&item.path))?;
        if !recovery.select(&item.path)? {
            return Err(ConversionError::Malformed {
                part: Some(item.path.clone()),
                detail: "multiple spine entries resolve to the same XHTML resource".into(),
            });
        }
        let entry = archive.read(&item.path)?;
        if deferred_navigation_path.as_deref() == Some(&item.path) {
            let parsed = navigation::parse_nav(
                &item.path,
                &entry.bytes,
                archive,
                budget,
                options.error_policy,
            )?;
            if parsed.entries.is_empty() {
                recovery.diagnostic(
                    "epub.navigationOmitted",
                    DiagnosticSeverity::Info,
                    &item.path,
                    "an empty EPUB navigation document was omitted",
                )?;
            } else {
                navigation = Some(parsed);
            }
            deferred_navigation_path = None;
        }
        xhtml_security::audit(&item.path, &entry.bytes, archive, budget, context)
            .map_err(|error| locate_chapter_error(error, &item.path))?;
        let prepared = match xhtml::prepare(&item.path, &entry.bytes, archive, budget, context) {
            Ok(prepared) => prepared,
            Err(error) if options.error_policy == ErrorPolicy::BestEffort && error.is_syntax() => {
                drop(entry);
                let error = locate_chapter_error(error.into_conversion_error(), &item.path);
                recovery.diagnostic(
                    "epub.spine.chapterOmitted",
                    DiagnosticSeverity::Warning,
                    &item.path,
                    &error,
                )?;
                recovery.omit(&original.path, Some(&item.path))?;
                continue;
            }
            Err(error) => {
                return Err(locate_chapter_error(error.into_conversion_error(), &item.path));
            }
        };
        handle_scripted_item(
            item,
            prepared.active_content_removed,
            options.error_policy,
            &mut recovery,
        )?;
        drop(entry);
        if let Some(chapter) = dispatch.convert(item, prepared, &mut recovery).await? {
            chapters.push(chapter);
        }
    }
    finish(
        package,
        deferred_navigation_path.is_some(),
        chapters,
        skipped_non_linear,
        recovery,
        navigation,
    )
}

fn offline_options(options: &ConversionOptions) -> ConversionOptions {
    let mut offline = options.clone();
    offline.network.enabled = false;
    offline.network.allowed_hosts.clear();
    offline
}

fn chapter_limit(
    package: &Package,
    options: &ConversionOptions,
    recovery: &mut RecoveryInventory,
) -> Result<usize, ConversionError> {
    let observed = package.spine.iter().filter(|item| item.linear).count();
    let selected = usize::try_from(options.limits.max_pages).unwrap_or(usize::MAX);
    if observed <= selected {
        return Ok(selected);
    }
    let first_omitted = package
        .spine
        .iter()
        .filter(|item| item.linear)
        .nth(selected)
        .and_then(|item| package.item(&item.idref).ok())
        .map_or(package.path.as_str(), |item| item.path.as_str());
    let error = ConversionError::ResourceLimit {
        limit: "max_pages",
        detail: format!("{observed} EPUB chapters > {}", options.limits.max_pages),
    };
    if options.error_policy == ErrorPolicy::Strict {
        return Err(error);
    }
    recovery.diagnostic(
        "resource.max_pages.sequenceTruncated",
        DiagnosticSeverity::Warning,
        first_omitted,
        format_args!(
            "resource limit max_pages: configured={}, observed={observed}, action=kept {selected} chapters and omitted {} subsequent chapters",
            options.limits.max_pages,
            observed.saturating_sub(selected)
        ),
    )?;
    Ok(selected)
}

fn finish(
    package: &Package,
    deferred_navigation_pending: bool,
    chapters: Vec<Chapter>,
    skipped_non_linear: usize,
    recovery: RecoveryInventory,
    navigation: Option<Navigation>,
) -> Result<SpineResult, ConversionError> {
    if chapters.is_empty() {
        return Err(ConversionError::Malformed {
            part: Some(package.path.clone()),
            detail: "EPUB spine has no linear XHTML content".into(),
        });
    }
    if deferred_navigation_pending {
        return Err(ConversionError::Internal {
            detail: "deferred EPUB navigation was not consumed by the linear spine".into(),
        });
    }
    Ok(SpineResult {
        chapters,
        skipped_non_linear,
        diagnostics: recovery.diagnostics,
        path_resolutions: recovery.path_resolutions,
        recovery_memory: recovery.memory,
        navigation,
    })
}

fn normalize_chapter_diagnostics(output: &mut ConverterOutput) {
    for diagnostic in &mut output.diagnostics {
        if matches!(
            diagnostic.code.as_str(),
            "html.parseRecovered" | "html.sourceLocationUnavailable"
        ) {
            diagnostic.severity = DiagnosticSeverity::Info;
        }
    }
}

fn select_spine_item<'a>(
    package: &'a Package,
    original: &'a ManifestItem,
    idref: &str,
    policy: ErrorPolicy,
    recovery: &mut RecoveryInventory,
) -> Result<Option<&'a ManifestItem>, ConversionError> {
    match select_xhtml(package, idref) {
        Ok(item) => Ok(Some(item)),
        Err(error)
            if policy == ErrorPolicy::BestEffort
                && matches!(error, ConversionError::Unsupported { .. }) =>
        {
            recovery.diagnostic(
                "epub.spine.chapterOmitted",
                DiagnosticSeverity::Warning,
                &original.path,
                format_args!("spine item {idref} was omitted: {error}"),
            )?;
            recovery.omit(&original.path, None)?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn handle_scripted_item(
    item: &ManifestItem,
    active_content_removed: bool,
    policy: ErrorPolicy,
    recovery: &mut RecoveryInventory,
) -> Result<(), ConversionError> {
    let declared = item.properties.contains("scripted");
    if !declared && !active_content_removed {
        return Ok(());
    }
    if policy == ErrorPolicy::Strict {
        return Err(ConversionError::Unsupported {
            detail: format!("active EPUB spine item {} is not supported", item.path),
        });
    }
    let message = if active_content_removed {
        "active script content or event handlers were removed while preserving static chapter content"
    } else {
        "a producer-declared scripted chapter was converted as inert static content"
    };
    recovery.diagnostic(
        "epub.spine.activeContentRemoved",
        DiagnosticSeverity::Warning,
        &item.path,
        message,
    )
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

pub(super) fn linear_spine_uses_path(
    package: &Package,
    path: &str,
) -> Result<bool, ConversionError> {
    for itemref in &package.spine {
        if !itemref.linear {
            continue;
        }
        match select_xhtml(package, &itemref.idref) {
            Ok(item) if item.path == path => return Ok(true),
            Ok(_) | Err(ConversionError::Unsupported { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
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

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{CancellationToken, ErrorCode, ExecutionOptions, ResourceLimits};

    fn context(memory: u64) -> ExecutionContext {
        ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: memory, ..ResourceLimits::default() },
        )
    }

    #[test]
    fn recovery_inventory_leases_one_hundred_thousand_omissions_and_releases() {
        let context = context(512 * 1024 * 1024);
        let mut inventory = RecoveryInventory::new(&context).unwrap();
        for index in 0..100_000 {
            let path = format!("OPS/chapter-{index:06}.xhtml");
            inventory
                .diagnostic(
                    "epub.spine.chapterOmitted",
                    DiagnosticSeverity::Warning,
                    &path,
                    "locally malformed chapter omitted",
                )
                .unwrap();
            inventory.omit(&path, None).unwrap();
        }
        assert_eq!(inventory.diagnostics.len(), 100_000);
        assert_eq!(inventory.path_resolutions.len(), 100_000);
        assert!(context.reserved_memory_bytes() > 100_000);
        drop(inventory);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn recovery_inventory_low_memory_cancellation_and_error_release() {
        let low = context(2_048);
        let mut inventory = RecoveryInventory::new(&low).unwrap();
        let error = inventory
            .diagnostic(
                "epub.spine.chapterOmitted",
                DiagnosticSeverity::Warning,
                "OPS/large.xhtml",
                format_args!("{:>4096}", ""),
            )
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        drop(inventory);
        assert_eq!(low.reserved_memory_bytes(), 0);

        let cancellation = CancellationToken::new();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation: cancellation.clone(), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let mut inventory = RecoveryInventory::new(&cancelled).unwrap();
        cancellation.cancel();
        let error = inventory.omit("OPS/cancelled.xhtml", None).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);
        drop(inventory);
        assert_eq!(cancelled.reserved_memory_bytes(), 0);

        let context = context(64 * 1024);
        let mut inventory = RecoveryInventory::new(&context).unwrap();
        inventory.resolve("OPS/original.svg", Some("OPS/one.xhtml")).unwrap();
        let error = inventory.resolve("OPS/original.svg", Some("OPS/two.xhtml")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Malformed);
        drop(inventory);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn omitted_chapter_placeholder_is_visible_located_and_valid() {
        let context = context(64 * 1024);
        let item = ManifestItem {
            id: "chapter".into(),
            path: "OPS/chapter.xhtml".into(),
            media_type: "application/xhtml+xml".into(),
            properties: BTreeSet::new(),
            fallback: None,
        };
        let prepared = xhtml::PreparedXhtml {
            bytes: Vec::new(),
            references: BTreeMap::new(),
            internal_targets: BTreeSet::new(),
            anchors: BTreeSet::new(),
            footnotes: Vec::new(),
            resource_paths: BTreeSet::new(),
            active_content_removed: false,
            _memory: context.reserve_memory(0).unwrap(),
        };
        let chapter = omitted_placeholder_chapter(&item, prepared, "resource limit");
        chapter.output.document.validate().unwrap();
        let node = chapter.output.document.blocks.first().unwrap();
        assert_eq!(node.provenance.locator.part.as_deref(), Some("OPS/chapter.xhtml"));
        let Block::Paragraph(content) = &node.block else { panic!("paragraph expected") };
        assert!(
            matches!(content.first(), Some(Inline::Text { value, .. }) if value.contains("resource limit"))
        );
    }
}

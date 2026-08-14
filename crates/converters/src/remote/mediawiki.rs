//! Policy-constrained `MediaWiki` page acquisition and semantic conversion.
//!
//! Threat model: the article URL, title, DNS answers, redirects, HTTP response,
//! and API JSON are all untrusted. The resolver performs no I/O unless the
//! caller enabled networking; the shared transport is the sole DNS/connect/TLS
//! boundary and re-authorizes every redirect address. Source and parser memory
//! are covered by request-bound reservations before allocation, and malformed,
//! missing, oversized, cancelled, and timed-out responses fail closed.

use super::redirect::source_redirects;
use crate::html::convert_html;
use into_markdown_core::{
    Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput,
    Diagnostic, DiagnosticSeverity, ExecutionContext, FormatCandidate, FormatDetector, FormatHint,
    Inline, InputFormat, InputRef, ProbeOutcome, ProvenanceKind, ResolvedInput, ResolvedSource,
    Services, SourceLocator, SourceMetadata, SourceResolutionMetadata, SourceResolver,
};
use into_markdown_http_transport::HttpClient;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::mem::size_of;
use std::sync::Arc;
use unicode_normalization::UnicodeNormalization;
use url::Url;

const PROVIDER_ID: &str = "builtin.converter.mediawiki";
const SOURCE_ID: &str = "builtin.source.mediawiki";
const JSON_MEDIA_TYPE: &str = "application/json";
// The HTTP transport deliberately strips every server-supplied Content-Type
// parameter. Only this resolver can add the internal parameter after it has
// authenticated the final endpoint and strict JSON base media type.
pub(crate) const AUTHENTICATED_MEDIA_TYPE: &str =
    "application/json; x-into-markdown-resolver=mediawiki";
const FORMATS: &[InputFormat] = &[InputFormat::Wikipedia];
const MAX_TITLE_BYTES: usize = 4 * 1024;
const MAX_JSON_KEY_BYTES: usize = 256;
const MAX_JSON_NUMBER_BYTES: usize = 128;
const MAX_API_COLLECTION_ITEMS: usize = 100_000;
const MAX_TRANSPORT_URL_BYTES: usize = 8 * 1024;
const RESOLVER_FIXED_MEMORY: usize = 32 * 1024;
// serde_json 1.0.151's retained strings are no larger than their encoded
// source slices. The smallest accepted array element is two bytes (`{}` or
// `""`); a 24-byte Cow/Vec slot and sub-2x geometric spare capacity therefore
// stay below a 24x source ratio. The remaining 8x covers owned decoded strings
// and the simultaneous HTML Arc handoff; the fixed margin covers the envelope
// and allocator small-object classes. The engine's outer converter reservation
// authenticates this complete child lease before serde_json is entered.
const API_JSON_MEMORY_FACTOR: u64 = 32;
const API_JSON_MEMORY_BASE: u64 = 64 * 1024;

/// Content detector for a validated `MediaWiki` `action=parse` transport shape.
#[derive(Debug, Default)]
pub struct MediaWikiFormatDetector;

impl FormatDetector for MediaWikiFormatDetector {
    fn id(&self) -> &'static str {
        "builtin.detector.mediawiki"
    }

    fn priority(&self) -> i32 {
        240
    }

    fn detect<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatHint,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            let matches = input.metadata.media_type.as_deref() == Some(AUTHENTICATED_MEDIA_TYPE)
                && input.metadata.uri.as_deref().is_some_and(looks_like_api_endpoint)
                && first_non_whitespace(&input.bytes, context)? == Some(b'{');
            Ok(matches
                .then(|| FormatCandidate::new(InputFormat::Wikipedia, 0.99, "MediaWiki API"))
                .into_iter()
                .collect())
        })
    }
}

fn first_non_whitespace(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Option<u8>, ConversionError> {
    for (index, byte) in bytes.iter().copied().enumerate() {
        if index.is_multiple_of(256) {
            context.checkpoint()?;
        }
        if !byte.is_ascii_whitespace() {
            return Ok(Some(byte));
        }
    }
    context.checkpoint()?;
    Ok(None)
}

/// `MediaWiki` article resolver backed exclusively by the audited HTTP client.
#[derive(Clone)]
pub struct MediaWikiSourceResolver {
    client: Arc<HttpClient>,
}

impl std::fmt::Debug for MediaWikiSourceResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("MediaWikiSourceResolver").finish_non_exhaustive()
    }
}

impl Default for MediaWikiSourceResolver {
    fn default() -> Self {
        Self { client: Arc::new(HttpClient::default()) }
    }
}

impl MediaWikiSourceResolver {
    /// Construct a resolver over an injected audited client.
    #[must_use]
    pub fn with_client(client: Arc<HttpClient>) -> Self {
        Self { client }
    }

    fn resolve_accounted_owned(
        &self,
        input: &InputRef,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<crate::BlockingFuture<ResolvedSource>, ConversionError> {
        context.checkpoint()?;
        let InputRef::Uri(source) = input else {
            return Err(ConversionError::Unsupported {
                detail: "MediaWiki source resolver accepts only article URIs".into(),
            });
        };
        if !options.network.enabled {
            return Err(ConversionError::Network {
                detail: "network resolution is disabled by default".into(),
            });
        }
        let retained_bytes = resolver_retained_memory(source, options)?;
        // This lease precedes every resolver-owned clone/allocation and remains
        // attached to ResolvedSource. The engine resizes only the separate body
        // lease, so URL/policy/redirect/source metadata stay authenticated.
        let retained_memory = context.reserve_memory(retained_bytes)?;
        let source = source.clone();
        let policy = super::network_policy(options);
        let limits = super::fetch_limits(options);
        let client = Arc::clone(&self.client);
        let worker_context = context.clone();
        super::http_pool()?.submit(move || {
            let (api_url, _) = mediawiki_urls(&source, &worker_context)?;
            let resource = client
                .get(api_url.as_str(), &policy, limits, &worker_context)
                .map_err(super::map_transport_error)?;
            let (bytes, reservation, final_url, media_type, filename, redirects) =
                resource.into_parts();
            validate_fetch_identity(&api_url, &final_url, media_type.as_deref(), &redirects)?;
            worker_context.checkpoint()?;
            let size = u64::try_from(bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_input_bytes",
                detail: "MediaWiki API response size cannot be represented as u64".into(),
            })?;
            Ok(ResolvedSource::with_memory_reservation(
                ResolvedInput {
                    bytes,
                    metadata: SourceMetadata {
                        name: filename,
                        media_type: Some(AUTHENTICATED_MEDIA_TYPE.into()),
                        uri: Some(final_url),
                        size,
                    },
                },
                reservation,
            )
            .with_retained_metadata_memory(retained_memory)
            .with_resolution_metadata(SourceResolutionMetadata {
                redirects: source_redirects(redirects),
            }))
        })
    }
}

fn resolver_retained_memory(
    source: &str,
    options: &ConversionOptions,
) -> Result<u64, ConversionError> {
    let allowlist = options
        .network
        .allowed_hosts
        .iter()
        .try_fold(0_usize, |total, host| total.checked_add(host.len()))
        .ok_or_else(resolver_memory_limit)?;
    let redirect_slots = usize::from(options.network.max_redirects)
        .checked_mul(
            MAX_TRANSPORT_URL_BYTES
                .checked_mul(2)
                .and_then(|bytes| {
                    bytes.checked_add(size_of::<into_markdown_core::SourceRedirect>())
                })
                .ok_or_else(resolver_memory_limit)?,
        )
        .ok_or_else(resolver_memory_limit)?;
    let bytes = source
        .len()
        .checked_mul(12)
        .and_then(|bytes| bytes.checked_add(allowlist))
        .and_then(|bytes| bytes.checked_add(redirect_slots))
        .and_then(|bytes| bytes.checked_add(RESOLVER_FIXED_MEMORY))
        .ok_or_else(resolver_memory_limit)?;
    u64::try_from(bytes).map_err(|_| resolver_memory_limit())
}

fn resolver_memory_limit() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "MediaWiki resolver retained-memory plan overflowed".into(),
    }
}

impl SourceResolver for MediaWikiSourceResolver {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn priority(&self) -> i32 {
        100
    }

    fn supports(&self, input: &InputRef) -> bool {
        matches!(input, InputRef::Uri(value) if looks_like_mediawiki_article(value))
    }

    fn resolve<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
        Box::pin(async move {
            self.resolve_accounted_owned(input, options, context)?
                .await
                .map(ResolvedSource::into_input)
        })
    }

    fn resolve_accounted<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedSource, ConversionError>> {
        Box::pin(async move { self.resolve_accounted_owned(input, options, context)?.await })
    }
}

/// Converter for the bounded `action=parse` response produced by the resolver.
#[derive(Debug, Default)]
pub struct MediaWikiConverter;

impl Converter for MediaWikiConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        230
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
            Ok(
                if candidate.format == InputFormat::Wikipedia
                    && input.metadata.media_type.as_deref() == Some(AUTHENTICATED_MEDIA_TYPE)
                    && input.metadata.uri.as_deref().is_some_and(looks_like_api_endpoint)
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
        Box::pin(async move { convert_mediawiki(input, options, context) })
    }
}

#[derive(Deserialize)]
struct ApiEnvelope<'a> {
    #[serde(borrow)]
    requestid: Cow<'a, str>,
    #[serde(borrow)]
    parse: Option<ApiPage<'a>>,
    #[serde(borrow)]
    error: Option<ApiError<'a>>,
    #[serde(borrow)]
    curtimestamp: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct ApiPage<'a> {
    #[serde(borrow)]
    title: Cow<'a, str>,
    pageid: u64,
    revid: u64,
    #[serde(borrow)]
    text: Cow<'a, str>,
    #[serde(default, borrow)]
    sections: Vec<ApiSection<'a>>,
    #[serde(default, borrow)]
    links: Vec<ApiLink<'a>>,
    #[serde(default, borrow)]
    images: Vec<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct ApiSection<'a> {
    #[serde(default, borrow)]
    line: Cow<'a, str>,
}

#[derive(Deserialize)]
struct ApiLink<'a> {
    #[serde(default, borrow)]
    title: Cow<'a, str>,
}

#[derive(Deserialize)]
struct ApiError<'a> {
    #[serde(borrow)]
    code: Cow<'a, str>,
}

fn convert_mediawiki(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let input_size = u64::try_from(input.bytes.len()).unwrap_or(u64::MAX);
    if input_size > options.limits.max_input_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: format!("{input_size} > {}", options.limits.max_input_bytes),
        });
    }
    if input.metadata.media_type.as_deref() != Some(AUTHENTICATED_MEDIA_TYPE) {
        return Err(mediawiki_malformed("mediawiki.invalidEnvelope"));
    }
    let api_url = input
        .metadata
        .uri
        .as_deref()
        .ok_or_else(|| mediawiki_malformed("mediawiki.apiUrlMissing"))?;
    if !looks_like_api_endpoint(api_url) {
        return Err(mediawiki_malformed("mediawiki.invalidApiEndpoint"));
    }
    let parser_peak = input_size
        .checked_mul(API_JSON_MEMORY_FACTOR)
        .and_then(|bytes| bytes.checked_add(API_JSON_MEMORY_BASE))
        .ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "MediaWiki API parser memory model overflowed".into(),
        })?;
    let _api_memory = context.reserve_memory(parser_peak)?;
    valid_api_endpoint(api_url)?;
    preflight_mediawiki_json(&input.bytes, options, context)?;
    let envelope: ApiEnvelope<'_> = serde_json::from_slice(&input.bytes)
        .map_err(|_| mediawiki_malformed("mediawiki.invalidJson"))?;
    if let Some(error) = envelope.error {
        return Err(if matches!(error.code.as_ref(), "missingtitle" | "pagecannotexist") {
            mediawiki_malformed("mediawiki.pageMissing")
        } else {
            mediawiki_malformed("mediawiki.apiError")
        });
    }
    let page = envelope.parse.ok_or_else(|| mediawiki_malformed("mediawiki.invalidEnvelope"))?;
    validate_page(&page, envelope.curtimestamp.as_deref())?;
    let retrieved_at = envelope.curtimestamp.unwrap_or_default();
    let requested_title = normalize_api_title(&envelope.requestid)?;
    let canonical_title = normalize_api_title(&page.title)?;
    let source_url = source_url_from_api(api_url, &canonical_title)?;

    let html_bytes: Arc<[u8]> = Arc::from(page.text.into_owned().into_bytes());
    let html_size =
        u64::try_from(html_bytes.len()).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: "MediaWiki HTML size cannot be represented as u64".into(),
        })?;
    let html_input = ResolvedInput {
        bytes: html_bytes,
        metadata: SourceMetadata {
            name: Some("mediawiki.html".into()),
            media_type: Some("text/html; charset=utf-8".into()),
            uri: Some(source_url.clone()),
            size: html_size,
        },
    };
    let mut output = convert_html(&html_input, options, context)?;
    output.document.metadata.title = Some(canonical_title.clone());
    bind_mediawiki_provenance(
        &mut output,
        &source_url,
        page.pageid,
        page.revid,
        &retrieved_at,
        context,
    )?;
    output
        .document
        .metadata
        .properties
        .insert("mediawiki.sectionCount".into(), page.sections.len().to_string());
    output
        .document
        .metadata
        .properties
        .insert("mediawiki.linkCount".into(), page.links.len().to_string());
    output
        .document
        .metadata
        .properties
        .insert("mediawiki.imageCount".into(), page.images.len().to_string());
    if requested_title != canonical_title {
        output.diagnostics.push(Diagnostic {
            code: "mediawiki.titleRedirected".into(),
            severity: DiagnosticSeverity::Info,
            message: format!("MediaWiki resolved {requested_title:?} to {canonical_title:?}"),
            locator: None,
        });
    }
    Ok(output)
}

fn validate_page(page: &ApiPage<'_>, timestamp: Option<&str>) -> Result<(), ConversionError> {
    if page.pageid == 0
        || page.revid == 0
        || invalid_title(&page.title)
        || page.text.trim().is_empty()
        || !timestamp.is_some_and(valid_timestamp)
        || page.sections.iter().any(|section| invalid_title(&section.line))
        || page.links.iter().any(|link| invalid_title(&link.title))
        || page.images.iter().any(|image| invalid_title(image))
    {
        return Err(mediawiki_malformed("mediawiki.invalidEnvelope"));
    }
    Ok(())
}

fn bind_mediawiki_provenance(
    output: &mut ConverterOutput,
    source_url: &str,
    page_id: u64,
    revision_id: u64,
    retrieved_at: &str,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    if source_url.len() > MAX_TRANSPORT_URL_BYTES
        || Url::parse(source_url).ok().is_none_or(|url| {
            !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || url.as_str() != source_url
        })
        || !valid_timestamp(retrieved_at)
    {
        return Err(mediawiki_malformed("mediawiki.invalidProvenance"));
    }
    let node_count = count_blocks(&output.document.blocks, context)?;
    let binding_bytes = node_count
        .checked_mul(PROVIDER_ID.len().checked_add(128).ok_or_else(binding_memory_limit)?)
        .and_then(|bytes| bytes.checked_add(source_url.len()))
        .and_then(|bytes| bytes.checked_add(retrieved_at.len()))
        .and_then(|bytes| bytes.checked_add(MAX_TITLE_BYTES.checked_mul(3)?))
        .and_then(|bytes| bytes.checked_add(8 * 512))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or_else(binding_memory_limit)?;
    let binding_memory = context.reserve_memory(binding_bytes)?;

    rewrite_block_provenance(&mut output.document.blocks, context)?;
    let properties = &mut output.document.metadata.properties;
    properties.insert("mediawiki.provider".into(), PROVIDER_ID.into());
    properties.insert("mediawiki.sourceUrl".into(), source_url.into());
    properties.insert("mediawiki.pageId".into(), page_id.to_string());
    properties.insert("mediawiki.revisionId".into(), revision_id.to_string());
    properties.insert("mediawiki.retrievedAt".into(), retrieved_at.into());
    output.attach_memory_reservation(context, binding_memory)
}

fn binding_memory_limit() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "MediaWiki source-binding memory plan overflowed".into(),
    }
}

fn count_blocks(nodes: &[BlockNode], context: &ExecutionContext) -> Result<usize, ConversionError> {
    fn visit_inlines(
        values: &[Inline],
        count: &mut usize,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        for value in values {
            if let Inline::SourceText { .. } = value {
                *count = count.checked_add(1).ok_or_else(binding_memory_limit)?;
            }
            if let Inline::Link { content, .. } = value {
                visit_inlines(content, count, context)?;
            }
            if count.is_multiple_of(256) {
                context.checkpoint()?;
            }
        }
        Ok(())
    }

    fn visit(
        nodes: &[BlockNode],
        count: &mut usize,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        for node in nodes {
            if count.is_multiple_of(256) {
                context.checkpoint()?;
            }
            *count = count.checked_add(1).ok_or_else(binding_memory_limit)?;
            match &node.block {
                Block::Paragraph(inlines)
                | Block::Heading { content: inlines, .. }
                | Block::TimedSegment { content: inlines, .. } => {
                    visit_inlines(inlines, count, context)?;
                }
                Block::List { items, .. } => {
                    for item in items {
                        visit(&item.blocks, count, context)?;
                    }
                }
                Block::Table { rows, .. } => {
                    for cell in rows.iter().flat_map(|row| &row.cells) {
                        visit(&cell.blocks, count, context)?;
                    }
                }
                Block::Footnote { blocks, .. }
                | Block::Page { blocks, .. }
                | Block::Slide { blocks, .. }
                | Block::Sheet { blocks, .. } => visit(blocks, count, context)?,
                _ => {}
            }
        }
        Ok(())
    }
    let mut count = 0;
    visit(nodes, &mut count, context)?;
    Ok(count)
}

fn rewrite_block_provenance(
    nodes: &mut [BlockNode],
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    fn rewrite_inline(
        values: &mut [Inline],
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        for (index, value) in values.iter_mut().enumerate() {
            if index.is_multiple_of(256) {
                context.checkpoint()?;
            }
            match value {
                Inline::SourceText { provenance, .. } => {
                    provenance.kind = ProvenanceKind::NativeParser;
                    provenance.provider.clear();
                    provenance.provider.push_str(PROVIDER_ID);
                    provenance.locator = SourceLocator::default();
                }
                Inline::Link { content, .. } => rewrite_inline(content, context)?,
                _ => {}
            }
        }
        Ok(())
    }

    for node in nodes {
        context.checkpoint()?;
        node.provenance.kind = ProvenanceKind::NativeParser;
        node.provenance.provider.clear();
        node.provenance.provider.push_str(PROVIDER_ID);
        node.provenance.locator = SourceLocator::default();
        match &mut node.block {
            Block::Paragraph(inlines)
            | Block::Heading { content: inlines, .. }
            | Block::TimedSegment { content: inlines, .. } => rewrite_inline(inlines, context)?,
            Block::List { items, .. } => {
                for item in items {
                    rewrite_block_provenance(&mut item.blocks, context)?;
                }
            }
            Block::Table { rows, .. } => {
                for cell in rows.iter_mut().flat_map(|row| &mut row.cells) {
                    rewrite_block_provenance(&mut cell.blocks, context)?;
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => rewrite_block_provenance(blocks, context)?,
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JsonObjectState {
    KeyOrEnd,
    Value,
    CommaOrEnd,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JsonArrayState {
    ValueOrEnd,
    Value,
    CommaOrEnd,
}

enum JsonShapeFrame {
    Object { state: JsonObjectState, keys: BTreeSet<String>, members: usize },
    Array { state: JsonArrayState, members: usize },
}

struct JsonShapeScanner<'a> {
    bytes: &'a [u8],
    offset: usize,
    next_checkpoint: usize,
    max_depth: usize,
    max_string: usize,
    max_items: usize,
    values: usize,
    context: &'a ExecutionContext,
    _key_memory: into_markdown_core::ResourceReservation,
}

#[cfg(test)]
std::thread_local! {
    static JSON_SCAN_CANCEL_AT: std::cell::RefCell<Option<(usize, into_markdown_core::CancellationToken)>> = const { std::cell::RefCell::new(None) };
    static JSON_SCAN_CHECKPOINTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn preflight_mediawiki_json(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let max_string = usize::try_from(options.limits.max_field_bytes).unwrap_or(usize::MAX);
    let max_items = usize::try_from(options.limits.max_table_cells)
        .unwrap_or(usize::MAX)
        .min(MAX_API_COLLECTION_ITEMS);
    let key_memory_bytes = bytes
        .len()
        .checked_mul(6)
        .and_then(|value| value.checked_add(16 * 1024))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(json_shape_memory_limit)?;
    let key_memory = context.reserve_memory(key_memory_bytes)?;
    let mut scanner = JsonShapeScanner {
        bytes,
        offset: 0,
        next_checkpoint: 0,
        max_depth: usize::from(options.limits.max_nesting_depth),
        max_string,
        max_items,
        values: 0,
        context,
        _key_memory: key_memory,
    };
    scanner.scan()
}

fn json_shape_memory_limit() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "MediaWiki JSON shape memory plan overflowed".into(),
    }
}

impl JsonShapeScanner<'_> {
    #[allow(clippy::too_many_lines)]
    fn scan(&mut self) -> Result<(), ConversionError> {
        let mut frames = Vec::<JsonShapeFrame>::new();
        let mut root_seen = false;
        loop {
            self.checkpoint()?;
            self.space()?;
            if frames.is_empty() && root_seen {
                return if self.offset == self.bytes.len() {
                    self.context.checkpoint()
                } else {
                    Err(mediawiki_malformed("mediawiki.invalidJson"))
                };
            }

            let mut closed = false;
            if let Some(frame) = frames.last_mut() {
                match frame {
                    JsonShapeFrame::Object { state, keys, members } => match state {
                        JsonObjectState::KeyOrEnd => {
                            if self.take(b'}') {
                                closed = true;
                            } else {
                                let key = self
                                    .string(true)?
                                    .ok_or_else(|| mediawiki_malformed("mediawiki.invalidJson"))?;
                                if !keys.insert(key) {
                                    return Err(mediawiki_malformed(
                                        "mediawiki.duplicateObjectField",
                                    ));
                                }
                                *members = members.checked_add(1).ok_or_else(collection_limit)?;
                                if *members > self.max_items {
                                    return Err(collection_limit());
                                }
                                self.space()?;
                                self.expect(b':')?;
                                *state = JsonObjectState::Value;
                                continue;
                            }
                        }
                        JsonObjectState::Value => {}
                        JsonObjectState::CommaOrEnd => {
                            if self.take(b'}') {
                                closed = true;
                            } else {
                                self.expect(b',')?;
                                self.space()?;
                                if self.bytes.get(self.offset) == Some(&b'}') {
                                    return Err(mediawiki_malformed("mediawiki.invalidJson"));
                                }
                                *state = JsonObjectState::KeyOrEnd;
                                continue;
                            }
                        }
                    },
                    JsonShapeFrame::Array { state, .. } => match state {
                        JsonArrayState::ValueOrEnd => {
                            if self.take(b']') {
                                closed = true;
                            }
                        }
                        JsonArrayState::Value => {}
                        JsonArrayState::CommaOrEnd => {
                            if self.take(b']') {
                                closed = true;
                            } else {
                                self.expect(b',')?;
                                self.space()?;
                                if self.bytes.get(self.offset) == Some(&b']') {
                                    return Err(mediawiki_malformed("mediawiki.invalidJson"));
                                }
                                *state = JsonArrayState::Value;
                                continue;
                            }
                        }
                    },
                }
            }
            if closed {
                frames.pop();
                continue;
            }

            if let Some(frame) = frames.last_mut() {
                match frame {
                    JsonShapeFrame::Object { state, .. } if *state == JsonObjectState::Value => {
                        *state = JsonObjectState::CommaOrEnd;
                    }
                    JsonShapeFrame::Array { state, members }
                        if matches!(*state, JsonArrayState::ValueOrEnd | JsonArrayState::Value) =>
                    {
                        *members = members.checked_add(1).ok_or_else(collection_limit)?;
                        if *members > self.max_items {
                            return Err(collection_limit());
                        }
                        *state = JsonArrayState::CommaOrEnd;
                    }
                    _ => return Err(mediawiki_malformed("mediawiki.invalidJson")),
                }
            } else if root_seen {
                return Err(mediawiki_malformed("mediawiki.invalidJson"));
            } else {
                root_seen = true;
            }

            self.values = self.values.checked_add(1).ok_or_else(collection_limit)?;
            if self.values > self.max_items {
                return Err(collection_limit());
            }
            match self.bytes.get(self.offset).copied() {
                Some(b'{') => {
                    self.begin_container(frames.len())?;
                    self.offset += 1;
                    frames.push(JsonShapeFrame::Object {
                        state: JsonObjectState::KeyOrEnd,
                        keys: BTreeSet::new(),
                        members: 0,
                    });
                }
                Some(b'[') => {
                    self.begin_container(frames.len())?;
                    self.offset += 1;
                    frames.push(JsonShapeFrame::Array {
                        state: JsonArrayState::ValueOrEnd,
                        members: 0,
                    });
                }
                Some(b'"') => {
                    self.string(false)?;
                }
                Some(b'-' | b'0'..=b'9') => self.number()?,
                Some(b't') => self.literal(b"true")?,
                Some(b'f') => self.literal(b"false")?,
                Some(b'n') => self.literal(b"null")?,
                _ => return Err(mediawiki_malformed("mediawiki.invalidJson")),
            }
        }
    }

    fn begin_container(&self, current_depth: usize) -> Result<(), ConversionError> {
        if current_depth >= self.max_depth {
            Err(ConversionError::ResourceLimit {
                limit: "max_nesting_depth",
                detail: "MediaWiki JSON exceeded its nesting-depth budget".into(),
            })
        } else {
            Ok(())
        }
    }

    fn checkpoint(&mut self) -> Result<(), ConversionError> {
        if self.offset >= self.next_checkpoint {
            #[cfg(test)]
            {
                let count = JSON_SCAN_CHECKPOINTS.get().saturating_add(1);
                JSON_SCAN_CHECKPOINTS.set(count);
                JSON_SCAN_CANCEL_AT.with(|slot| {
                    if let Some((trigger, token)) = slot.borrow().as_ref()
                        && count == *trigger
                    {
                        token.cancel();
                    }
                });
            }
            self.context.checkpoint()?;
            self.next_checkpoint = self.offset.saturating_add(256);
        }
        Ok(())
    }

    fn space(&mut self) -> Result<(), ConversionError> {
        while self.bytes.get(self.offset).is_some_and(u8::is_ascii_whitespace) {
            self.offset += 1;
            self.checkpoint()?;
        }
        Ok(())
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.offset) == Some(&byte) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), ConversionError> {
        if self.take(byte) { Ok(()) } else { Err(mediawiki_malformed("mediawiki.invalidJson")) }
    }

    fn string(&mut self, retain: bool) -> Result<Option<String>, ConversionError> {
        self.expect(b'"')?;
        let mut decoded = retain.then(String::new);
        let mut decoded_bytes = 0_usize;
        loop {
            self.checkpoint()?;
            let byte = *self
                .bytes
                .get(self.offset)
                .ok_or_else(|| mediawiki_malformed("mediawiki.invalidJson"))?;
            match byte {
                b'"' => {
                    self.offset += 1;
                    return Ok(decoded);
                }
                0..=0x1f => return Err(mediawiki_malformed("mediawiki.invalidJson")),
                b'\\' => {
                    self.offset += 1;
                    let escape = *self
                        .bytes
                        .get(self.offset)
                        .ok_or_else(|| mediawiki_malformed("mediawiki.invalidJson"))?;
                    self.offset += 1;
                    let character = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{0008}',
                        b'f' => '\u{000c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode_escape()?,
                        _ => return Err(mediawiki_malformed("mediawiki.invalidJson")),
                    };
                    decoded_bytes =
                        decoded_bytes.checked_add(character.len_utf8()).ok_or_else(string_limit)?;
                    if let Some(value) = decoded.as_mut() {
                        value.push(character);
                    }
                }
                _ => {
                    let width = match byte {
                        0x00..=0x7f => 1,
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(mediawiki_malformed("mediawiki.invalidJson")),
                    };
                    let end = self
                        .offset
                        .checked_add(width)
                        .ok_or_else(|| mediawiki_malformed("mediawiki.invalidJson"))?;
                    let character = std::str::from_utf8(
                        self.bytes
                            .get(self.offset..end)
                            .ok_or_else(|| mediawiki_malformed("mediawiki.invalidJson"))?,
                    )
                    .ok()
                    .and_then(|value| value.chars().next())
                    .ok_or_else(|| mediawiki_malformed("mediawiki.invalidJson"))?;
                    self.offset = end;
                    decoded_bytes =
                        decoded_bytes.checked_add(character.len_utf8()).ok_or_else(string_limit)?;
                    if let Some(value) = decoded.as_mut() {
                        value.push(character);
                    }
                }
            }
            let limit =
                if retain { self.max_string.min(MAX_JSON_KEY_BYTES) } else { self.max_string };
            if decoded_bytes > limit {
                return Err(string_limit());
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, ConversionError> {
        let first = self.hex_quad()?;
        let scalar = if (0xd800..=0xdbff).contains(&first) {
            if self.bytes.get(self.offset..self.offset.saturating_add(2)) != Some(b"\\u") {
                return Err(mediawiki_malformed("mediawiki.invalidJson"));
            }
            self.offset += 2;
            let second = self.hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(mediawiki_malformed("mediawiki.invalidJson"));
            }
            0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(mediawiki_malformed("mediawiki.invalidJson"));
        } else {
            u32::from(first)
        };
        char::from_u32(scalar).ok_or_else(|| mediawiki_malformed("mediawiki.invalidJson"))
    }

    fn hex_quad(&mut self) -> Result<u16, ConversionError> {
        let mut value = 0_u16;
        for _ in 0..4 {
            let byte = *self
                .bytes
                .get(self.offset)
                .ok_or_else(|| mediawiki_malformed("mediawiki.invalidJson"))?;
            self.offset += 1;
            value = value
                .checked_mul(16)
                .and_then(|value| value.checked_add(u16::from(hex(byte).ok()?)))
                .ok_or_else(|| mediawiki_malformed("mediawiki.invalidJson"))?;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<(), ConversionError> {
        let start = self.offset;
        self.take(b'-');
        match self.bytes.get(self.offset).copied() {
            Some(b'0') => {
                self.offset += 1;
                if self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                    return Err(mediawiki_malformed("mediawiki.invalidJson"));
                }
            }
            Some(b'1'..=b'9') => {
                self.offset += 1;
                while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                    self.offset += 1;
                    self.checkpoint()?;
                }
            }
            _ => return Err(mediawiki_malformed("mediawiki.invalidJson")),
        }
        if self.take(b'.') {
            if !self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                return Err(mediawiki_malformed("mediawiki.invalidJson"));
            }
            while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
                self.checkpoint()?;
            }
        }
        if self.bytes.get(self.offset).is_some_and(|byte| matches!(byte, b'e' | b'E')) {
            self.offset += 1;
            if self.bytes.get(self.offset).is_some_and(|byte| matches!(byte, b'+' | b'-')) {
                self.offset += 1;
            }
            if !self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                return Err(mediawiki_malformed("mediawiki.invalidJson"));
            }
            while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
                self.checkpoint()?;
            }
        }
        if self.offset.saturating_sub(start) > MAX_JSON_NUMBER_BYTES || !self.value_boundary() {
            return Err(mediawiki_malformed("mediawiki.invalidJson"));
        }
        Ok(())
    }

    fn literal(&mut self, value: &[u8]) -> Result<(), ConversionError> {
        if !self.bytes.get(self.offset..).is_some_and(|bytes| bytes.starts_with(value)) {
            return Err(mediawiki_malformed("mediawiki.invalidJson"));
        }
        self.offset += value.len();
        if self.value_boundary() {
            Ok(())
        } else {
            Err(mediawiki_malformed("mediawiki.invalidJson"))
        }
    }

    fn value_boundary(&self) -> bool {
        self.bytes
            .get(self.offset)
            .is_none_or(|byte| byte.is_ascii_whitespace() || matches!(byte, b',' | b'}' | b']'))
    }
}

fn collection_limit() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "mediawiki_json_collection_items",
        detail: "MediaWiki JSON exceeded its collection budget".into(),
    }
}

fn string_limit() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_field_bytes",
        detail: "MediaWiki JSON string exceeded its field budget".into(),
    }
}

fn valid_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if !(20..=35).contains(&bytes.len())
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || bytes.last() != Some(&b'Z')
    {
        return false;
    }
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        decimal(bytes, 0, 4),
        decimal(bytes, 5, 2),
        decimal(bytes, 8, 2),
        decimal(bytes, 11, 2),
        decimal(bytes, 14, 2),
        decimal(bytes, 17, 2),
    ) else {
        return false;
    };
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    let fractional = bytes.len() == 20
        || (bytes.len() > 21
            && bytes.get(19) == Some(&b'.')
            && bytes[20..bytes.len() - 1].iter().all(u8::is_ascii_digit));
    year >= 1970
        && (1..=days).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 59
        && fractional
}

fn decimal(bytes: &[u8], start: usize, length: usize) -> Option<u32> {
    bytes.get(start..start.checked_add(length)?)?.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit().then(|| value * 10 + u32::from(byte - b'0'))
    })
}

fn normalize_api_title(value: &str) -> Result<String, ConversionError> {
    if invalid_title(value) {
        return Err(mediawiki_malformed("mediawiki.invalidEnvelope"));
    }
    let normalized = value.nfc().collect::<String>();
    if normalized.trim() != normalized || invalid_title(&normalized) {
        return Err(mediawiki_malformed("mediawiki.invalidEnvelope"));
    }
    Ok(normalized)
}

fn invalid_title(value: &str) -> bool {
    value.is_empty() || value.len() > MAX_TITLE_BYTES || value.chars().any(char::is_control)
}

fn looks_like_mediawiki_article(value: &str) -> bool {
    let (rest, explicit) = if let Some(rest) = value.strip_prefix("mediawiki+https://") {
        (rest, true)
    } else if let Some(rest) = value.strip_prefix("mediawiki+http://") {
        (rest, true)
    } else if let Some(rest) = value.strip_prefix("https://") {
        (rest, false)
    } else if let Some(rest) = value.strip_prefix("http://") {
        (rest, false)
    } else {
        return false;
    };
    let path_start = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..path_start];
    if authority.is_empty() || authority.contains('@') || authority.starts_with('[') {
        return false;
    }
    let host = authority.split(':').next().unwrap_or_default();
    if !explicit && !is_wikipedia_host(host) {
        return false;
    }
    let suffix = &rest[path_start..];
    let path_end = suffix.find(['?', '#']).unwrap_or(suffix.len());
    suffix[..path_end].strip_prefix("/wiki/").is_some_and(|title| !title.is_empty())
}

fn is_wikipedia_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("wikipedia.org")
        || host.get(..host.len().saturating_sub(".wikipedia.org".len())).is_some_and(|prefix| {
            !prefix.is_empty() && host[prefix.len()..].eq_ignore_ascii_case(".wikipedia.org")
        })
}

fn mediawiki_urls(
    source: &str,
    context: &ExecutionContext,
) -> Result<(Url, String), ConversionError> {
    context.checkpoint()?;
    let (transport_source, explicit) = if let Some(rest) = source.strip_prefix("mediawiki+https://")
    {
        (format!("https://{rest}"), true)
    } else if let Some(rest) = source.strip_prefix("mediawiki+http://") {
        (format!("http://{rest}"), true)
    } else {
        (source.to_owned(), false)
    };
    let page_url = Url::parse(&transport_source)
        .map_err(|_| mediawiki_malformed("mediawiki.invalidArticleUrl"))?;
    if !matches!(page_url.scheme(), "http" | "https")
        || page_url.host_str().is_none()
        || !page_url.username().is_empty()
        || page_url.password().is_some()
        || (!explicit && !page_url.host_str().is_some_and(is_wikipedia_host))
    {
        return Err(mediawiki_malformed("mediawiki.invalidArticleUrl"));
    }
    let path = page_url.path().to_owned();
    let encoded_title = path
        .strip_prefix("/wiki/")
        .filter(|title| !title.is_empty())
        .ok_or_else(|| mediawiki_malformed("mediawiki.invalidArticleUrl"))?;
    let title = decode_title(encoded_title, context)?;

    let mut api_url = page_url;
    api_url.set_path("/w/api.php");
    api_url.set_query(None);
    api_url.set_fragment(None);
    {
        let mut query = api_url.query_pairs_mut();
        query.append_pair("action", "parse");
        query.append_pair("page", &title);
        query.append_pair("prop", "text|sections|links|images|displaytitle|revid");
        query.append_pair("redirects", "1");
        query.append_pair("format", "json");
        query.append_pair("formatversion", "2");
        query.append_pair("curtimestamp", "1");
        query.append_pair("requestid", &title);
    }
    Ok((api_url, title))
}

fn valid_api_endpoint(value: &str) -> Result<Url, ConversionError> {
    let url = Url::parse(value).map_err(|_| mediawiki_malformed("mediawiki.invalidApiEndpoint"))?;
    if value.len() > MAX_TRANSPORT_URL_BYTES
        || !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/w/api.php"
        || url.query().is_some()
        || url.fragment().is_some()
        || url.as_str() != value
    {
        return Err(mediawiki_malformed("mediawiki.invalidApiEndpoint"));
    }
    Ok(url)
}

fn looks_like_api_endpoint(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("https://").or_else(|| value.strip_prefix("http://"))
    else {
        return false;
    };
    let Some(path_start) = rest.find('/') else { return false };
    let authority = &rest[..path_start];
    !authority.is_empty()
        && !authority.contains('@')
        && !authority.starts_with('[')
        && &rest[path_start..] == "/w/api.php"
}

fn validate_fetch_identity(
    requested: &Url,
    final_url: &str,
    media_type: Option<&str>,
    redirects: &[into_markdown_http_transport::RedirectHop],
) -> Result<(), ConversionError> {
    if media_type != Some(JSON_MEDIA_TYPE) {
        return Err(mediawiki_malformed("mediawiki.invalidMediaType"));
    }
    let mut expected = requested.clone();
    expected.set_query(None);
    expected.set_fragment(None);
    let final_endpoint = valid_api_endpoint(final_url)
        .map_err(|_| mediawiki_malformed("mediawiki.apiRedirectRejected"))?;
    if !same_origin(&expected, &final_endpoint) || expected.path() != final_endpoint.path() {
        return Err(mediawiki_malformed("mediawiki.apiRedirectRejected"));
    }
    for redirect in redirects {
        let from = valid_api_endpoint(&redirect.from)
            .map_err(|_| mediawiki_malformed("mediawiki.apiRedirectRejected"))?;
        let to = valid_api_endpoint(&redirect.to)
            .map_err(|_| mediawiki_malformed("mediawiki.apiRedirectRejected"))?;
        if !same_origin(&expected, &from) || !same_origin(&expected, &to) {
            return Err(mediawiki_malformed("mediawiki.apiRedirectRejected"));
        }
    }
    Ok(())
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn source_url_from_api(api_url: &str, title: &str) -> Result<String, ConversionError> {
    let mut source = valid_api_endpoint(api_url)?;
    source.set_path("/");
    {
        let mut path = source
            .path_segments_mut()
            .map_err(|()| mediawiki_malformed("mediawiki.invalidApiEndpoint"))?;
        path.clear();
        path.push("wiki");
        path.push(title);
    }
    let value: String = source.into();
    if value.len() > MAX_TRANSPORT_URL_BYTES {
        return Err(mediawiki_malformed("mediawiki.invalidProvenance"));
    }
    Ok(value)
}

fn decode_title(encoded: &str, context: &ExecutionContext) -> Result<String, ConversionError> {
    if encoded.is_empty() || encoded.len() > MAX_TITLE_BYTES.saturating_mul(3) {
        return Err(title_limit());
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(encoded.len()).map_err(|_| title_limit())?;
    let source = encoded.as_bytes();
    let mut index = 0;
    while index < source.len() {
        if index.is_multiple_of(256) {
            context.checkpoint()?;
        }
        if source[index] == b'%' {
            let high = *source.get(index + 1).ok_or_else(invalid_title_encoding)?;
            let low = *source.get(index + 2).ok_or_else(invalid_title_encoding)?;
            bytes.push(hex(high)? * 16 + hex(low)?);
            index += 3;
        } else {
            bytes.push(source[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(bytes).map_err(|_| invalid_title_encoding())?;
    let normalized = decoded.replace('_', " ").nfc().collect::<String>();
    let normalized = normalized.trim();
    if invalid_title(normalized) {
        return Err(title_limit());
    }
    Ok(normalized.into())
}

fn hex(byte: u8) -> Result<u8, ConversionError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(invalid_title_encoding()),
    }
}

fn invalid_title_encoding() -> ConversionError {
    mediawiki_malformed("mediawiki.invalidTitleEncoding")
}

fn title_limit() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_mediawiki_title_bytes",
        detail: "MediaWiki title exceeded its canonicalization budget".into(),
    }
}

fn mediawiki_malformed(detail: &'static str) -> ConversionError {
    ConversionError::Malformed { part: Some("mediawiki".into()), detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        CancellationToken, ErrorCode, ExecutionOptions, FormatCandidate, FormatDetector,
        FormatHint, NetworkOptions, ResourceLimits,
    };
    use into_markdown_http_transport::{Connection, ConnectionFactory, DnsResolver};
    use std::collections::VecDeque;
    use std::io::{self, Read, Write};
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    struct CountingDns(AtomicUsize);

    impl DnsResolver for CountingDns {
        fn resolve(&self, _: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![SocketAddr::from(([8, 8, 8, 8], port))])
        }
    }

    struct PrivateDns(AtomicUsize);

    impl DnsResolver for PrivateDns {
        fn resolve(&self, _: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
        }
    }

    struct ScriptedConnection {
        response: io::Cursor<Vec<u8>>,
        request: Arc<Mutex<Vec<u8>>>,
    }

    impl Read for ScriptedConnection {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.response.read(output)
        }
    }

    impl Write for ScriptedConnection {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            self.request.lock().unwrap().extend_from_slice(input);
            Ok(input.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct ScriptedConnector {
        response: Vec<u8>,
        request: Arc<Mutex<Vec<u8>>>,
    }

    struct QueueConnector {
        responses: Mutex<VecDeque<Vec<u8>>>,
        requests: AtomicUsize,
    }

    impl ConnectionFactory for QueueConnector {
        fn connect(
            &self,
            _: &str,
            _: &str,
            _: SocketAddr,
            _: &ExecutionContext,
            _: Instant,
        ) -> Result<Box<dyn Connection>, into_markdown_http_transport::TransportError> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response queue exhausted");
            Ok(Box::new(ScriptedConnection {
                response: io::Cursor::new(response),
                request: Arc::new(Mutex::new(Vec::new())),
            }))
        }
    }

    fn response(content_type: Option<&str>, body: &[u8]) -> Vec<u8> {
        let content_type =
            content_type.map(|value| format!("Content-Type: {value}\r\n")).unwrap_or_default();
        let mut response =
            format!("HTTP/1.1 200 OK\r\n{content_type}Content-Length: {}\r\n\r\n", body.len())
                .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn api_input(bytes: impl Into<Arc<[u8]>>) -> ResolvedInput {
        ResolvedInput {
            bytes: bytes.into(),
            metadata: SourceMetadata {
                media_type: Some(AUTHENTICATED_MEDIA_TYPE.into()),
                uri: Some("https://en.wikipedia.org/w/api.php".into()),
                ..SourceMetadata::default()
            },
        }
    }

    fn verify_mediawiki_blocks(nodes: &[BlockNode]) -> usize {
        let mut count = 0;
        for node in nodes {
            count += 1;
            assert_eq!(node.provenance.provider, PROVIDER_ID);
            assert_eq!(node.provenance.locator, SourceLocator::default());
            count += match &node.block {
                Block::List { items, .. } => {
                    items.iter().map(|item| verify_mediawiki_blocks(&item.blocks)).sum()
                }
                Block::Table { rows, .. } => rows
                    .iter()
                    .flat_map(|row| &row.cells)
                    .map(|cell| verify_mediawiki_blocks(&cell.blocks))
                    .sum(),
                Block::Footnote { blocks, .. }
                | Block::Page { blocks, .. }
                | Block::Slide { blocks, .. }
                | Block::Sheet { blocks, .. } => verify_mediawiki_blocks(blocks),
                _ => 0,
            };
        }
        count
    }

    impl ConnectionFactory for ScriptedConnector {
        fn connect(
            &self,
            _: &str,
            _: &str,
            _: SocketAddr,
            _: &ExecutionContext,
            _: Instant,
        ) -> Result<Box<dyn Connection>, into_markdown_http_transport::TransportError> {
            Ok(Box::new(ScriptedConnection {
                response: io::Cursor::new(self.response.clone()),
                request: Arc::clone(&self.request),
            }))
        }
    }

    fn context(memory: u64) -> ExecutionContext {
        ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: memory, ..ResourceLimits::default() },
        )
    }

    fn enabled_options() -> ConversionOptions {
        ConversionOptions {
            network: NetworkOptions {
                enabled: true,
                allowed_hosts: vec!["en.wikipedia.org".into()],
                ..NetworkOptions::default()
            },
            ..ConversionOptions::default()
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(output) => return output,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn offline_rejection_occurs_before_dns_or_connect() {
        let dns = Arc::new(CountingDns(AtomicUsize::new(0)));
        let resolver =
            MediaWikiSourceResolver::with_client(Arc::new(HttpClient::with_resolver(dns.clone())));
        let error = block_on(resolver.resolve(
            &InputRef::Uri("https://en.wikipedia.org/wiki/Rust".into()),
            &ConversionOptions::default(),
            &context(1_000_000),
        ))
        .err()
        .unwrap();
        assert_eq!(error.code(), ErrorCode::Network);
        assert_eq!(dns.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn resolver_claims_only_unambiguous_article_uris() {
        let resolver = MediaWikiSourceResolver::default();
        assert!(resolver.supports(&InputRef::Uri("https://en.wikipedia.org/wiki/Rust".into())));
        assert!(
            resolver
                .supports(&InputRef::Uri("mediawiki+https://wiki.example.test/wiki/Rust".into()))
        );
        for ordinary in [
            "https://example.test/assets/wiki/manual.html",
            "https://example.test/wiki/help.json",
            "https://en.wikipedia.org/assets/wiki/manual.html",
            "https://en.wikipedia.org/docs/wiki/Rust",
            "mediawiki+https://wiki.example.test/docs/wiki/Rust",
        ] {
            assert!(!resolver.supports(&InputRef::Uri(ordinary.into())), "claimed {ordinary}");
        }
    }

    #[test]
    fn host_private_network_and_cancellation_policy_are_inherited() {
        let denied_dns = Arc::new(CountingDns(AtomicUsize::new(0)));
        let denied = MediaWikiSourceResolver::with_client(Arc::new(HttpClient::with_resolver(
            denied_dns.clone(),
        )));
        let mut denied_options = enabled_options();
        denied_options.network.allowed_hosts = vec!["other.example".into()];
        let error = block_on(denied.resolve(
            &InputRef::Uri("https://en.wikipedia.org/wiki/Rust".into()),
            &denied_options,
            &context(1_000_000),
        ))
        .err()
        .unwrap();
        assert_eq!(error.code(), ErrorCode::Network);
        assert_eq!(denied_dns.0.load(Ordering::SeqCst), 0);

        let private_dns = Arc::new(PrivateDns(AtomicUsize::new(0)));
        let private = MediaWikiSourceResolver::with_client(Arc::new(HttpClient::with_resolver(
            private_dns.clone(),
        )));
        let error = block_on(private.resolve(
            &InputRef::Uri("https://en.wikipedia.org/wiki/Rust".into()),
            &enabled_options(),
            &context(1_000_000),
        ))
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Network);
        assert_eq!(private_dns.0.load(Ordering::SeqCst), 1);

        let cancelled_dns = Arc::new(CountingDns(AtomicUsize::new(0)));
        let cancelled = MediaWikiSourceResolver::with_client(Arc::new(HttpClient::with_resolver(
            cancelled_dns.clone(),
        )));
        let token = CancellationToken::new();
        token.cancel();
        let cancelled_context = ExecutionContext::new(
            ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let error = block_on(cancelled.resolve(
            &InputRef::Uri("https://en.wikipedia.org/wiki/Rust".into()),
            &enabled_options(),
            &cancelled_context,
        ))
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert_eq!(cancelled_dns.0.load(Ordering::SeqCst), 0);
        assert_eq!(cancelled_context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn controlled_transport_fetches_the_bounded_api_endpoint() {
        let body = include_bytes!("../../tests/fixtures/mediawiki/complete.json");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        let mut response = response;
        response.extend_from_slice(body);
        let request = Arc::new(Mutex::new(Vec::new()));
        let client = HttpClient::with_components(
            Arc::new(CountingDns(AtomicUsize::new(0))),
            Arc::new(ScriptedConnector { response, request: Arc::clone(&request) }),
        );
        let resolved = block_on(MediaWikiSourceResolver::with_client(Arc::new(client)).resolve(
            &InputRef::Uri(
                "https://en.wikipedia.org/wiki/Rust_(programming_language)?oldid=1#x".into(),
            ),
            &enabled_options(),
            &context(8 * 1024 * 1024),
        ))
        .unwrap();
        assert_eq!(resolved.metadata.media_type.as_deref(), Some(AUTHENTICATED_MEDIA_TYPE));
        assert_eq!(resolved.metadata.uri.as_deref(), Some("https://en.wikipedia.org/w/api.php"));
        let request = String::from_utf8(request.lock().unwrap().clone()).unwrap();
        assert!(request.starts_with("GET /w/api.php?"));
        assert!(request.contains("page=Rust+%28programming+language%29"));
        assert!(request.contains("requestid=Rust+%28programming+language%29"));
        assert!(!request.contains("oldid"));
    }

    #[test]
    fn response_identity_and_json_media_type_are_fail_closed() {
        let body = include_bytes!("../../tests/fixtures/mediawiki/complete.json");
        for media_type in [Some("text/plain"), Some("text/html"), None] {
            let connector = Arc::new(QueueConnector {
                responses: Mutex::new(VecDeque::from([response(media_type, body)])),
                requests: AtomicUsize::new(0),
            });
            let client =
                HttpClient::with_components(Arc::new(CountingDns(AtomicUsize::new(0))), connector);
            let error = block_on(MediaWikiSourceResolver::with_client(Arc::new(client)).resolve(
                &InputRef::Uri("https://en.wikipedia.org/wiki/Rust".into()),
                &enabled_options(),
                &context(8 * 1024 * 1024),
            ))
            .unwrap_err();
            assert!(error.to_string().contains("mediawiki.invalidMediaType"));
        }

        for location in
            ["https://en.wikipedia.org/other/api.php", "https://other.example/w/api.php"]
        {
            let redirect =
                format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
                    .into_bytes();
            let connector = Arc::new(QueueConnector {
                responses: Mutex::new(VecDeque::from([
                    redirect,
                    response(Some("application/json; charset=utf-8"), body),
                ])),
                requests: AtomicUsize::new(0),
            });
            let client =
                HttpClient::with_components(Arc::new(CountingDns(AtomicUsize::new(0))), connector);
            let mut options = enabled_options();
            options.network.allowed_hosts.push("other.example".into());
            let error = block_on(MediaWikiSourceResolver::with_client(Arc::new(client)).resolve(
                &InputRef::Uri("https://en.wikipedia.org/wiki/Rust".into()),
                &options,
                &context(8 * 1024 * 1024),
            ))
            .unwrap_err();
            assert!(error.to_string().contains("mediawiki.apiRedirectRejected"));
        }
    }

    #[test]
    fn api_response_limit_is_enforced_before_body_acceptance() {
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 5\r\n\r\n12345"
                .to_vec();
        let request = Arc::new(Mutex::new(Vec::new()));
        let client = HttpClient::with_components(
            Arc::new(CountingDns(AtomicUsize::new(0))),
            Arc::new(ScriptedConnector { response, request }),
        );
        let mut options = enabled_options();
        options.limits.max_input_bytes = 4;
        let error = block_on(MediaWikiSourceResolver::with_client(Arc::new(client)).resolve(
            &InputRef::Uri("https://en.wikipedia.org/wiki/Rust".into()),
            &options,
            &context(1_000_000),
        ))
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
    }

    #[test]
    fn resolver_retained_lease_precedes_closure_and_survives_handoff() {
        let options = enabled_options();
        let source = "https://en.wikipedia.org/wiki/Rust";
        let plan = resolver_retained_memory(source, &options).unwrap();
        let dns = Arc::new(CountingDns(AtomicUsize::new(0)));
        let low = context(plan - 1);
        let error = block_on(
            MediaWikiSourceResolver::with_client(Arc::new(HttpClient::with_resolver(dns.clone())))
                .resolve_accounted(&InputRef::Uri(source.into()), &options, &low),
        )
        .err()
        .unwrap();
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        assert_eq!(dns.0.load(Ordering::SeqCst), 0);
        assert_eq!(low.reserved_memory_bytes(), 0);

        let body = include_bytes!("../../tests/fixtures/mediawiki/complete.json");
        let connector = Arc::new(QueueConnector {
            responses: Mutex::new(VecDeque::from([response(Some(JSON_MEDIA_TYPE), body)])),
            requests: AtomicUsize::new(0),
        });
        let client =
            HttpClient::with_components(Arc::new(CountingDns(AtomicUsize::new(0))), connector);
        let exact = context(plan + u64::try_from(body.len()).unwrap() + 128 * 1024);
        let resolved =
            block_on(MediaWikiSourceResolver::with_client(Arc::new(client)).resolve_accounted(
                &InputRef::Uri(source.into()),
                &options,
                &exact,
            ))
            .unwrap();
        assert!(exact.reserved_memory_bytes() >= plan + u64::try_from(body.len()).unwrap());
        drop(resolved);
        assert_eq!(exact.reserved_memory_bytes(), 0);

        let connector = Arc::new(QueueConnector {
            responses: Mutex::new(VecDeque::from([response(Some("text/plain"), body)])),
            requests: AtomicUsize::new(0),
        });
        let client =
            HttpClient::with_components(Arc::new(CountingDns(AtomicUsize::new(0))), connector);
        let failed = context(8 * 1024 * 1024);
        let _ = block_on(MediaWikiSourceResolver::with_client(Arc::new(client)).resolve_accounted(
            &InputRef::Uri(source.into()),
            &options,
            &failed,
        ))
        .err()
        .unwrap();
        assert_eq!(failed.reserved_memory_bytes(), 0);
    }

    #[test]
    fn complete_fixture_yields_semantic_ir_and_provenance_metadata() {
        let input = ResolvedInput {
            bytes: Arc::from(
                include_bytes!("../../tests/fixtures/mediawiki/complete.json").as_slice(),
            ),
            metadata: SourceMetadata {
                name: Some("mediawiki.json".into()),
                media_type: Some(AUTHENTICATED_MEDIA_TYPE.into()),
                uri: Some("https://en.wikipedia.org/w/api.php".into()),
                size: 0,
            },
        };
        let output = block_on(MediaWikiConverter.convert(
            &input,
            &FormatCandidate::new(InputFormat::Wikipedia, 1.0, "test"),
            &ConversionOptions::default(),
            &Services::default(),
            &context(64 * 1024 * 1024),
        ))
        .unwrap();
        assert_eq!(output.document.metadata.title.as_deref(), Some("Rust (programming language)"));
        assert_eq!(
            output.document.metadata.properties.get("mediawiki.revisionId").map(String::as_str),
            Some("123456")
        );
        assert_eq!(
            output.document.metadata.properties.get("mediawiki.retrievedAt").map(String::as_str),
            Some("2026-08-13T00:00:00Z")
        );
        assert_eq!(
            output.document.metadata.properties.get("mediawiki.provider").map(String::as_str),
            Some(PROVIDER_ID)
        );
        assert_eq!(
            output.document.metadata.properties.get("mediawiki.sourceUrl").map(String::as_str),
            Some("https://en.wikipedia.org/wiki/Rust%20(programming%20language)")
        );
        assert!(output.document.blocks.iter().all(|block| {
            block.provenance.provider == PROVIDER_ID
                && block.provenance.locator == SourceLocator::default()
        }));
        assert!(output.document.blocks.len() >= 3);
        assert_eq!(output.assets.len(), 1);
        assert_eq!(
            output.assets[0].external_uri.as_deref(),
            Some("https://upload.wikimedia.org/rust.png")
        );
        assert!(output.diagnostics.iter().all(|item| item.code != "mediawiki.titleRedirected"));
    }

    #[test]
    fn nested_blocks_are_bound_to_the_single_document_source_record() {
        let bytes: Arc<[u8]> = Arc::from(
            br#"{"requestid":"Nested","curtimestamp":"2026-08-13T00:00:00Z","parse":{"title":"Nested","pageid":7,"revid":9,"text":"<main><ul><li><p>list</p></li></ul><table><tr><td>cell</td></tr></table></main>","sections":[],"links":[],"images":[]}}"#
                .as_slice(),
        );
        let output = convert_mediawiki(
            &api_input(bytes),
            &ConversionOptions::default(),
            &context(64 * 1024 * 1024),
        )
        .unwrap();

        assert!(verify_mediawiki_blocks(&output.document.blocks) >= 4);
        assert_eq!(
            output.document.metadata.properties.get("mediawiki.sourceUrl").map(String::as_str),
            Some("https://en.wikipedia.org/wiki/Nested")
        );
    }

    #[test]
    fn only_resolver_authenticated_api_candidate_outranks_the_json_wire_shape() {
        let authenticated = ResolvedInput {
            bytes: Arc::from(
                include_bytes!("../../tests/fixtures/mediawiki/complete.json").as_slice(),
            ),
            metadata: SourceMetadata {
                media_type: Some(AUTHENTICATED_MEDIA_TYPE.into()),
                uri: Some("https://en.wikipedia.org/w/api.php".into()),
                ..SourceMetadata::default()
            },
        };
        let context = context(1_000_000);
        let mediawiki = block_on(MediaWikiFormatDetector.detect(
            &authenticated,
            &FormatHint::default(),
            &context,
        ))
        .unwrap();
        let wire_candidates = block_on(crate::ContentFormatDetector.detect(
            &authenticated,
            &FormatHint::default(),
            &context,
        ))
        .unwrap();
        assert_eq!(mediawiki[0].format, InputFormat::Wikipedia);
        assert!(mediawiki[0].confidence > wire_candidates[0].confidence);

        let ordinary = ResolvedInput {
            bytes: Arc::clone(&authenticated.bytes),
            metadata: SourceMetadata {
                media_type: Some(JSON_MEDIA_TYPE.into()),
                uri: Some("https://en.wikipedia.org/w/api.php".into()),
                ..SourceMetadata::default()
            },
        };
        let forged_hint = FormatHint {
            media_type: Some(AUTHENTICATED_MEDIA_TYPE.into()),
            ..FormatHint::default()
        };
        assert!(
            block_on(MediaWikiFormatDetector.detect(&ordinary, &forged_hint, &context))
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            block_on(MediaWikiConverter.probe(
                &ordinary,
                &FormatCandidate::new(InputFormat::Wikipedia, 1.0, "forged hint"),
                &context,
            ))
            .unwrap(),
            ProbeOutcome::NotApplicable
        ));
        let ordinary_candidates = block_on(crate::ContentFormatDetector.detect(
            &ordinary,
            &FormatHint::default(),
            &context,
        ))
        .unwrap();
        assert_eq!(ordinary_candidates[0].format, InputFormat::Json);
    }

    #[test]
    fn redirect_and_missing_page_have_stable_diagnostics() {
        let redirect = ResolvedInput {
            bytes: Arc::from(
                include_bytes!("../../tests/fixtures/mediawiki/redirect.json").as_slice(),
            ),
            metadata: SourceMetadata {
                media_type: Some(AUTHENTICATED_MEDIA_TYPE.into()),
                uri: Some("https://en.wikipedia.org/w/api.php".into()),
                ..SourceMetadata::default()
            },
        };
        let output =
            convert_mediawiki(&redirect, &ConversionOptions::default(), &context(64 * 1024 * 1024))
                .unwrap();
        assert!(output.diagnostics.iter().any(|item| item.code == "mediawiki.titleRedirected"));

        let missing = ResolvedInput {
            bytes: Arc::from(
                include_bytes!("../../tests/fixtures/mediawiki/missing.json").as_slice(),
            ),
            metadata: SourceMetadata {
                media_type: Some(AUTHENTICATED_MEDIA_TYPE.into()),
                uri: Some("https://en.wikipedia.org/w/api.php".into()),
                ..SourceMetadata::default()
            },
        };
        let error =
            convert_mediawiki(&missing, &ConversionOptions::default(), &context(64 * 1024 * 1024))
                .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Malformed);
        assert!(error.to_string().contains("mediawiki.pageMissing"));
    }

    #[test]
    fn malformed_title_and_low_budgets_fail_closed() {
        assert!(
            !MediaWikiSourceResolver::default()
                .supports(&InputRef::Uri("https://example.test/?next=/wiki/not-an-article".into()))
        );
        assert_eq!(
            mediawiki_urls("https://en.wikipedia.org/wiki/%GG", &context(1_000_000))
                .unwrap_err()
                .code(),
            ErrorCode::Malformed
        );
        let resolver = MediaWikiSourceResolver::default();
        let error = block_on(resolver.resolve(
            &InputRef::Uri("https://en.wikipedia.org/wiki/Rust".into()),
            &enabled_options(),
            &context(1),
        ))
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceLimit);

        let bytes = include_bytes!("../../tests/fixtures/mediawiki/missing.json");
        let parser_peak =
            u64::try_from(bytes.len()).unwrap() * API_JSON_MEMORY_FACTOR + API_JSON_MEMORY_BASE;
        let input = ResolvedInput {
            bytes: Arc::from(bytes.as_slice()),
            metadata: SourceMetadata {
                media_type: Some(AUTHENTICATED_MEDIA_TYPE.into()),
                uri: Some("https://en.wikipedia.org/w/api.php".into()),
                ..SourceMetadata::default()
            },
        };
        let error =
            convert_mediawiki(&input, &ConversionOptions::default(), &context(parser_peak - 1))
                .unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        let shape_peak = u64::try_from(bytes.len() * 6 + 16 * 1024).unwrap();
        let exact = context(parser_peak + shape_peak);
        let error = convert_mediawiki(&input, &ConversionOptions::default(), &exact).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Malformed);
        assert!(error.to_string().contains("mediawiki.pageMissing"));
        assert_eq!(exact.reserved_memory_bytes(), 0);
        assert!(!valid_timestamp("2026-02-29T00:00:00Z"));
        assert!(!valid_timestamp("2026-08-13T00:00:00.Z"));
    }

    #[test]
    fn json_shape_limits_duplicates_and_mid_parse_cancellation_are_enforced() {
        let mut options = ConversionOptions::default();
        options.limits.max_field_bytes = 4;
        preflight_mediawiki_json(br#"{"a":"1234"}"#, &options, &context(1_000_000)).unwrap();
        options.limits.max_field_bytes = 3;
        assert_eq!(
            preflight_mediawiki_json(br#"{"a":"1234"}"#, &options, &context(1_000_000))
                .unwrap_err()
                .code(),
            ErrorCode::ResourceLimit
        );

        let mut options = ConversionOptions::default();
        options.limits.max_nesting_depth = 2;
        preflight_mediawiki_json(br#"{"a":[0]}"#, &options, &context(1_000_000)).unwrap();
        options.limits.max_nesting_depth = 1;
        assert_eq!(
            preflight_mediawiki_json(br#"{"a":[0]}"#, &options, &context(1_000_000))
                .unwrap_err()
                .code(),
            ErrorCode::ResourceLimit
        );

        let mut options = ConversionOptions::default();
        options.limits.max_table_cells = 3;
        preflight_mediawiki_json(b"[0,1]", &options, &context(1_000_000)).unwrap();
        options.limits.max_table_cells = 2;
        assert_eq!(
            preflight_mediawiki_json(b"[0,1]", &options, &context(1_000_000)).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );

        for duplicate in [
            br#"{"parse":{},"parse":{}}"#.as_slice(),
            br#"{"unknown":{"x":1,"x":2}}"#.as_slice(),
            br#"{"unknown":{"x":1,"\u0078":2}}"#.as_slice(),
        ] {
            let error = preflight_mediawiki_json(
                duplicate,
                &ConversionOptions::default(),
                &context(1_000_000),
            )
            .unwrap_err();
            assert!(error.to_string().contains("mediawiki.duplicateObjectField"));
        }

        let token = CancellationToken::new();
        JSON_SCAN_CHECKPOINTS.set(0);
        JSON_SCAN_CANCEL_AT.with(|slot| *slot.borrow_mut() = Some((3, token.clone())));
        let cancelled_context = ExecutionContext::new(
            ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let long = format!("{{\"value\":\"{}\"}}", "x".repeat(4 * 1024));
        let error = preflight_mediawiki_json(
            long.as_bytes(),
            &ConversionOptions::default(),
            &cancelled_context,
        )
        .unwrap_err();
        JSON_SCAN_CANCEL_AT.with(|slot| slot.borrow_mut().take());
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert!(JSON_SCAN_CHECKPOINTS.get() >= 3);

        let token = CancellationToken::new();
        JSON_SCAN_CHECKPOINTS.set(0);
        JSON_SCAN_CANCEL_AT.with(|slot| *slot.borrow_mut() = Some((3, token.clone())));
        let cancelled_context = ExecutionContext::new(
            ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let short_token_flood = format!("[{}]", "0,".repeat(4 * 1024) + "0");
        let error = preflight_mediawiki_json(
            short_token_flood.as_bytes(),
            &ConversionOptions::default(),
            &cancelled_context,
        )
        .unwrap_err();
        JSON_SCAN_CANCEL_AT.with(|slot| slot.borrow_mut().take());
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert!(JSON_SCAN_CHECKPOINTS.get() >= 3);

        JSON_SCAN_CHECKPOINTS.set(0);
        let long = format!("{{\"value\":\"{}\"}}", "x".repeat(8 * 1024 * 1024));
        let deadline_context = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(std::time::Duration::from_millis(1)),
                ..ExecutionOptions::default()
            },
            ResourceLimits { max_memory_bytes: 128 * 1024 * 1024, ..ResourceLimits::default() },
        );
        let error = preflight_mediawiki_json(
            long.as_bytes(),
            &ConversionOptions::default(),
            &deadline_context,
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Timeout);
        assert!(JSON_SCAN_CHECKPOINTS.get() > 1);
    }
}

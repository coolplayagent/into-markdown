//! Offline HTML5 parsing and deterministic semantic extraction.

use base64::Engine as _;
use html5ever::interface::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, ParseOpts, QualName, parse_document};
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, BoxFuture, Cell, ConversionError, ConversionOptions,
    Converter, ConverterOutput, Diagnostic, DiagnosticSeverity, Document, DocumentMetadata,
    ExecutionContext, FormatCandidate, Inline, InlineMark, InputFormat, IrErrorCode, ListItem,
    ListKind, MAX_DOCUMENT_INLINES, MAX_DOCUMENT_NODES, MAX_TABLE_COLUMNS, NodeId, ProbeOutcome,
    Provenance, ProvenanceKind, ResolvedInput, Services, SourceLocator, TableAlignment, TableRow,
    canonical_external_asset_uri,
};
use std::borrow::Cow;
use std::cell::{Cell as MutCell, Ref, RefCell};
use std::fmt::Write as _;
use std::mem::size_of;
use url::Url;

use super::text::{DecodedText, LogicalMemory, decode_source};

const FORMATS: &[InputFormat] = &[InputFormat::Html];
const PROVIDER_ID: &str = "builtin.converter.html";
const MAX_HTML_EVENTS: usize = 1_000_000;
const META_PRESCAN_BYTES: usize = 1024;
const CHECKPOINT_EVENTS: usize = 1024;
const SOURCE_LOCATION_MESSAGE: &str = "HTML5 tree construction can synthesize or reparent nodes; ambiguous DOM nodes intentionally have no fabricated byte span";

// Feed fragments use a cooperative logical-memory bound because html5ever has
// no allocator hook. This model is tied to crates.io html5ever/markup5ever
// 0.39.0 from servo/html5ever commit ce64836c685025a5fef0860fa2e9c80b2683e8d0
// (Cargo checksums 46a176…/7122d9…) and tendril 0.5.1 from commit
// d64dfd4c21cf2451649107ade7eaf042d95fbc5a (checksum 5fed54…).
// Audited sources: html5ever tokenizer/mod.rs 815a67…, tree_builder/mod.rs
// e8f663…, markup5ever buffer_queue.rs 5d0bcd…, tendril buf32.rs 77947b….
// The model includes BufferQueue's 16 slots, every tokenizer tendril and
// attribute, four TreeBuilder vectors, a twofold Vec capacity factor, tendril's
// next-power-of-two (<2x) capacity, and all 8 adoption-agency outer rounds.
const HTML5EVER_MODEL_ID: &str = "html5ever@ce64836c+markup5ever@ce64836c+tendril@d64dfd4c;bufq=16;token-tendrils=9;tree-vecs=4;vec=2;tendril=2;adoption=8;mutation=64";
const PARSER_BASE_BYTES: usize = 64 * 1024;
const BUFFER_QUEUE_SLOTS: usize = 16;
const TOKENIZER_TENDRILS: usize = 9;
const TREE_BUILDER_VECTORS: usize = 4;
const VEC_GROWTH_FACTOR: usize = 2;
const TENDRIL_GROWTH_FACTOR: usize = 2;
const ADOPTION_AGENCY_ROUNDS: usize = 8;
const MUTATIONS_PER_TOKEN: usize = 64;
const PARSER_BYTES_PER_MUTATION: usize = 256;

#[derive(Clone, Copy, Debug)]
struct HtmlParserPreflight {
    bytes: usize,
    tags: usize,
    attributes: usize,
    entities: usize,
    text_runs: usize,
    max_tag_bytes: usize,
    max_attributes_per_tag: usize,
}

fn parser_limit(detail: &'static str) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}

fn checked_add(left: usize, right: usize, detail: &'static str) -> Result<usize, ConversionError> {
    left.checked_add(right).ok_or_else(|| parser_limit(detail))
}

fn checked_mul(left: usize, right: usize, detail: &'static str) -> Result<usize, ConversionError> {
    left.checked_mul(right).ok_or_else(|| parser_limit(detail))
}

fn validate_html_model_id(model: &str) -> Result<(), ConversionError> {
    if model == HTML5EVER_MODEL_ID {
        Ok(())
    } else {
        Err(ConversionError::Internal {
            detail: "feed HTML parser allocation model does not match the audited dependency"
                .into(),
        })
    }
}

/// Allocation-free, checkpointed scan run before an html5ever parser exists.
/// It deliberately treats every `<`, `&`, whitespace run, and `=` as possible
/// work, including malformed/unclosed tags and raw-text contents. Over-counting
/// is allowed; under-counting third-party parser work is not.
fn preflight_feed_html(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<HtmlParserPreflight, ConversionError> {
    validate_html_model_id(HTML5EVER_MODEL_ID)?;
    let mut result = HtmlParserPreflight {
        bytes: bytes.len(),
        tags: 0,
        attributes: 0,
        entities: 0,
        text_runs: 0,
        max_tag_bytes: 0,
        max_attributes_per_tag: 0,
    };
    let mut in_tag = false;
    let mut quote = 0_u8;
    let mut tag_start = 0_usize;
    let mut tag_attributes = 0_usize;
    let mut whitespace = false;
    let mut text = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if index.is_multiple_of(CHECKPOINT_EVENTS) {
            context.checkpoint()?;
        }
        if byte == b'&' {
            result.entities = checked_add(result.entities, 1, "HTML entity count overflowed")?;
        }
        if !in_tag {
            if byte == b'<' {
                result.tags = checked_add(result.tags, 1, "HTML tag count overflowed")?;
                in_tag = true;
                quote = 0;
                tag_start = index;
                tag_attributes = 0;
                whitespace = false;
                text = false;
            } else if !text {
                result.text_runs =
                    checked_add(result.text_runs, 1, "HTML text-run count overflowed")?;
                text = true;
            }
            continue;
        }
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
            continue;
        }
        if byte == b'<' {
            let length = index.saturating_sub(tag_start);
            result.max_tag_bytes = result.max_tag_bytes.max(length);
            result.attributes =
                checked_add(result.attributes, tag_attributes, "HTML attribute total overflowed")?;
            result.max_attributes_per_tag = result.max_attributes_per_tag.max(tag_attributes);
            result.tags = checked_add(result.tags, 1, "HTML tag count overflowed")?;
            tag_start = index;
            tag_attributes = 0;
            whitespace = false;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = byte;
            continue;
        }
        if byte.is_ascii_whitespace() {
            if !whitespace {
                tag_attributes = checked_add(tag_attributes, 1, "HTML attribute count overflowed")?;
            }
            whitespace = true;
            continue;
        }
        if byte == b'=' {
            tag_attributes = checked_add(tag_attributes, 1, "HTML attribute count overflowed")?;
        }
        whitespace = false;
        if byte == b'>' {
            let length = index.saturating_add(1).saturating_sub(tag_start);
            result.max_tag_bytes = result.max_tag_bytes.max(length);
            result.attributes =
                checked_add(result.attributes, tag_attributes, "HTML attribute total overflowed")?;
            result.max_attributes_per_tag = result.max_attributes_per_tag.max(tag_attributes);
            in_tag = false;
        }
    }
    if in_tag {
        let length = bytes.len().saturating_sub(tag_start);
        result.max_tag_bytes = result.max_tag_bytes.max(length);
        result.attributes =
            checked_add(result.attributes, tag_attributes, "HTML attribute total overflowed")?;
        result.max_attributes_per_tag = result.max_attributes_per_tag.max(tag_attributes);
    }
    Ok(result)
}

impl HtmlParserPreflight {
    /// Checked upper bound for parser workspace plus the complete retained DOM.
    /// Vec and tendril factors are applied once here; DOM's prepaid meter checks
    /// real capacities without charging the feed lease a second time.
    fn memory_bound(self) -> Result<usize, ConversionError> {
        let tokens = checked_add(
            checked_add(self.tags, self.entities, "HTML token count overflowed")?,
            checked_add(self.text_runs, 8, "HTML token count overflowed")?,
            "HTML token count overflowed",
        )?;
        let mutations = checked_mul(tokens, MUTATIONS_PER_TOKEN, "HTML mutation bound overflowed")?;
        let adoption = checked_mul(
            checked_mul(self.tags, ADOPTION_AGENCY_ROUNDS, "HTML adoption bound overflowed")?,
            checked_add(
                self.max_tag_bytes,
                checked_mul(
                    self.max_attributes_per_tag,
                    size_of::<Attribute>(),
                    "HTML adoption attribute bound overflowed",
                )?,
                "HTML adoption payload bound overflowed",
            )?,
            "HTML adoption payload bound overflowed",
        )?;
        let input_tendril_factor =
            checked_mul(TENDRIL_GROWTH_FACTOR, 4, "HTML tendril factor overflowed")?;
        let input_tendrils =
            checked_mul(self.bytes, input_tendril_factor, "HTML tendril bound overflowed")?;
        let tokenizer_tendrils = checked_mul(
            checked_mul(
                self.max_tag_bytes,
                TOKENIZER_TENDRILS,
                "HTML tokenizer tendril bound overflowed",
            )?,
            TENDRIL_GROWTH_FACTOR,
            "HTML tokenizer tendril bound overflowed",
        )?;
        let attribute_payload = checked_mul(
            checked_add(
                self.bytes,
                checked_mul(self.attributes, size_of::<Attribute>(), "HTML attrs overflowed")?,
                "HTML attrs overflowed",
            )?,
            TENDRIL_GROWTH_FACTOR,
            "HTML attrs overflowed",
        )?;
        let mutation_unit = checked_mul(
            PARSER_BYTES_PER_MUTATION,
            VEC_GROWTH_FACTOR,
            "HTML mutation unit overflowed",
        )?;
        let mutation_payload =
            checked_mul(mutations, mutation_unit, "HTML mutation bytes overflowed")?;
        let tree_vectors = checked_mul(
            checked_mul(tokens, TREE_BUILDER_VECTORS, "HTML TreeBuilder vector bound overflowed")?,
            checked_mul(
                size_of::<usize>(),
                VEC_GROWTH_FACTOR,
                "HTML TreeBuilder vector bound overflowed",
            )?,
            "HTML TreeBuilder vector bound overflowed",
        )?;
        let queue = checked_mul(
            BUFFER_QUEUE_SLOTS,
            size_of::<StrTendril>(),
            "HTML buffer queue overflowed",
        )?;
        [
            input_tendrils,
            tokenizer_tendrils,
            attribute_payload,
            mutation_payload,
            tree_vectors,
            adoption,
            queue,
        ]
        .into_iter()
        .try_fold(PARSER_BASE_BYTES, |sum, value| {
            checked_add(sum, value, "HTML parser preflight memory overflowed")
        })
    }
}

#[cfg(test)]
pub(crate) fn feed_html_parser_memory_bound(
    fragment: &str,
    context: &ExecutionContext,
) -> Result<usize, ConversionError> {
    preflight_feed_html(fragment.as_bytes(), context)?.memory_bound()
}

/// Persistent-output budget shared by a feed and every nested HTML fragment.
///
/// The reservation is owned by the feed until its final `Document` is complete.
/// Nested HTML must reserve each material object and string before constructing
/// it; callers only verify the returned counts and never perform the first
/// limit check after allocation.
pub(crate) struct FeedHtmlBudget {
    pub(crate) memory: LogicalMemory,
    pub(crate) nodes: usize,
    pub(crate) inlines: usize,
    pub(crate) assets: usize,
    pub(crate) diagnostics: usize,
    pub(crate) strings: usize,
    pub(crate) output_bytes: u64,
    persistent_memory_bytes: usize,
    max_nodes: usize,
    max_inlines: usize,
    max_assets: usize,
    max_diagnostics: usize,
    max_strings: usize,
    max_output_bytes: u64,
    max_persistent_memory_bytes: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct FeedHtmlBudgetSnapshot {
    pub(crate) nodes: usize,
    pub(crate) inlines: usize,
    pub(crate) assets: usize,
    pub(crate) diagnostics: usize,
    pub(crate) strings: usize,
    pub(crate) output_bytes: u64,
    pub(crate) persistent_memory_bytes: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct FeedHtmlTransactionSnapshot {
    counters: FeedHtmlBudgetSnapshot,
    memory_bytes: usize,
}

impl FeedHtmlBudget {
    pub(crate) fn new(
        max_output_bytes: u64,
        max_diagnostics: usize,
        max_memory_bytes: u64,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        Ok(Self {
            memory: LogicalMemory::new(context)?,
            nodes: 0,
            inlines: 0,
            assets: 0,
            diagnostics: 0,
            strings: 0,
            output_bytes: 0,
            persistent_memory_bytes: 0,
            max_nodes: MAX_DOCUMENT_NODES,
            max_inlines: MAX_DOCUMENT_INLINES,
            max_assets: MAX_DOCUMENT_NODES,
            max_diagnostics,
            max_strings: MAX_DOCUMENT_NODES
                .saturating_mul(8)
                .saturating_add(MAX_DOCUMENT_INLINES.saturating_mul(2)),
            max_output_bytes,
            max_persistent_memory_bytes: max_memory_bytes,
        })
    }

    pub(crate) fn charge_memory(&mut self, bytes: usize) -> Result<(), ConversionError> {
        let next = self.persistent_memory_bytes.checked_add(bytes).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "aggregate persistent output memory overflowed".into(),
            }
        })?;
        if u64::try_from(next).unwrap_or(u64::MAX) > self.max_persistent_memory_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: format!(
                    "aggregate persistent output memory exceeds {} bytes",
                    self.max_persistent_memory_bytes
                ),
            });
        }
        self.memory.charge(bytes)?;
        self.persistent_memory_bytes = next;
        Ok(())
    }

    pub(crate) fn prepay_parser_memory(&mut self, bytes: usize) -> Result<(), ConversionError> {
        let next = self.memory.mark().checked_add(bytes).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "aggregate HTML parser preflight memory overflowed".into(),
            }
        })?;
        if u64::try_from(next).unwrap_or(u64::MAX) > self.max_persistent_memory_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: format!(
                    "aggregate feed and nested HTML memory exceeds {} bytes",
                    self.max_persistent_memory_bytes
                ),
            });
        }
        self.memory.charge(bytes)
    }

    fn consume(
        current: &mut usize,
        maximum: usize,
        amount: usize,
        name: &'static str,
    ) -> Result<(), ConversionError> {
        let next = current.checked_add(amount).ok_or_else(|| ConversionError::ResourceLimit {
            limit: name,
            detail: format!("aggregate {name} count overflowed"),
        })?;
        if next > maximum {
            return Err(ConversionError::ResourceLimit {
                limit: name,
                detail: format!("aggregate {name} exceeds {maximum}"),
            });
        }
        *current = next;
        Ok(())
    }

    pub(crate) fn node(&mut self) -> Result<(), ConversionError> {
        Self::consume(&mut self.nodes, self.max_nodes, 1, "feed_nodes")
    }

    pub(crate) fn nodes(&mut self, count: usize) -> Result<(), ConversionError> {
        Self::consume(&mut self.nodes, self.max_nodes, count, "feed_nodes")
    }

    pub(crate) fn inline(&mut self) -> Result<(), ConversionError> {
        Self::consume(&mut self.inlines, self.max_inlines, 1, "feed_inlines")
    }

    pub(crate) fn inlines(&mut self, count: usize) -> Result<(), ConversionError> {
        Self::consume(&mut self.inlines, self.max_inlines, count, "feed_inlines")
    }

    pub(crate) fn asset(&mut self) -> Result<(), ConversionError> {
        Self::consume(&mut self.assets, self.max_assets, 1, "feed_assets")
    }

    /// Consume diagnostic/string/output quotas before the feed constructs the
    /// two owned strings. Their real allocator capacities are charged by
    /// `reserve_string_capacity`, so this method deliberately does not charge
    /// payload bytes a second time.
    pub(crate) fn begin_feed_diagnostic(
        &mut self,
        code_bytes: usize,
        message_bytes: usize,
    ) -> Result<(), ConversionError> {
        Self::consume(&mut self.diagnostics, self.max_diagnostics, 1, "feed_diagnostics")?;
        self.consume_strings(1, code_bytes)?;
        self.consume_strings(1, message_bytes)
    }

    fn html_diagnostic(
        &mut self,
        code_bytes: usize,
        message_bytes: usize,
    ) -> Result<(), ConversionError> {
        Self::consume(&mut self.diagnostics, self.max_diagnostics, 1, "feed_diagnostics")?;
        self.consume_strings(1, code_bytes)?;
        self.consume_strings(1, message_bytes)
    }

    pub(crate) fn strings(&mut self, count: usize, bytes: usize) -> Result<(), ConversionError> {
        self.consume_strings(count, bytes)?;
        self.charge_memory(bytes)
    }

    pub(crate) fn record_output_strings(
        &mut self,
        count: usize,
        bytes: usize,
    ) -> Result<(), ConversionError> {
        self.consume_strings(count, bytes)
    }

    fn consume_strings(&mut self, count: usize, bytes: usize) -> Result<(), ConversionError> {
        Self::consume(&mut self.strings, self.max_strings, count, "feed_output_strings")?;
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let next =
            self.output_bytes.checked_add(bytes).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_feed_text_bytes",
                detail: "aggregate feed output bytes overflowed".into(),
            })?;
        if next > self.max_output_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_feed_text_bytes",
                detail: format!("aggregate feed output exceeds {} bytes", self.max_output_bytes),
            });
        }
        self.output_bytes = next;
        Ok(())
    }

    fn new_output_string(&mut self, bytes: usize) -> Result<String, ConversionError> {
        let strings_mark = self.strings;
        let output_mark = self.output_bytes;
        self.consume_strings(1, bytes)?;
        let mut output = String::new();
        if let Err(error) = self.reserve_string_capacity(&mut output, bytes) {
            self.strings = strings_mark;
            self.output_bytes = output_mark;
            return Err(error);
        }
        record_feed_html_object(FeedHtmlObjectKind::String);
        Ok(output)
    }

    fn new_precounted_string(&mut self, bytes: usize) -> Result<String, ConversionError> {
        let mut output = String::new();
        self.reserve_string_capacity(&mut output, bytes)?;
        record_feed_html_object(FeedHtmlObjectKind::String);
        Ok(output)
    }

    pub(crate) fn reserve_string_capacity(
        &mut self,
        string: &mut String,
        additional: usize,
    ) -> Result<(), ConversionError> {
        let required =
            string.len().checked_add(additional).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "aggregate HTML string capacity overflowed".into(),
            })?;
        if required <= string.capacity() {
            return Ok(());
        }
        let target = required.max(string.capacity().saturating_mul(2)).max(64);
        let bytes = target.checked_sub(string.capacity()).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "aggregate HTML string capacity underflowed".into(),
            }
        })?;
        let memory_mark = self.memory.mark();
        let persistent_mark = self.persistent_memory_bytes;
        self.charge_memory(bytes)?;
        record_feed_html_capacity_reserve_call();
        if let Err(error) = string.try_reserve_exact(target - string.len()) {
            self.memory.rewind(memory_mark)?;
            self.persistent_memory_bytes = persistent_mark;
            return Err(ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: format!("aggregate HTML string allocation failed: {error}"),
            });
        }
        let Some(actual_bytes) = string.capacity().checked_sub(target) else {
            *string = String::new();
            self.memory.rewind(memory_mark)?;
            self.persistent_memory_bytes = persistent_mark;
            return Err(ConversionError::Internal {
                detail: "aggregate HTML string reserve returned less than requested capacity"
                    .into(),
            });
        };
        if actual_bytes > 0
            && let Err(error) = self.charge_memory(actual_bytes)
        {
            *string = String::new();
            self.memory.rewind(memory_mark)?;
            self.persistent_memory_bytes = persistent_mark;
            return Err(error);
        }
        record_feed_html_capacity_growth();
        Ok(())
    }

    /// Reserve logical allocator capacity before a persistent helper vector grows.
    ///
    /// Object limits are deliberately separate from this capacity accounting:
    /// the vector's slots account for the object representation, while owned
    /// `String`/nested-vector payloads account for their own capacities. This
    /// avoids charging `size_of::<T>()` once as an object and again as a slot.
    pub(crate) fn reserve_vec<T>(
        &mut self,
        vector: &mut Vec<T>,
        additional: usize,
    ) -> Result<(), ConversionError> {
        let required =
            vector.len().checked_add(additional).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "aggregate HTML vector capacity overflowed".into(),
            })?;
        if required <= vector.capacity() {
            return Ok(());
        }
        let target = required.max(vector.capacity().saturating_mul(2)).max(4);
        let bytes = target
            .checked_sub(vector.capacity())
            .and_then(|slots| slots.checked_mul(size_of::<T>()))
            .ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "aggregate HTML vector byte capacity overflowed".into(),
            })?;
        let memory_mark = self.memory.mark();
        let persistent_mark = self.persistent_memory_bytes;
        self.charge_memory(bytes)?;
        record_feed_html_capacity_reserve_call();
        if let Err(error) = vector.try_reserve_exact(target - vector.len()) {
            self.memory.rewind(memory_mark)?;
            self.persistent_memory_bytes = persistent_mark;
            return Err(ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: format!("aggregate HTML vector allocation failed: {error}"),
            });
        }
        let Some(actual_slots) = vector.capacity().checked_sub(target) else {
            *vector = Vec::new();
            self.memory.rewind(memory_mark)?;
            self.persistent_memory_bytes = persistent_mark;
            return Err(ConversionError::Internal {
                detail: "aggregate HTML vector reserve returned less than requested capacity"
                    .into(),
            });
        };
        if actual_slots > 0
            && let Err(error) =
                self.charge_memory(actual_slots.checked_mul(size_of::<T>()).ok_or_else(|| {
                    ConversionError::ResourceLimit {
                        limit: "max_memory_bytes",
                        detail: "aggregate HTML vector actual capacity overflowed".into(),
                    }
                })?)
        {
            *vector = Vec::new();
            self.memory.rewind(memory_mark)?;
            self.persistent_memory_bytes = persistent_mark;
            return Err(error);
        }
        record_feed_html_capacity_growth();
        Ok(())
    }

    /// Release a conservatively prepaid third-party/parser workspace range.
    /// Persistent allocations made while the range was held remain charged.
    pub(crate) fn release_parser_memory(&mut self, bytes: usize) -> Result<(), ConversionError> {
        self.memory.release(bytes)
    }

    /// Release an explicitly temporary capacity after its owner has been
    /// dropped, while retaining later persistent allocations in the lease.
    pub(crate) fn release_temporary_capacity(
        &mut self,
        bytes: usize,
    ) -> Result<(), ConversionError> {
        let next = self.persistent_memory_bytes.checked_sub(bytes).ok_or_else(|| {
            ConversionError::Internal {
                detail: "temporary capacity exceeds persistent feed memory".into(),
            }
        })?;
        self.memory.release(bytes)?;
        self.persistent_memory_bytes = next;
        Ok(())
    }

    /// Validate a temporary-capacity release before a transaction publishes
    /// any values. No allocation or counter mutation occurs here. A subsequent
    /// release of the same byte count is infallible unless the budget has an
    /// internal accounting defect; diagnostic publication performs no
    /// fallible operation between this check and that release.
    pub(crate) fn validate_temporary_capacity_release(
        &self,
        bytes: usize,
    ) -> Result<(), ConversionError> {
        if bytes > self.persistent_memory_bytes || bytes > self.memory.mark() {
            return Err(ConversionError::Internal {
                detail: "temporary capacity exceeds persistent feed memory".into(),
            });
        }
        Ok(())
    }

    fn constructed(kind: FeedHtmlObjectKind) {
        record_feed_html_object(kind);
    }

    /// Update only the logical output-byte total for replacing an already
    /// counted string. The caller reserves the replacement's real allocator
    /// capacity separately and releases the old capacity after dropping it.
    pub(crate) fn replacement_output_growth(
        &mut self,
        old_bytes: usize,
        new_bytes: usize,
    ) -> Result<(), ConversionError> {
        let growth = u64::try_from(new_bytes.saturating_sub(old_bytes)).unwrap_or(u64::MAX);
        let next = self.output_bytes.checked_add(growth).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_feed_text_bytes",
                detail: "aggregate feed output bytes overflowed".into(),
            }
        })?;
        if next > self.max_output_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_feed_text_bytes",
                detail: format!("aggregate feed output exceeds {} bytes", self.max_output_bytes),
            });
        }
        self.output_bytes = next;
        Ok(())
    }

    pub(crate) const fn snapshot(&self) -> FeedHtmlBudgetSnapshot {
        FeedHtmlBudgetSnapshot {
            nodes: self.nodes,
            inlines: self.inlines,
            assets: self.assets,
            diagnostics: self.diagnostics,
            strings: self.strings,
            output_bytes: self.output_bytes,
            persistent_memory_bytes: self.persistent_memory_bytes,
        }
    }

    pub(crate) fn transaction_snapshot(&self) -> FeedHtmlTransactionSnapshot {
        FeedHtmlTransactionSnapshot { counters: self.snapshot(), memory_bytes: self.memory.mark() }
    }

    /// Restore a fragment transaction after every value created since the
    /// snapshot has been dropped by the caller.
    pub(crate) fn rewind(
        &mut self,
        snapshot: FeedHtmlTransactionSnapshot,
    ) -> Result<(), ConversionError> {
        self.memory.rewind(snapshot.memory_bytes)?;
        self.nodes = snapshot.counters.nodes;
        self.inlines = snapshot.counters.inlines;
        self.assets = snapshot.counters.assets;
        self.diagnostics = snapshot.counters.diagnostics;
        self.strings = snapshot.counters.strings;
        self.output_bytes = snapshot.counters.output_bytes;
        self.persistent_memory_bytes = snapshot.counters.persistent_memory_bytes;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_test_limits(&mut self, limits: FeedHtmlBudgetSnapshot) {
        self.max_nodes = limits.nodes;
        self.max_inlines = limits.inlines;
        self.max_assets = limits.assets;
        self.max_diagnostics = limits.diagnostics;
        self.max_strings = limits.strings;
        self.max_output_bytes = limits.output_bytes;
        self.max_persistent_memory_bytes =
            u64::try_from(limits.persistent_memory_bytes).unwrap_or(u64::MAX);
    }
}

#[derive(Clone, Copy)]
enum FeedHtmlObjectKind {
    Node,
    Inline,
    Asset,
    Diagnostic,
    String,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FeedHtmlObjectCounts {
    pub(crate) nodes: usize,
    pub(crate) inlines: usize,
    pub(crate) assets: usize,
    pub(crate) diagnostics: usize,
    pub(crate) strings: usize,
    pub(crate) capacity_growths: usize,
    pub(crate) capacity_reserve_calls: usize,
    pub(crate) parser_constructions: usize,
}

#[cfg(test)]
thread_local! {
    static FEED_HTML_OBJECTS: MutCell<FeedHtmlObjectCounts> = const {
        MutCell::new(FeedHtmlObjectCounts {
            nodes: 0,
            inlines: 0,
            assets: 0,
            diagnostics: 0,
            strings: 0,
            capacity_growths: 0,
            capacity_reserve_calls: 0,
            parser_constructions: 0,
        })
    };
}

#[cfg(test)]
fn record_feed_html_capacity_growth() {
    FEED_HTML_OBJECTS.with(|count| {
        let mut current = count.get();
        current.capacity_growths = current.capacity_growths.saturating_add(1);
        count.set(current);
    });
}

#[cfg(not(test))]
fn record_feed_html_capacity_growth() {}

#[cfg(test)]
fn record_feed_html_capacity_reserve_call() {
    FEED_HTML_OBJECTS.with(|count| {
        let mut current = count.get();
        current.capacity_reserve_calls = current.capacity_reserve_calls.saturating_add(1);
        count.set(current);
    });
}

#[cfg(not(test))]
fn record_feed_html_capacity_reserve_call() {}

#[cfg(test)]
fn record_feed_html_parser_construction() {
    FEED_HTML_OBJECTS.with(|count| {
        let mut current = count.get();
        current.parser_constructions = current.parser_constructions.saturating_add(1);
        count.set(current);
    });
}

#[cfg(not(test))]
fn record_feed_html_parser_construction() {}

#[cfg(test)]
fn record_feed_html_object(kind: FeedHtmlObjectKind) {
    FEED_HTML_OBJECTS.with(|count| {
        let mut current = count.get();
        let target = match kind {
            FeedHtmlObjectKind::Node => &mut current.nodes,
            FeedHtmlObjectKind::Inline => &mut current.inlines,
            FeedHtmlObjectKind::Asset => &mut current.assets,
            FeedHtmlObjectKind::Diagnostic => &mut current.diagnostics,
            FeedHtmlObjectKind::String => &mut current.strings,
        };
        *target = target.saturating_add(1);
        count.set(current);
    });
}

#[cfg(not(test))]
fn record_feed_html_object(_: FeedHtmlObjectKind) {}

#[cfg(test)]
pub(crate) fn reset_feed_html_object_count() {
    FEED_HTML_OBJECTS.with(|count| count.set(FeedHtmlObjectCounts::default()));
}

#[cfg(test)]
pub(crate) fn feed_html_object_count() -> FeedHtmlObjectCounts {
    FEED_HTML_OBJECTS.with(MutCell::get)
}

/// Browser-compatible HTML5 parser with an offline semantic extractor.
#[derive(Debug, Default)]
pub struct HtmlConverter;

impl Converter for HtmlConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn priority(&self) -> i32 {
        210
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
            if candidate.format != InputFormat::Html {
                return Ok(ProbeOutcome::NotApplicable);
            }
            let evidence =
                match super::bounded_utf8_prefix(&input.bytes, super::TEXT_INSPECTION_BYTE_LIMIT) {
                    Some((text, _)) => super::html_document_evidence(text, context)?,
                    None => false,
                };
            Ok(
                if candidate.explicit
                    || candidate.detector_id == "builtin.detector.hints"
                    || evidence
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
        Box::pin(async move { convert_html(input, options, context) })
    }
}

#[derive(Clone)]
enum NodeData {
    Document,
    Element { name: QualName, attrs: Vec<Attribute>, template: Option<usize> },
    Text(String),
    Other,
}

#[derive(Clone)]
struct DomNode {
    parent: Option<usize>,
    children: Vec<usize>,
    depth: usize,
    data: NodeData,
}

struct Dom {
    nodes: RefCell<Vec<DomNode>>,
    error: RefCell<Option<ConversionError>>,
    parse_errors: MutCell<usize>,
    events: MutCell<usize>,
    max_depth: usize,
    context: ExecutionContext,
    memory: RefCell<LogicalMemory>,
}

impl Dom {
    fn new(
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        Self::with_memory(options, context, LogicalMemory::new(context)?)
    }

    fn with_memory(
        options: &ConversionOptions,
        context: &ExecutionContext,
        mut memory: LogicalMemory,
    ) -> Result<Self, ConversionError> {
        let mut nodes = Vec::new();
        memory.reserve_vec(&mut nodes, 2)?;
        nodes.push(DomNode {
            parent: None,
            children: Vec::new(),
            depth: 0,
            data: NodeData::Document,
        });
        nodes.push(DomNode {
            parent: None,
            children: Vec::new(),
            depth: 0,
            data: NodeData::Element {
                name: QualName::new(None, html5ever::ns!(html), html5ever::local_name!("span")),
                attrs: Vec::new(),
                template: None,
            },
        });
        Ok(Self {
            nodes: RefCell::new(nodes),
            error: RefCell::new(None),
            parse_errors: MutCell::new(0),
            events: MutCell::new(0),
            max_depth: usize::from(options.limits.max_nesting_depth),
            context: context.clone(),
            memory: RefCell::new(memory),
        })
    }

    fn event(&self) -> bool {
        if self.poisoned() {
            return false;
        }
        let next = self.events.get().saturating_add(1);
        self.events.set(next);
        if next > MAX_HTML_EVENTS {
            self.set_error_once(ConversionError::ResourceLimit {
                limit: "html_events",
                detail: format!("HTML parser exceeded {MAX_HTML_EVENTS} tree events"),
            });
            return false;
        }
        if next.is_multiple_of(CHECKPOINT_EVENTS)
            && let Err(error) = self.context.checkpoint()
        {
            self.set_error_once(error);
            return false;
        }
        true
    }

    fn poisoned(&self) -> bool {
        self.error.borrow().is_some()
    }

    fn set_error_once(&self, error: ConversionError) {
        let mut first = self.error.borrow_mut();
        if first.is_none() {
            *first = Some(error);
        }
    }

    fn add(&self, data: NodeData) -> usize {
        if !self.event() {
            return 1;
        }
        let mut nodes = self.nodes.borrow_mut();
        if nodes.len() >= MAX_DOCUMENT_NODES {
            self.set_error_once(ConversionError::ResourceLimit {
                limit: "html_nodes",
                detail: format!("HTML DOM exceeded {MAX_DOCUMENT_NODES} nodes"),
            });
            return 1;
        }
        if let NodeData::Element { attrs, .. } = &data {
            let attribute_storage =
                attrs.capacity().saturating_mul(size_of::<Attribute>()).saturating_add(
                    attrs
                        .iter()
                        .map(|attribute| {
                            attribute.name.local.len().saturating_add(attribute.value.len())
                        })
                        .sum::<usize>(),
                );
            if let Err(error) = self.memory.borrow_mut().charge(attribute_storage) {
                self.set_error_once(error);
                return 1;
            }
        }
        if let Err(error) = self.memory.borrow_mut().reserve_vec(&mut nodes, 1) {
            self.set_error_once(error);
            return 1;
        }
        let id = nodes.len();
        nodes.push(DomNode { parent: None, children: Vec::new(), depth: 0, data });
        id
    }

    fn detach(&self, child: usize) {
        if self.poisoned() {
            return;
        }
        let parent = self.nodes.borrow().get(child).and_then(|node| node.parent);
        if let Some(parent) = parent {
            let mut nodes = self.nodes.borrow_mut();
            if let Some(position) = nodes[parent].children.iter().position(|id| *id == child) {
                nodes[parent].children.remove(position);
            }
            nodes[child].parent = None;
        }
    }

    fn insert(&self, parent: usize, child: usize, before: Option<usize>) {
        if self.poisoned() {
            return;
        }
        if !self.event()
            || parent >= self.nodes.borrow().len()
            || child >= self.nodes.borrow().len()
        {
            return;
        }
        self.detach(child);
        let depth = self.nodes.borrow()[parent].depth.saturating_add(1);
        let relative_height = {
            let nodes = self.nodes.borrow();
            let base = nodes[child].depth;
            let mut height = 0;
            let mut stack = Vec::new();
            if let Err(error) = self.memory.borrow_mut().reserve_vec(&mut stack, 1) {
                drop(nodes);
                self.set_error_once(error);
                return;
            }
            stack.push(child);
            while let Some(id) = stack.pop() {
                height = height.max(nodes[id].depth.saturating_sub(base));
                if let Err(error) =
                    self.memory.borrow_mut().reserve_vec(&mut stack, nodes[id].children.len())
                {
                    drop(nodes);
                    self.set_error_once(error);
                    return;
                }
                stack.extend(nodes[id].children.iter().copied());
            }
            height
        };
        if depth.saturating_add(relative_height) > self.max_depth {
            self.set_error_once(ConversionError::ResourceLimit {
                limit: "html_nesting_depth",
                detail: format!("HTML DOM exceeded {} levels", self.max_depth),
            });
            return;
        }
        let mut nodes = self.nodes.borrow_mut();
        let old_depth = nodes[child].depth;
        nodes[child].parent = Some(parent);
        let position = before
            .and_then(|id| nodes[parent].children.iter().position(|child| *child == id))
            .unwrap_or(nodes[parent].children.len());
        if let Err(error) = self.memory.borrow_mut().reserve_vec(&mut nodes[parent].children, 1) {
            drop(nodes);
            self.set_error_once(error);
            return;
        }
        nodes[parent].children.insert(position, child);
        let mut stack = Vec::new();
        if let Err(error) = self.memory.borrow_mut().reserve_vec(&mut stack, 1) {
            drop(nodes);
            self.set_error_once(error);
            return;
        }
        stack.push(child);
        while let Some(id) = stack.pop() {
            let relative = nodes[id].depth.saturating_sub(old_depth);
            nodes[id].depth = depth.saturating_add(relative);
            if let Err(error) =
                self.memory.borrow_mut().reserve_vec(&mut stack, nodes[id].children.len())
            {
                drop(nodes);
                self.set_error_once(error);
                return;
            }
            stack.extend(nodes[id].children.iter().copied());
        }
    }

    fn append_item(&self, parent: usize, item: NodeOrText<usize>, before: Option<usize>) {
        if self.poisoned() {
            return;
        }
        match item {
            NodeOrText::AppendNode(child) => self.insert(parent, child, before),
            NodeOrText::AppendText(text) => {
                if text.is_empty() {
                    return;
                }
                let previous = {
                    let nodes = self.nodes.borrow();
                    let position = before
                        .and_then(|id| nodes[parent].children.iter().position(|child| *child == id))
                        .unwrap_or(nodes[parent].children.len());
                    position
                        .checked_sub(1)
                        .and_then(|index| nodes[parent].children.get(index))
                        .copied()
                };
                if let Some(previous) = previous {
                    let mut nodes = self.nodes.borrow_mut();
                    if let NodeData::Text(value) = &mut nodes[previous].data {
                        if let Err(error) =
                            self.memory.borrow_mut().reserve_string(value, text.len())
                        {
                            self.set_error_once(error);
                        } else {
                            value.push_str(&text);
                        }
                        return;
                    }
                }
                let mut value = String::new();
                if let Err(error) = self.memory.borrow_mut().reserve_string(&mut value, text.len())
                {
                    self.set_error_once(error);
                    return;
                }
                value.push_str(&text);
                let child = self.add(NodeData::Text(value));
                self.insert(parent, child, before);
            }
        }
    }
}

impl TreeSink for Dom {
    type Handle = usize;
    type Output = Self;
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) -> Self {
        self
    }
    fn parse_error(&self, _: Cow<'static, str>) {
        if self.poisoned() {
            return;
        }
        self.parse_errors.set(self.parse_errors.get().saturating_add(1));
    }
    fn get_document(&self) -> usize {
        if self.poisoned() {
            return 0;
        }
        0
    }
    fn elem_name<'a>(&'a self, target: &'a usize) -> Self::ElemName<'a> {
        if self.poisoned() {
            return Ref::map(self.nodes.borrow(), |nodes| match &nodes[1].data {
                NodeData::Element { name, .. } => name,
                _ => unreachable!("sentinel is always an element"),
            });
        }
        Ref::map(self.nodes.borrow(), |nodes| match &nodes[*target].data {
            NodeData::Element { name, .. } => name,
            _ => unreachable!("html5ever requested a name for a non-element handle"),
        })
    }
    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> usize {
        if self.poisoned() {
            return 1;
        }
        let template = flags.template.then(|| self.add(NodeData::Document));
        if self.poisoned() {
            return 1;
        }
        let element = self.add(NodeData::Element { name, attrs, template });
        if self.poisoned() {
            return 1;
        }
        if let Some(template) = template {
            self.nodes.borrow_mut()[template].parent = Some(element);
        }
        element
    }
    fn create_comment(&self, _: StrTendril) -> usize {
        if self.poisoned() {
            return 1;
        }
        self.add(NodeData::Other)
    }
    fn create_pi(&self, _: StrTendril, _: StrTendril) -> usize {
        if self.poisoned() {
            return 1;
        }
        self.add(NodeData::Other)
    }
    fn append(&self, parent: &usize, child: NodeOrText<usize>) {
        if self.poisoned() {
            return;
        }
        self.append_item(*parent, child, None);
    }
    fn append_before_sibling(&self, sibling: &usize, child: NodeOrText<usize>) {
        if self.poisoned() {
            return;
        }
        let parent = self.nodes.borrow().get(*sibling).and_then(|node| node.parent).unwrap_or(0);
        self.append_item(parent, child, Some(*sibling));
    }
    fn append_based_on_parent_node(
        &self,
        element: &usize,
        previous: &usize,
        child: NodeOrText<usize>,
    ) {
        if self.poisoned() {
            return;
        }
        if self.nodes.borrow().get(*element).and_then(|node| node.parent).is_some() {
            self.append_before_sibling(element, child);
        } else {
            self.append(previous, child);
        }
    }
    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {
        if self.poisoned() {
            return;
        }
        let _ = self.event();
    }
    fn mark_script_already_started(&self, _: &usize) {
        let _ = self.poisoned();
    }
    fn pop(&self, _: &usize) {
        let _ = self.poisoned();
    }
    fn get_template_contents(&self, target: &usize) -> usize {
        if self.poisoned() {
            return 1;
        }
        match &self.nodes.borrow()[*target].data {
            NodeData::Element { template: Some(id), .. } => *id,
            _ => 1,
        }
    }
    fn same_node(&self, x: &usize, y: &usize) -> bool {
        if self.poisoned() {
            return false;
        }
        x == y
    }
    fn set_quirks_mode(&self, _: QuirksMode) {
        let _ = self.poisoned();
    }
    fn add_attrs_if_missing(&self, target: &usize, attrs: Vec<Attribute>) {
        if self.poisoned() {
            return;
        }
        let logical =
            attrs.iter().map(|attr| attr.name.local.len().saturating_add(attr.value.len())).sum();
        if let Err(error) = self.memory.borrow_mut().charge(logical) {
            self.set_error_once(error);
            return;
        }
        let mut nodes = self.nodes.borrow_mut();
        if let NodeData::Element { attrs: existing, .. } = &mut nodes[*target].data {
            let additional = attrs
                .iter()
                .filter(|attr| !existing.iter().any(|present| present.name == attr.name))
                .count();
            if let Err(error) = self.memory.borrow_mut().reserve_vec(existing, additional) {
                drop(nodes);
                self.set_error_once(error);
                return;
            }
            for attr in attrs {
                if !existing.iter().any(|present| present.name == attr.name) {
                    existing.push(attr);
                }
            }
        }
    }
    fn associate_with_form(&self, _: &usize, _: &usize, _: (&usize, Option<&usize>)) {
        let _ = self.poisoned();
    }
    fn remove_from_parent(&self, target: &usize) {
        if self.poisoned() {
            return;
        }
        self.detach(*target);
    }
    fn reparent_children(&self, node: &usize, new_parent: &usize) {
        if self.poisoned() {
            return;
        }
        let mut children = Vec::new();
        let child_count = self.nodes.borrow()[*node].children.len();
        if let Err(error) = self.memory.borrow_mut().reserve_vec(&mut children, child_count) {
            self.set_error_once(error);
            return;
        }
        children.extend_from_slice(&self.nodes.borrow()[*node].children);
        for child in children {
            self.insert(*new_parent, child, None);
        }
    }
    fn is_mathml_annotation_xml_integration_point(&self, _: &usize) -> bool {
        if self.poisoned() {
            return false;
        }
        false
    }
    fn set_current_line(&self, _: u64) {
        let _ = self.poisoned();
    }
    fn allow_declarative_shadow_roots(&self, _: &usize) -> bool {
        if self.poisoned() {
            return false;
        }
        false
    }
    fn attach_declarative_shadow(&self, _: &usize, _: &usize, _: &[Attribute]) -> bool {
        if self.poisoned() {
            return false;
        }
        false
    }
    fn maybe_clone_an_option_into_selectedcontent(&self, _: &usize) {
        let _ = self.poisoned();
    }
}

pub(crate) fn convert_html(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    convert_html_with_images(input, &[], options, context)
}

/// A container-owned image that may replace one exact canonical `cid:` reference.
pub(crate) struct EmbeddedImage {
    pub(crate) cid: String,
    pub(crate) asset: AssetId,
}

/// Convert isolated HTML while resolving only caller-audited embedded image references.
pub(crate) fn convert_embedded_html_with_images(
    bytes: &[u8],
    images: &[EmbeddedImage],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let input = ResolvedInput {
        bytes: std::sync::Arc::from(bytes),
        metadata: into_markdown_core::SourceMetadata {
            media_type: Some("text/html".into()),
            size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            ..into_markdown_core::SourceMetadata::default()
        },
    };
    convert_html_with_images(&input, images, options, context)
}

fn convert_html_with_images(
    input: &ResolvedInput,
    images: &[EmbeddedImage],
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
    let (charset, charset_diagnostics) = html_charset(input, options, context)?;
    let (mut decoded, decoded_diagnostics) =
        decode_source(&input.bytes, charset.as_deref(), options.text.decoding_mode, context)?;
    let mut diagnostics = Vec::new();
    diagnostics.extend(decoded_diagnostics);
    diagnostics.extend(charset_diagnostics);
    // This reservation represents cooperative parser work, not html5ever's allocator or RSS.
    let event_units = decoded.text.len().saturating_mul(4).min(MAX_HTML_EVENTS);
    let parser_work = decoded
        .text
        .len()
        .saturating_mul(2)
        .saturating_add(event_units.saturating_mul(size_of::<usize>()));
    decoded.memory.charge(parser_work)?;
    let sink = Dom::new(options, context)?;
    let dom = parse_html_dom(sink, decoded.text.as_str());
    finish_html_dom(
        dom,
        input.bytes.len(),
        input.metadata.uri.as_deref(),
        Some(decoded),
        images,
        options,
        context,
        diagnostics,
        None,
    )
}

fn parse_html_dom(sink: Dom, text: &str) -> Dom {
    let parse_options = ParseOpts {
        tree_builder: html5ever::tree_builder::TreeBuilderOpts {
            scripting_enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    parse_document(sink, parse_options).one(text)
}

#[allow(clippy::too_many_arguments)]
fn finish_html_dom(
    dom: Dom,
    input_len: usize,
    source_uri: Option<&str>,
    decoded: Option<DecodedText>,
    embedded_images: &[EmbeddedImage],
    options: &ConversionOptions,
    context: &ExecutionContext,
    mut diagnostics: Vec<Diagnostic>,
    mut feed_budget: Option<&mut FeedHtmlBudget>,
) -> Result<ConverterOutput, ConversionError> {
    if let Some(error) = dom.error.into_inner() {
        return Err(error);
    }
    if dom.parse_errors.get() > 0 {
        let count = dom.parse_errors.get();
        let diagnostic = if let Some(budget) = feed_budget.as_deref_mut() {
            let message_len = "HTML5 parser recovered from ".len()
                + decimal_digits(count)
                + " syntax error(s)".len();
            budget.html_diagnostic("html.parseRecovered".len(), message_len)?;
            budget.reserve_vec(&mut diagnostics, 1)?;
            budgeted_warning(budget, "html.parseRecovered", message_len, |message| {
                write!(message, "HTML5 parser recovered from {count} syntax error(s)")
            })?
        } else {
            warning(
                "html.parseRecovered",
                format!("HTML5 parser recovered from {count} syntax error(s)"),
            )
        };
        diagnostics.push(diagnostic);
    }
    let source_diagnostic = if let Some(budget) = feed_budget.as_deref_mut() {
        budget.html_diagnostic(
            "html.sourceLocationUnavailable".len(),
            SOURCE_LOCATION_MESSAGE.len(),
        )?;
        budget.reserve_vec(&mut diagnostics, 1)?;
        budgeted_warning(
            budget,
            "html.sourceLocationUnavailable",
            SOURCE_LOCATION_MESSAGE.len(),
            |message| message.write_str(SOURCE_LOCATION_MESSAGE),
        )?
    } else {
        warning("html.sourceLocationUnavailable", SOURCE_LOCATION_MESSAGE.into())
    };
    diagnostics.push(source_diagnostic);

    let nodes = dom.nodes.into_inner();
    let builder = Builder::new(
        &nodes,
        input_len,
        source_uri,
        decoded,
        embedded_images,
        options,
        context,
        diagnostics,
        feed_budget,
    );
    builder.extract()
}

/// Parse an already decoded feed HTML fragment through the same security and
/// semantic extraction path as a standalone HTML document.
pub(crate) fn convert_feed_html_fragment(
    fragment: &str,
    base_uri: Option<&str>,
    options: &ConversionOptions,
    context: &ExecutionContext,
    budget: &mut FeedHtmlBudget,
) -> Result<ConverterOutput, ConversionError> {
    let snapshot = budget.transaction_snapshot();
    let preflight = preflight_feed_html(fragment.as_bytes(), context)?;
    let parser_memory = preflight.memory_bound()?;
    budget.prepay_parser_memory(parser_memory)?;
    let result = (|| {
        // Feed XML decoding already produced valid UTF-8. Bypassing the HTML
        // charset decoder avoids a redundant owned copy and lets the same
        // feed-owned lease cover parser workspace, DOM, and final output.
        let sink = Dom::with_memory(options, context, LogicalMemory::prepaid(parser_memory))?;
        record_feed_html_parser_construction();
        let dom = parse_html_dom(sink, fragment);
        finish_html_dom(
            dom,
            fragment.len(),
            base_uri,
            None,
            &[],
            options,
            context,
            Vec::new(),
            Some(budget),
        )
    })();
    match result {
        Ok(output) => {
            if let Err(error) = budget.memory.release(parser_memory) {
                drop(output);
                budget.rewind(snapshot)?;
                return Err(error);
            }
            Ok(output)
        }
        Err(error) => {
            // `result` owns and drops parser, DOM, and partial output before
            // the full fragment transaction is rewound.
            budget.rewind(snapshot)?;
            Err(error)
        }
    }
}

fn html_charset(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Option<String>, Vec<Diagnostic>), ConversionError> {
    let explicit = options
        .text
        .charset
        .as_deref()
        .or_else(|| input.metadata.media_type.as_deref().and_then(media_type_charset));
    if let Some(explicit) = explicit {
        let mut diagnostics = Vec::new();
        if let Some(meta) = prescan_meta_charset(&input.bytes, context)?
            && !meta.eq_ignore_ascii_case(explicit)
        {
            diagnostics.push(warning(
                "html.metaCharsetIgnored",
                format!("meta charset {meta} conflicts with explicit charset {explicit}"),
            ));
        }
        return Ok((Some(explicit.to_owned()), diagnostics));
    }
    Ok((prescan_meta_charset(&input.bytes, context)?, Vec::new()))
}

fn media_type_charset(value: &str) -> Option<&str> {
    value
        .split(';')
        .skip(1)
        .find_map(|parameter| {
            let (name, value) = parameter.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches(['\'', '"']))
        })
        .filter(|value| !value.is_empty())
}

fn prescan_meta_charset(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Option<String>, ConversionError> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf])
        || bytes.starts_with(&[0xff, 0xfe])
        || bytes.starts_with(&[0xfe, 0xff])
    {
        return Ok(None);
    }
    let Some(sample) = bytes.get(..bytes.len().min(META_PRESCAN_BYTES)) else {
        return Ok(None);
    };
    if !sample
        .iter()
        .all(|byte| *byte == b'\t' || *byte == b'\n' || *byte == b'\r' || *byte >= 0x20)
    {
        return Ok(None);
    }
    let mut offset = 0;
    let mut steps = 0_usize;
    while offset < sample.len() {
        steps = steps.saturating_add(1);
        if steps.is_multiple_of(128) {
            context.checkpoint()?;
        }
        if ascii_prefix_at(sample, offset, b"<!--") {
            offset = find_ascii(sample, offset.saturating_add(4), b"-->", context)?
                .map_or(sample.len(), |end| end.saturating_add(3));
            continue;
        }
        if sample.get(offset) != Some(&b'<') {
            offset += 1;
            continue;
        }
        let Some((name, end)) = scan_start_tag(sample, offset, context)? else {
            offset += 1;
            continue;
        };
        if name.eq_ignore_ascii_case(b"script") || name.eq_ignore_ascii_case(b"style") {
            offset = find_raw_text_end(sample, end, name, context)?;
            continue;
        }
        if name.eq_ignore_ascii_case(b"meta") {
            let Some(tag) = sample.get(offset..end) else {
                return Ok(None);
            };
            let direct = meta_attribute(tag, b"charset", context)?;
            let legacy = if meta_attribute(tag, b"http-equiv", context)?
                .is_some_and(|value| value.eq_ignore_ascii_case(b"content-type"))
            {
                meta_attribute(tag, b"content", context)?.and_then(extract_charset_from_content)
            } else {
                None
            };
            if let Some(value) = direct
                .or(legacy)
                .filter(|value| !value.is_empty() && value.iter().all(u8::is_ascii))
            {
                return Ok(String::from_utf8(value.to_vec()).ok());
            }
        }
        offset = end.max(offset.saturating_add(1));
    }
    Ok(None)
}

fn meta_attribute<'a>(
    tag: &'a [u8],
    wanted: &[u8],
    context: &ExecutionContext,
) -> Result<Option<&'a [u8]>, ConversionError> {
    let mut offset = 5;
    while offset < tag.len() {
        if offset.is_multiple_of(128) {
            context.checkpoint()?;
        }
        while tag.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        if matches!(tag.get(offset), None | Some(b'>' | b'/')) {
            break;
        }
        let name_start = offset;
        while tag.get(offset).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'-') {
            offset += 1;
        }
        if offset == name_start {
            offset += 1;
            continue;
        }
        let Some(name) = tag.get(name_start..offset) else { return Ok(None) };
        while tag.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        if tag.get(offset) != Some(&b'=') {
            offset = offset.max(name_start.saturating_add(1));
            continue;
        }
        offset += 1;
        while tag.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        let value = if matches!(tag.get(offset), Some(b'\'' | b'\"')) {
            let Some(quote) = tag.get(offset).copied() else { return Ok(None) };
            offset += 1;
            let start = offset;
            let Some(rest) = tag.get(offset..) else { return Ok(None) };
            let Some(length) = rest.iter().position(|byte| *byte == quote) else {
                return Ok(None);
            };
            offset += length;
            let Some(value) = tag.get(start..offset) else { return Ok(None) };
            offset += 1;
            value
        } else {
            let start = offset;
            while tag
                .get(offset)
                .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b'>' | b'/'))
            {
                offset += 1;
            }
            let Some(value) = tag.get(start..offset) else { return Ok(None) };
            value
        };
        if name.eq_ignore_ascii_case(wanted) {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn scan_start_tag<'a>(
    bytes: &'a [u8],
    start: usize,
    context: &ExecutionContext,
) -> Result<Option<(&'a [u8], usize)>, ConversionError> {
    let mut offset = start.saturating_add(1);
    let name_start = offset;
    while bytes.get(offset).is_some_and(u8::is_ascii_alphabetic) {
        offset += 1;
    }
    let Some(name) = bytes.get(name_start..offset).filter(|name| !name.is_empty()) else {
        return Ok(None);
    };
    if !bytes.get(offset).is_some_and(|byte| is_html_space(*byte) || matches!(byte, b'/' | b'>')) {
        return Ok(None);
    }
    let Some(end) = find_tag_end_checked(bytes, offset, context)? else { return Ok(None) };
    Ok(Some((name, end)))
}

fn find_tag_end_checked(
    bytes: &[u8],
    mut offset: usize,
    context: &ExecutionContext,
) -> Result<Option<usize>, ConversionError> {
    let mut quote = None;
    while let Some(byte) = bytes.get(offset).copied() {
        if offset.is_multiple_of(128) {
            context.checkpoint()?;
        }
        match (quote, byte) {
            (Some(active), value) if active == value => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return Ok(Some(offset.saturating_add(1))),
            _ => {}
        }
        offset += 1;
    }
    Ok(None)
}

fn find_raw_text_end(
    bytes: &[u8],
    mut offset: usize,
    name: &[u8],
    context: &ExecutionContext,
) -> Result<usize, ConversionError> {
    while offset < bytes.len() {
        if offset.is_multiple_of(128) {
            context.checkpoint()?;
        }
        if bytes.get(offset..offset.saturating_add(2)) == Some(b"</") {
            let name_start = offset.saturating_add(2);
            let name_end = name_start.saturating_add(name.len());
            if bytes
                .get(name_start..name_end)
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
                && bytes
                    .get(name_end)
                    .is_some_and(|byte| is_html_space(*byte) || matches!(byte, b'/' | b'>'))
                && let Some(end) = find_tag_end_checked(bytes, name_end, context)?
            {
                return Ok(end);
            }
        }
        offset += 1;
    }
    Ok(bytes.len())
}

fn find_ascii(
    bytes: &[u8],
    mut offset: usize,
    needle: &[u8],
    context: &ExecutionContext,
) -> Result<Option<usize>, ConversionError> {
    while offset.saturating_add(needle.len()) <= bytes.len() {
        if offset.is_multiple_of(128) {
            context.checkpoint()?;
        }
        if ascii_prefix_at(bytes, offset, needle) {
            return Ok(Some(offset));
        }
        offset += 1;
    }
    Ok(None)
}

fn ascii_prefix_at(bytes: &[u8], offset: usize, needle: &[u8]) -> bool {
    bytes
        .get(offset..offset.saturating_add(needle.len()))
        .is_some_and(|value| value.eq_ignore_ascii_case(needle))
}

fn extract_charset_from_content(content: &[u8]) -> Option<&[u8]> {
    let mut offset = 0_usize;
    while offset.saturating_add(7) <= content.len() {
        if ascii_prefix_at(content, offset, b"charset") {
            let mut value = offset.saturating_add(7);
            while content.get(value).is_some_and(|byte| is_html_space(*byte)) {
                value += 1;
            }
            if content.get(value) != Some(&b'=') {
                offset += 1;
                continue;
            }
            value += 1;
            while content.get(value).is_some_and(|byte| is_html_space(*byte)) {
                value += 1;
            }
            let end = content
                .get(value..)?
                .iter()
                .position(|byte| is_html_space(*byte) || *byte == b';')
                .map_or(content.len(), |length| value.saturating_add(length));
            return content.get(value..end);
        }
        offset += 1;
    }
    None
}

fn is_html_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | 0x0c)
}

struct Builder<'a, 'budget> {
    nodes: &'a [DomNode],
    input_len: usize,
    source_uri: Option<&'a str>,
    embedded_images: &'a [EmbeddedImage],
    _decoded: Option<DecodedText>,
    context: &'a ExecutionContext,
    diagnostics: Vec<Diagnostic>,
    blocks: Vec<BlockNode>,
    assets: Vec<Asset>,
    metadata: DocumentMetadata,
    next_node: usize,
    base: Option<Url>,
    inline_count: usize,
    max_table_rows: u64,
    max_table_columns: u64,
    max_table_cells: u64,
    limits: into_markdown_core::ResourceLimits,
    total_asset_bytes: u64,
    feed_budget: Option<&'budget mut FeedHtmlBudget>,
}

#[derive(Clone, Copy, Default)]
struct NodeContext(u8);

impl NodeContext {
    const HIDDEN: u8 = 1;
    const BOILERPLATE: u8 = 1 << 1;
    const FOREIGN: u8 = 1 << 2;
    const TEMPLATE: u8 = 1 << 3;
    const HEAD: u8 = 1 << 4;

    fn mark(&mut self, flag: u8, value: bool) {
        if value {
            self.0 |= flag;
        }
    }

    const fn excluded(self) -> bool {
        self.0 & (Self::HIDDEN | Self::BOILERPLATE | Self::FOREIGN | Self::TEMPLATE) != 0
    }

    const fn in_head(self) -> bool {
        self.0 & Self::HEAD != 0
    }
}

struct PlannedTableCell {
    node: usize,
    column: usize,
    row_span: u32,
    column_span: u32,
}

struct PlannedTable {
    rows: Vec<Vec<PlannedTableCell>>,
    width: usize,
}

#[derive(Clone, Copy)]
struct SourceTableRow {
    node: usize,
    group: usize,
}

impl<'a, 'budget> Builder<'a, 'budget> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        nodes: &'a [DomNode],
        input_len: usize,
        source_uri: Option<&'a str>,
        decoded: Option<DecodedText>,
        embedded_images: &'a [EmbeddedImage],
        options: &ConversionOptions,
        context: &'a ExecutionContext,
        diagnostics: Vec<Diagnostic>,
        feed_budget: Option<&'budget mut FeedHtmlBudget>,
    ) -> Self {
        Self {
            nodes,
            input_len,
            source_uri,
            embedded_images,
            _decoded: decoded,
            context,
            diagnostics,
            blocks: Vec::new(),
            assets: Vec::new(),
            metadata: DocumentMetadata::default(),
            next_node: 0,
            base: None,
            inline_count: 0,
            max_table_rows: options.limits.max_table_rows,
            max_table_columns: options.limits.max_table_columns,
            max_table_cells: options.limits.max_table_cells,
            limits: options.limits.clone(),
            total_asset_bytes: 0,
            feed_budget,
        }
    }

    fn output_string(&mut self, value: &str) -> Result<String, ConversionError> {
        let mut output = if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.new_output_string(value.len())?
        } else {
            String::with_capacity(value.len())
        };
        output.push_str(value);
        Ok(output)
    }

    fn normalized_attr(
        &mut self,
        id: usize,
        name: &str,
    ) -> Result<Option<String>, ConversionError> {
        let Some(length) = self.attr(id, name).map(|value| normalized_len(value, self.context))
        else {
            return Ok(None);
        };
        let length = length?;
        let mut output = if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.new_output_string(length)?
        } else {
            String::with_capacity(length)
        };
        normalize_into(self.attr(id, name).unwrap_or_default(), &mut output, self.context)?;
        Ok(Some(output))
    }

    fn attr_output(&mut self, id: usize, name: &str) -> Result<String, ConversionError> {
        let length = self.attr(id, name).map_or(0, str::len);
        let mut output = if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.new_output_string(length)?
        } else {
            String::with_capacity(length)
        };
        output.push_str(self.attr(id, name).unwrap_or_default());
        Ok(output)
    }

    fn name_output(&mut self, id: usize) -> Result<Option<String>, ConversionError> {
        let Some(length) = self.name(id).map(str::len) else {
            return Ok(None);
        };
        let mut output = String::new();
        if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.reserve_string_capacity(&mut output, length)?;
        } else {
            output.try_reserve_exact(length).map_err(|error| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: format!("HTML tag-name allocation failed: {error}"),
            })?;
        }
        output.push_str(self.name(id).unwrap_or_default());
        Ok(Some(output))
    }

    fn normalized_node_text(&mut self, id: usize) -> Result<String, ConversionError> {
        let length = match &self.nodes[id].data {
            NodeData::Text(value) => normalized_len(value, self.context)?,
            _ => 0,
        };
        let mut output = if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.new_output_string(length)?
        } else {
            String::with_capacity(length)
        };
        if let NodeData::Text(value) = &self.nodes[id].data {
            normalize_into(value, &mut output, self.context)?;
        }
        Ok(output)
    }

    fn raw_visible_text_output(&mut self, id: usize) -> Result<String, ConversionError> {
        let length = self.raw_visible_text_len(id);
        let mut output = if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.new_output_string(length)?
        } else {
            String::with_capacity(length)
        };
        self.append_raw_visible_text(id, &mut output);
        Ok(output)
    }

    fn append_raw_visible_text(&self, id: usize, output: &mut String) {
        if self.node_context(id).excluded() {
            return;
        }
        match &self.nodes[id].data {
            NodeData::Text(value) => output.push_str(value),
            NodeData::Element { .. } | NodeData::Document => {
                for child in &self.nodes[id].children {
                    self.append_raw_visible_text(*child, output);
                }
            }
            NodeData::Other => {}
        }
    }

    fn reserve_vector<T>(
        &mut self,
        vector: &mut Vec<T>,
        additional: usize,
    ) -> Result<(), ConversionError> {
        if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.reserve_vec(vector, additional)?;
        }
        Ok(())
    }

    fn constructed(&self, kind: FeedHtmlObjectKind) {
        if self.feed_budget.is_some() {
            FeedHtmlBudget::constructed(kind);
        }
    }

    fn reserve_inline(&mut self) -> Result<(), ConversionError> {
        self.inline_count =
            self.inline_count.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "html_inlines",
                detail: "HTML inline count overflowed".into(),
            })?;
        if self.inline_count > MAX_DOCUMENT_INLINES {
            return Self::limit("html_inlines", "HTML produced too many inline nodes");
        }
        if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.inline()?;
        }
        Ok(())
    }

    fn reserve_structural(&mut self) -> Result<(), ConversionError> {
        if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.node()?;
        }
        Ok(())
    }

    fn push_warning(&mut self, code: &str, message: &str) -> Result<(), ConversionError> {
        let diagnostic = if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.html_diagnostic(code.len(), message.len())?;
            budget.reserve_vec(&mut self.diagnostics, 1)?;
            budgeted_warning(budget, code, message.len(), |output| output.write_str(message))?
        } else {
            warning(code, message.into())
        };
        self.diagnostics.push(diagnostic);
        Ok(())
    }

    fn extract(mut self) -> Result<ConverterOutput, ConversionError> {
        if self.feed_budget.is_none() {
            self.read_metadata();
        }
        self.base = self.valid_base()?;
        let root = self.choose_main()?;
        self.blocks = self.collect_child_blocks(root, 0)?;
        if self.blocks.is_empty() {
            return Err(ConversionError::Malformed {
                part: Some("html".into()),
                detail: "HTML contains no visible document content".into(),
            });
        }
        let document =
            Document { metadata: self.metadata, blocks: self.blocks, ..Document::default() };
        document.validate().map_err(|error| {
            let detail = format!("parsed IR invalid at {}: {}", error.path, error.detail);
            if error.code == IrErrorCode::ResourceLimit {
                ConversionError::ResourceLimit { limit: "html_ir", detail }
            } else {
                ConversionError::Malformed { part: Some("html".into()), detail }
            }
        })?;
        Ok(ConverterOutput::new(document, self.assets, self.diagnostics))
    }

    fn read_metadata(&mut self) {
        for id in 0..self.nodes.len() {
            let context = self.node_context(id);
            if context.excluded() || !context.in_head() || !self.is_html_element(id) {
                continue;
            }
            match self.name(id) {
                Some("title") if self.metadata.title.is_none() => {
                    self.metadata.title = nonempty(self.visible_text(id));
                }
                Some("meta") => {
                    let key = self.attr(id, "name").or_else(|| self.attr(id, "property"));
                    let value = self.attr(id, "content");
                    if let (Some(key), Some(value)) = (key, value) {
                        let key = key.to_ascii_lowercase();
                        if key == "author" {
                            self.metadata.authors.push(value.into());
                        } else if matches!(
                            key.as_str(),
                            "description" | "keywords" | "og:title" | "og:description"
                        ) {
                            self.metadata
                                .properties
                                .insert(format!("html.meta.{key}"), value.into());
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(html) = (0..self.nodes.len())
            .find(|id| self.is_html_element(*id) && self.name(*id) == Some("html"))
            && !self.node_context(html).excluded()
            && let Some(lang) = self.attr(html, "lang")
        {
            self.metadata.properties.insert("html.lang".into(), lang.into());
        }
    }

    fn valid_base(&mut self) -> Result<Option<Url>, ConversionError> {
        let source = self.source_uri.and_then(canonical_base_url);
        for id in 0..self.nodes.len() {
            let context = self.node_context(id);
            if self.is_html_element(id)
                && context.in_head()
                && !context.excluded()
                && self.name(id) == Some("base")
                && let Some(href) = self.attr(id, "href")
            {
                let parsed = Url::parse(href).ok().or_else(|| source.as_ref()?.join(href).ok());
                if let Some(url) = parsed.and_then(valid_http_base) {
                    return Ok(Some(url));
                }
                self.push_warning(
                    "html.baseRejected",
                    "base URL is not a canonical public HTTP(S) reference",
                )?;
            }
        }
        Ok(source)
    }

    fn choose_main(&mut self) -> Result<usize, ConversionError> {
        let body = (0..self.nodes.len()).find(|id| self.name(*id) == Some("body")).unwrap_or(0);
        let selected = (0..self.nodes.len())
            .filter(|id| {
                self.is_html_element(*id)
                    && (matches!(self.name(*id), Some("main" | "article"))
                        || self.attr(*id, "role").is_some_and(|v| v.eq_ignore_ascii_case("main")))
            })
            .filter(|id| !self.node_context(*id).excluded())
            .max_by_key(|id| (self.score(*id), usize::MAX - *id));
        if let Some(id) = selected.filter(|id| self.visible_text_len(*id) > 0) {
            return Ok(id);
        }
        self.push_warning(
            "html.mainContentFallback",
            "no non-empty explicit main-content region; used visible body content",
        )?;
        Ok(body)
    }

    // Fixed scoring constants are intentionally simple and covered by golden tests.
    fn score(&self, id: usize) -> i64 {
        let text = i64::try_from(self.visible_text_len(id)).unwrap_or(i64::MAX);
        let links = i64::try_from(self.link_text_len(id)).unwrap_or(i64::MAX);
        let paragraphs = i64::try_from(self.descendants_named(id, &["p"]) * 80).unwrap_or(i64::MAX);
        let headings = i64::try_from(self.descendants_named(id, &["h1", "h2", "h3"]) * 120)
            .unwrap_or(i64::MAX);
        text.saturating_add(paragraphs)
            .saturating_add(headings)
            .saturating_sub(links.saturating_mul(2))
    }

    fn collect_child_blocks(
        &mut self,
        id: usize,
        depth: usize,
    ) -> Result<Vec<BlockNode>, ConversionError> {
        self.context.checkpoint()?;
        if depth > usize::from(u16::MAX) {
            return Self::limit("html_nesting_depth", "semantic extraction depth overflowed");
        }
        let child_count = self.nodes.get(id).map_or(0, |node| node.children.len());
        let mut children = Vec::new();
        self.reserve_vector(&mut children, child_count)?;
        if let Some(node) = self.nodes.get(id) {
            children.extend_from_slice(&node.children);
        }
        let mut blocks = Vec::new();
        let mut inline = Vec::new();
        for child in children {
            if self.node_context(child).excluded() && !self.is_foreign_root(child) {
                continue;
            }
            if self.is_block_node(child) {
                self.flush_paragraph(&mut blocks, &mut inline)?;
                let built = self.build_block(child, depth.saturating_add(1))?;
                self.reserve_vector(&mut blocks, built.len())?;
                blocks.extend(built);
            } else {
                let built = self.inline_node(child, Vec::new())?;
                self.reserve_vector(&mut inline, built.len())?;
                inline.extend(built);
            }
        }
        self.flush_paragraph(&mut blocks, &mut inline)?;
        Ok(blocks)
    }

    fn flush_paragraph(
        &mut self,
        blocks: &mut Vec<BlockNode>,
        inline: &mut Vec<Inline>,
    ) -> Result<(), ConversionError> {
        if inline.is_empty() {
            return Ok(());
        }
        self.reserve_vector(blocks, 1)?;
        let node = self.make_node(Block::Paragraph(std::mem::take(inline)))?;
        blocks.push(node);
        Ok(())
    }

    fn build_block(&mut self, id: usize, depth: usize) -> Result<Vec<BlockNode>, ConversionError> {
        if self.node_context(id).excluded() && !self.is_foreign_root(id) {
            return Ok(Vec::new());
        }
        let Some(name) = self.name_output(id)? else {
            return Ok(Vec::new());
        };
        let block =
            match name.as_str() {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    let content = self.inline_children(id)?;
                    if content.is_empty() {
                        None
                    } else {
                        Some(self.make_node(Block::Heading {
                            level: name.as_bytes()[1] - b'0',
                            content,
                        })?)
                    }
                }
                "p" | "address" | "figcaption" => {
                    let content = self.inline_children(id)?;
                    if content.is_empty() {
                        None
                    } else {
                        Some(self.make_node(Block::Paragraph(content))?)
                    }
                }
                "div" | "section" | "article" | "main" | "body" | "html" => {
                    return self.collect_child_blocks(id, depth);
                }
                "ul" | "ol" => match self.build_list(id, name == "ol", depth)? {
                    Some(value) => Some(self.make_node(value)?),
                    None => None,
                },
                "table" => match self.build_table(id, depth)? {
                    Some(value) => Some(self.make_node(value)?),
                    None => None,
                },
                "pre" => {
                    let code = self.first_visible_descendant(id, "code");
                    let text_bytes = self.raw_visible_text_len(id);
                    if text_bytes == 0 {
                        None
                    } else {
                        let text = self.raw_visible_text_output(id)?;
                        let language =
                            code.map(|code| self.code_language_output(code)).transpose()?.flatten();
                        Some(self.make_node(Block::Code { language, text })?)
                    }
                }
                "img" => match self.build_image(id)? {
                    Some(value) => Some(self.make_node(value)?),
                    None => None,
                },
                "hr" => Some(self.make_node(Block::Rule)?),
                "svg" | "math" => {
                    let message = if name == "svg" {
                        "svg content was not traversed as HTML resources"
                    } else {
                        "math content was not traversed as HTML resources"
                    };
                    self.push_warning("html.activeForeignContentOmitted", message)?;
                    None
                }
                "li" | "tr" | "td" | "th" | "code" | "head" => None,
                _ => return self.collect_child_blocks(id, depth),
            };
        let mut output = Vec::new();
        if let Some(block) = block {
            self.reserve_vector(&mut output, 1)?;
            output.push(block);
        }
        Ok(output)
    }

    fn build_list(
        &mut self,
        id: usize,
        ordered: bool,
        depth: usize,
    ) -> Result<Option<Block>, ConversionError> {
        let mut items = Vec::new();
        let mut children = Vec::new();
        self.reserve_vector(&mut children, self.nodes[id].children.len())?;
        children.extend_from_slice(&self.nodes[id].children);
        for child in children {
            if !self.is_html_element(child)
                || self.name(child) != Some("li")
                || self.node_context(child).excluded()
            {
                continue;
            }
            let blocks = self.collect_child_blocks(child, depth.saturating_add(1))?;
            if !blocks.is_empty() {
                self.reserve_structural()?;
                self.reserve_vector(&mut items, 1)?;
                self.constructed(FeedHtmlObjectKind::Node);
                items.push(ListItem { checked: None, marker_label: None, blocks });
            }
        }
        if items.is_empty() {
            return Ok(None);
        }
        let start = self.attr(id, "start").and_then(|value| value.parse().ok()).unwrap_or(1);
        Ok(Some(Block::List {
            kind: if ordered { ListKind::Ordered } else { ListKind::Bullet },
            start,
            items,
        }))
    }

    fn build_table(&mut self, id: usize, depth: usize) -> Result<Option<Block>, ConversionError> {
        let source_rows = self.direct_table_rows(id)?;
        let row_count = source_rows.len();
        if u64::try_from(row_count).unwrap_or(u64::MAX) > self.max_table_rows {
            return Self::limit("max_table_rows", "HTML table has too many rows");
        }
        if source_rows.is_empty() {
            return Ok(None);
        }
        let planned = self.plan_table(&source_rows)?;
        if planned.width == 0 {
            return Ok(None);
        }
        let logical_cells = u64::try_from(planned.width)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::try_from(row_count).unwrap_or(u64::MAX));
        if logical_cells > self.max_table_cells {
            return Self::limit("max_table_cells", "HTML table has too many logical cells");
        }
        let rows = self.render_table(planned, depth)?;
        Ok(Some(Block::Table { rows, alignments: Vec::<TableAlignment>::new() }))
    }

    fn plan_table(
        &mut self,
        source_rows: &[SourceTableRow],
    ) -> Result<PlannedTable, ConversionError> {
        let mut occupancy = Vec::<u32>::new();
        let mut planned = Vec::<Vec<PlannedTableCell>>::new();
        self.reserve_vector(&mut planned, source_rows.len())?;
        let mut width = 0_usize;
        for (row_index, source_row) in source_rows.iter().copied().enumerate() {
            let mut row_cells = Vec::new();
            let group_rows = source_rows[row_index..]
                .iter()
                .take_while(|row| row.group == source_row.group)
                .count();
            let remaining_rows = u32::try_from(group_rows).unwrap_or(u32::MAX).max(1);
            for cell in self.direct_table_cells(source_row.node)? {
                let requested_row_span = table_span(self.attr(cell, "rowspan"));
                let requested_row_span =
                    if requested_row_span == 0 { remaining_rows } else { requested_row_span };
                let row_span = requested_row_span.min(remaining_rows);
                if row_span != requested_row_span {
                    self.push_warning(
                        "html.tableRowspanClamped",
                        "rowspan extending beyond its row group was clamped",
                    )?;
                }
                let requested_column_span = table_span(self.attr(cell, "colspan"));
                let column_span = if requested_column_span == 0 {
                    self.push_warning(
                        "html.tableColspanNormalized",
                        "zero colspan was normalized to one column",
                    )?;
                    1
                } else {
                    requested_column_span
                };
                let span =
                    usize::try_from(column_span).map_err(|_| ConversionError::ResourceLimit {
                        limit: "max_table_columns",
                        detail: "HTML column span cannot be represented".into(),
                    })?;
                let mut column = 0_usize;
                loop {
                    while occupancy.get(column).is_some_and(|remaining| *remaining > 0) {
                        column += 1;
                    }
                    let end =
                        column.checked_add(span).ok_or_else(|| ConversionError::ResourceLimit {
                            limit: "max_table_columns",
                            detail: "HTML table width overflowed".into(),
                        })?;
                    if u64::try_from(end).unwrap_or(u64::MAX)
                        > self.max_table_columns.min(MAX_TABLE_COLUMNS as u64)
                    {
                        return Self::limit("max_table_columns", "HTML table is too wide");
                    }
                    if occupancy
                        .get(column..end)
                        .is_some_and(|slots| slots.iter().any(|remaining| *remaining > 0))
                    {
                        column += 1;
                        continue;
                    }
                    if occupancy.len() < end {
                        let additional = end - occupancy.len();
                        self.reserve_vector(&mut occupancy, additional)?;
                        occupancy.resize(end, 0);
                    }
                    occupancy[column..end].fill(row_span);
                    width = width.max(end);
                    self.reserve_vector(&mut row_cells, 1)?;
                    row_cells.push(PlannedTableCell { node: cell, column, row_span, column_span });
                    break;
                }
            }
            planned.push(row_cells);
            for remaining in &mut occupancy {
                *remaining = remaining.saturating_sub(1);
            }
        }
        Ok(PlannedTable { rows: planned, width })
    }

    fn render_table(
        &mut self,
        planned: PlannedTable,
        depth: usize,
    ) -> Result<Vec<TableRow>, ConversionError> {
        let mut rows = Vec::new();
        self.reserve_vector(&mut rows, planned.rows.len())?;
        let mut active = Vec::new();
        self.reserve_vector(&mut active, planned.width)?;
        active.resize(planned.width, 0_u32);
        for row_cells in planned.rows {
            let mut cells = Vec::new();
            let mut planned_index = 0_usize;
            let mut column = 0_usize;
            while column < planned.width {
                if active[column] > 0 {
                    column += 1;
                    continue;
                }
                if row_cells.get(planned_index).is_some_and(|cell| cell.column == column) {
                    let cell = &row_cells[planned_index];
                    let span = usize::try_from(cell.column_span).unwrap_or(planned.width);
                    let end = column.saturating_add(span).min(planned.width);
                    active[column..end].fill(cell.row_span);
                    let blocks = self.collect_child_blocks(cell.node, depth.saturating_add(1))?;
                    self.reserve_structural()?;
                    self.reserve_vector(&mut cells, 1)?;
                    self.constructed(FeedHtmlObjectKind::Node);
                    cells.push(Cell {
                        row_span: cell.row_span,
                        column_span: cell.column_span,
                        header: self.name(cell.node) == Some("th"),
                        blocks,
                    });
                    planned_index += 1;
                    column = end;
                } else {
                    active[column] = 1;
                    self.reserve_structural()?;
                    self.reserve_vector(&mut cells, 1)?;
                    self.constructed(FeedHtmlObjectKind::Node);
                    cells.push(Cell {
                        row_span: 1,
                        column_span: 1,
                        header: false,
                        blocks: Vec::new(),
                    });
                    column += 1;
                }
            }
            self.reserve_structural()?;
            self.constructed(FeedHtmlObjectKind::Node);
            rows.push(TableRow { cells });
            for remaining in &mut active {
                *remaining = remaining.saturating_sub(1);
            }
        }
        Ok(rows)
    }

    fn build_image(&mut self, id: usize) -> Result<Option<Block>, ConversionError> {
        let alt = self.normalized_attr(id, "alt")?.and_then(nonempty);
        let Some(src) = self.attr(id, "src") else { return Ok(None) };
        if let Some(cid) = src.strip_prefix("cid:") {
            let asset = self
                .embedded_images
                .iter()
                .find(|image| image.cid == cid)
                .map(|image| image.asset.clone());
            if let Some(asset) = asset {
                return Ok(Some(Block::Image { asset, alt }));
            }
            self.push_warning(
                "html.cidImageRejected",
                "CID image was not an exact canonical audited attachment reference",
            )?;
            return match alt {
                Some(value) => {
                    self.reserve_inline()?;
                    let mut content = Vec::new();
                    self.reserve_vector(&mut content, 1)?;
                    self.constructed(FeedHtmlObjectKind::Inline);
                    content.push(Inline::Text { value, marks: Vec::new() });
                    Ok(Some(Block::Paragraph(content)))
                }
                None => Ok(None),
            };
        }
        if src.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:")) {
            let data_uri = src.to_owned();
            return self.build_data_image(&data_uri, alt);
        }
        let resolved = {
            Url::parse(src).ok().or_else(|| self.base.as_ref().and_then(|base| base.join(src).ok()))
        };
        let Some(resolved) = resolved else {
            self.push_warning(
                "html.imageUriRejected",
                "image URI was retained only as alternative text; no network access occurred",
            )?;
            return match alt {
                Some(value) => {
                    self.reserve_inline()?;
                    let mut content = Vec::new();
                    self.reserve_vector(&mut content, 1)?;
                    self.constructed(FeedHtmlObjectKind::Inline);
                    content.push(Inline::Text { value, marks: Vec::new() });
                    Ok(Some(Block::Paragraph(content)))
                }
                None => Ok(None),
            };
        };
        let uri = self.output_string(resolved.as_str())?;
        if canonical_external_asset_uri(&uri).as_deref() != Some(uri.as_ref()) {
            self.push_warning(
                "html.imageUriRejected",
                "image URI was retained only as alternative text; no network access occurred",
            )?;
            return match alt {
                Some(value) => {
                    self.reserve_inline()?;
                    let mut content = Vec::new();
                    self.reserve_vector(&mut content, 1)?;
                    self.constructed(FeedHtmlObjectKind::Inline);
                    content.push(Inline::Text { value, marks: Vec::new() });
                    Ok(Some(Block::Paragraph(content)))
                }
                None => Ok(None),
            };
        }
        if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.asset()?;
        }
        let mut asset_id_value = if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.new_output_string(26)?
        } else {
            String::with_capacity(26)
        };
        write!(&mut asset_id_value, "html-external-image-{:06}", self.assets.len() + 1).map_err(
            |_| ConversionError::Internal {
                detail: "failed to format HTML asset identifier".into(),
            },
        )?;
        let asset_id = AssetId(asset_id_value);
        let media_type = self.output_string(image_media_type(&uri))?;
        // The image block owns a second AssetId string in addition to the
        // asset-registry key.
        let block_asset_id = AssetId(self.output_string(&asset_id.0)?);
        if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.reserve_vec(&mut self.assets, 1)?;
            FeedHtmlBudget::constructed(FeedHtmlObjectKind::Asset);
        }
        self.assets.push(Asset {
            id: asset_id,
            filename: None,
            media_type,
            bytes: Vec::new(),
            external_uri: Some(uri),
        });
        Ok(Some(Block::Image { asset: block_asset_id, alt }))
    }

    #[allow(clippy::too_many_lines)] // Canonical decoding and complete raster audit are one boundary.
    fn build_data_image(
        &mut self,
        src: &str,
        alt: Option<String>,
    ) -> Result<Option<Block>, ConversionError> {
        // Feed conversion owns a separate aggregate-memory model. Keeping data
        // images out of nested feed HTML avoids charging their bytes twice.
        if self.feed_budget.is_some() {
            self.push_warning(
                "html.imageUriRejected",
                "data image was retained only as alternative text in nested feed HTML",
            )?;
            return self.image_alt_fallback(alt);
        }
        let Some((header, payload)) = src.split_once(',') else {
            self.push_warning("html.dataImageRejected", "data image URI has no payload")?;
            return self.image_alt_fallback(alt);
        };
        let media_type = header
            .get(5..)
            .and_then(|value| value.strip_suffix(";base64"))
            .map(str::to_ascii_lowercase);
        let Some(media_type) = media_type.filter(|value| {
            matches!(
                value.as_str(),
                "image/png"
                    | "image/jpeg"
                    | "image/gif"
                    | "image/webp"
                    | "image/bmp"
                    | "image/tiff"
            )
        }) else {
            self.push_warning(
                "html.dataImageRejected",
                "data image must use an explicitly supported raster media type and base64 encoding",
            )?;
            return self.image_alt_fallback(alt);
        };
        let canonical_padding = if payload.len() % 4 == 0 {
            if payload.ends_with("==") {
                2
            } else if payload.ends_with('=') {
                1
            } else {
                0
            }
        } else {
            0
        };
        let estimated = payload.len().saturating_add(3) / 4 * 3 - canonical_padding;
        if u64::try_from(estimated).unwrap_or(u64::MAX) > self.limits.max_asset_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: "HTML data image exceeds the per-asset byte budget".into(),
            });
        }
        let bytes = match base64::engine::general_purpose::STANDARD.decode(payload) {
            Ok(bytes) if base64::engine::general_purpose::STANDARD.encode(&bytes) == payload => {
                bytes
            }
            _ => {
                self.push_warning(
                    "html.dataImageRejected",
                    "data image payload is not canonical base64",
                )?;
                return self.image_alt_fallback(alt);
            }
        };
        let byte_count = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if byte_count > self.limits.max_asset_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: "HTML data image exceeds the per-asset byte budget".into(),
            });
        }
        let format = crate::image_converter::format::detect(&bytes, self.context)?;
        let declared_matches = if media_type == "image/gif" {
            bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")
        } else {
            format.is_some_and(|format| format.media_type() == media_type)
        };
        if !declared_matches {
            self.push_warning(
                "html.dataImageRejected",
                "data image bytes do not match the declared raster media type",
            )?;
            return self.image_alt_fallback(alt);
        }
        let extension = if media_type == "image/gif" {
            crate::epub::image::validate(
                &bytes,
                &media_type,
                "HTML data image",
                &self.limits,
                self.context,
            )?;
            "gif"
        } else {
            let format = format.expect("checked above");
            crate::image_converter::envelope::validate(format, &bytes, &self.limits, self.context)?;
            format.extension()
        };
        if let Some(asset) = self.assets.iter().find(|asset| {
            asset.external_uri.is_none() && asset.media_type == media_type && asset.bytes == bytes
        }) {
            return Ok(Some(Block::Image { asset: asset.id.clone(), alt }));
        }
        self.total_asset_bytes =
            self.total_asset_bytes.checked_add(byte_count).ok_or_else(|| {
                ConversionError::ResourceLimit {
                    limit: "max_total_asset_bytes",
                    detail: "HTML data image byte total overflowed".into(),
                }
            })?;
        if self.total_asset_bytes > self.limits.max_total_asset_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_total_asset_bytes",
                detail: "HTML data images exceed the aggregate asset byte budget".into(),
            });
        }
        let asset = AssetId(format!("html-data-image-{:06}", self.assets.len() + 1));
        self.assets.push(Asset {
            id: asset.clone(),
            filename: Some(format!("embedded.{extension}")),
            media_type,
            bytes,
            external_uri: None,
        });
        Ok(Some(Block::Image { asset, alt }))
    }

    fn image_alt_fallback(
        &mut self,
        alt: Option<String>,
    ) -> Result<Option<Block>, ConversionError> {
        match alt {
            Some(value) => {
                self.reserve_inline()?;
                Ok(Some(Block::Paragraph(vec![Inline::Text { value, marks: Vec::new() }])))
            }
            None => Ok(None),
        }
    }

    fn inline_children(&mut self, id: usize) -> Result<Vec<Inline>, ConversionError> {
        let mut output = Vec::new();
        let mut children = Vec::new();
        self.reserve_vector(&mut children, self.nodes[id].children.len())?;
        children.extend_from_slice(&self.nodes[id].children);
        for child in children {
            if !self.is_block_node(child) {
                let built = self.inline_node(child, Vec::new())?;
                self.reserve_vector(&mut output, built.len())?;
                output.extend(built);
            }
        }
        Ok(output)
    }

    #[allow(clippy::too_many_lines)] // Allocation boundaries stay next to every inline constructor.
    fn inline_node(
        &mut self,
        id: usize,
        mut marks: Vec<InlineMark>,
    ) -> Result<Vec<Inline>, ConversionError> {
        let mut output = Vec::new();
        if self.node_context(id).excluded() {
            return Ok(output);
        }
        if let NodeData::Text(value) = &self.nodes[id].data {
            if value.chars().all(char::is_whitespace) {
                return Ok(output);
            }
            self.reserve_inline()?;
            let value = self.normalized_node_text(id)?;
            if !value.is_empty() {
                self.reserve_vector(&mut output, 1)?;
                self.constructed(FeedHtmlObjectKind::Inline);
                output.push(Inline::Text { value, marks });
            }
            return Ok(output);
        }
        if let Some(name) = self.name(id) {
            if name == "a" {
                let content = self.inline_children_with_marks(id, &marks)?;
                let Some(href) = self.attr(id, "href") else {
                    return Ok(content);
                };
                let href_bytes = href.len();
                let resolved =
                    Url::parse(href).ok().or_else(|| self.base.as_ref()?.join(href).ok());
                let target_bytes =
                    resolved.as_ref().map_or_else(|| href_bytes, |url| url.as_str().len());
                let target = if let Some(resolved) = resolved.as_ref() {
                    self.output_string(resolved.as_str())?
                } else {
                    self.attr_output(id, "href")?
                };
                debug_assert_eq!(target.len(), target_bytes);
                if safe_link_target(&target) && !content.is_empty() {
                    self.reserve_inline()?;
                    self.reserve_vector(&mut output, 1)?;
                    self.constructed(FeedHtmlObjectKind::Inline);
                    output.push(Inline::Link { target, content });
                    return Ok(output);
                }
                if !content.is_empty() {
                    self.push_warning(
                        "html.linkUriRejected",
                        "unsafe link destination was omitted",
                    )?;
                }
                return Ok(content);
            }
            match name {
                "strong" | "b" => {
                    self.reserve_vector(&mut marks, 1)?;
                    marks.push(InlineMark::Bold);
                }
                "em" | "i" => {
                    self.reserve_vector(&mut marks, 1)?;
                    marks.push(InlineMark::Italic);
                }
                "del" | "s" | "strike" => {
                    self.reserve_vector(&mut marks, 1)?;
                    marks.push(InlineMark::Strikethrough);
                }
                "u" => {
                    self.reserve_vector(&mut marks, 1)?;
                    marks.push(InlineMark::Underline);
                }
                "sup" => {
                    self.reserve_vector(&mut marks, 1)?;
                    marks.push(InlineMark::Superscript);
                }
                "sub" => {
                    self.reserve_vector(&mut marks, 1)?;
                    marks.push(InlineMark::Subscript);
                }
                "br" => {
                    self.reserve_inline()?;
                    self.reserve_vector(&mut output, 1)?;
                    self.constructed(FeedHtmlObjectKind::Inline);
                    output.push(Inline::LineBreak);
                    return Ok(output);
                }
                "code" => {
                    let bound = self.visible_text_len(id);
                    if bound == 0 {
                        return Ok(Vec::new());
                    }
                    let Some(value) = nonempty(self.raw_visible_text_output(id)?) else {
                        return Ok(Vec::new());
                    };
                    self.reserve_inline()?;
                    self.reserve_vector(&mut output, 1)?;
                    self.constructed(FeedHtmlObjectKind::Inline);
                    output.push(Inline::Code(value));
                    return Ok(output);
                }
                "svg" | "math" | "script" | "style" | "template" | "noscript" | "img" => {
                    return Ok(output);
                }
                _ => {}
            }
        }
        let mut children = Vec::new();
        self.reserve_vector(&mut children, self.nodes[id].children.len())?;
        children.extend_from_slice(&self.nodes[id].children);
        for child in children {
            let mut child_marks = Vec::new();
            self.reserve_vector(&mut child_marks, marks.len())?;
            child_marks.extend_from_slice(&marks);
            let built = self.inline_node(child, child_marks)?;
            self.reserve_vector(&mut output, built.len())?;
            output.extend(built);
        }
        Ok(output)
    }

    fn inline_children_with_marks(
        &mut self,
        id: usize,
        marks: &[InlineMark],
    ) -> Result<Vec<Inline>, ConversionError> {
        let mut output = Vec::new();
        let mut children = Vec::new();
        self.reserve_vector(&mut children, self.nodes[id].children.len())?;
        children.extend_from_slice(&self.nodes[id].children);
        for child in children {
            let mut child_marks = Vec::new();
            self.reserve_vector(&mut child_marks, marks.len())?;
            child_marks.extend_from_slice(marks);
            let built = self.inline_node(child, child_marks)?;
            self.reserve_vector(&mut output, built.len())?;
            output.extend(built);
        }
        Ok(output)
    }

    fn make_node(&mut self, block: Block) -> Result<BlockNode, ConversionError> {
        self.reserve_structural()?;
        self.next_node += 1;
        let mut id = if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.new_output_string(11)?
        } else {
            String::with_capacity(11)
        };
        write!(&mut id, "html-{:06}", self.next_node).map_err(|_| ConversionError::Internal {
            detail: "failed to format HTML node identifier".into(),
        })?;
        let provider = self.output_string(PROVIDER_ID)?;
        self.constructed(FeedHtmlObjectKind::Node);
        Ok(BlockNode {
            id: NodeId(id),
            block,
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider,
                locator: SourceLocator {
                    byte_start: Some(0),
                    byte_end: u64::try_from(self.input_len).ok(),
                    ..SourceLocator::default()
                },
                confidence: None,
            },
        })
    }
    fn name(&self, id: usize) -> Option<&str> {
        match &self.nodes.get(id)?.data {
            NodeData::Element { name, .. } => Some(name.local.as_ref()),
            _ => None,
        }
    }
    fn attr(&self, id: usize, name: &str) -> Option<&str> {
        match &self.nodes.get(id)?.data {
            NodeData::Element { attrs, .. } => attrs
                .iter()
                .find(|a| a.name.local.as_ref().eq_ignore_ascii_case(name))
                .map(|a| a.value.as_ref()),
            _ => None,
        }
    }
    fn is_html_element(&self, id: usize) -> bool {
        match &self.nodes.get(id).map(|node| &node.data) {
            Some(NodeData::Element { name, .. }) => name.ns == html5ever::ns!(html),
            _ => false,
        }
    }
    fn node_context(&self, id: usize) -> NodeContext {
        let mut context = NodeContext::default();
        let mut current = Some(id);
        while let Some(node) = current.and_then(|node| self.nodes.get(node)) {
            if let NodeData::Element { name, .. } = &node.data {
                if name.ns == html5ever::ns!(html) {
                    let local = name.local.as_ref();
                    context.mark(NodeContext::HEAD, local == "head");
                    context.mark(NodeContext::TEMPLATE, local == "template");
                    context.mark(NodeContext::HIDDEN, Self::node_hidden(node));
                    context.mark(NodeContext::BOILERPLATE, Self::node_boilerplate(node));
                } else {
                    context.mark(NodeContext::FOREIGN, true);
                }
            }
            current = node.parent;
        }
        context
    }
    fn node_hidden(node: &DomNode) -> bool {
        let NodeData::Element { name, attrs, .. } = &node.data else { return false };
        matches!(
            name.local.as_ref(),
            "script"
                | "style"
                | "template"
                | "noscript"
                | "iframe"
                | "object"
                | "embed"
                | "canvas"
                | "input"
                | "select"
                | "textarea"
                | "button"
        ) || attrs.iter().any(|attr| matches!(attr.name.local.as_ref(), "hidden" | "inert"))
            || attrs.iter().any(|attr| {
                attr.name.local.as_ref() == "aria-hidden" && attr.value.eq_ignore_ascii_case("true")
            })
    }
    fn node_boilerplate(node: &DomNode) -> bool {
        let NodeData::Element { name, attrs, .. } = &node.data else { return false };
        if matches!(name.local.as_ref(), "nav" | "aside" | "footer") {
            return true;
        }
        if attrs.iter().any(|attr| {
            attr.name.local.as_ref() == "role"
                && ["navigation", "banner", "contentinfo", "complementary", "dialog"]
                    .iter()
                    .any(|role| attr.value.eq_ignore_ascii_case(role))
        }) {
            return true;
        }
        attrs.iter().filter(|attr| matches!(attr.name.local.as_ref(), "id" | "class")).any(|attr| {
            attr.value
                .split(|character: char| {
                    character.is_ascii_whitespace() || matches!(character, '-' | '_' | ':' | '.')
                })
                .filter(|token| !token.is_empty())
                .any(is_boilerplate_token)
        })
    }
    fn visible_text(&self, id: usize) -> String {
        normalize(&self.raw_visible_text(id))
    }
    fn raw_visible_text(&self, id: usize) -> String {
        if self.node_context(id).excluded() {
            return String::new();
        }
        let mut out = String::with_capacity(self.raw_visible_text_len(id));
        self.append_raw_visible_text(id, &mut out);
        out
    }
    fn raw_visible_text_len(&self, id: usize) -> usize {
        if self.node_context(id).excluded() {
            return 0;
        }
        match &self.nodes[id].data {
            NodeData::Text(value) => value.len(),
            NodeData::Element { .. } | NodeData::Document => self.nodes[id]
                .children
                .iter()
                .map(|child| self.raw_visible_text_len(*child))
                .fold(0_usize, usize::saturating_add),
            NodeData::Other => 0,
        }
    }
    fn visible_text_len(&self, id: usize) -> usize {
        self.raw_visible_text_len(id)
    }
    fn link_text_len(&self, id: usize) -> usize {
        self.nodes[id]
            .children
            .iter()
            .copied()
            .map(|child| {
                let here = if self.is_html_element(child)
                    && self.name(child) == Some("a")
                    && !self.node_context(child).excluded()
                {
                    self.raw_visible_text_len(child)
                } else {
                    0
                };
                here.saturating_add(self.link_text_len(child))
            })
            .fold(0_usize, usize::saturating_add)
    }
    fn descendants_named(&self, id: usize, names: &[&str]) -> usize {
        self.nodes[id]
            .children
            .iter()
            .copied()
            .map(|child| {
                usize::from(
                    self.is_html_element(child)
                        && !self.node_context(child).excluded()
                        && self.name(child).is_some_and(|name| names.contains(&name)),
                )
                .saturating_add(self.descendants_named(child, names))
            })
            .sum()
    }
    fn first_visible_descendant(&self, id: usize, name: &str) -> Option<usize> {
        for child in self.nodes[id].children.iter().copied() {
            if self.is_html_element(child)
                && self.name(child) == Some(name)
                && !self.node_context(child).excluded()
            {
                return Some(child);
            }
            if let Some(found) = self.first_visible_descendant(child, name) {
                return Some(found);
            }
        }
        None
    }
    fn is_block_node(&self, id: usize) -> bool {
        if !self.is_html_element(id) {
            return matches!(self.name(id), Some("svg" | "math"));
        }
        matches!(
            self.name(id),
            Some(
                "address"
                    | "article"
                    | "body"
                    | "div"
                    | "figcaption"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "head"
                    | "hr"
                    | "html"
                    | "img"
                    | "main"
                    | "ol"
                    | "p"
                    | "pre"
                    | "section"
                    | "table"
                    | "ul"
            )
        )
    }
    fn is_foreign_root(&self, id: usize) -> bool {
        if self.is_html_element(id) || !matches!(self.name(id), Some("svg" | "math")) {
            return false;
        }
        self.nodes[id].parent.is_some_and(|parent| !self.node_context(parent).excluded())
    }
    fn direct_table_rows(&mut self, table: usize) -> Result<Vec<SourceTableRow>, ConversionError> {
        let mut rows = Vec::new();
        for child in self.nodes[table].children.iter().copied() {
            if !self.is_html_element(child) || self.node_context(child).excluded() {
                continue;
            }
            if self.name(child) == Some("tr") {
                self.reserve_vector(&mut rows, 1)?;
                rows.push(SourceTableRow { node: child, group: table });
            } else if matches!(self.name(child), Some("thead" | "tbody" | "tfoot")) {
                for row in self.nodes[child].children.iter().copied() {
                    if self.is_html_element(row)
                        && self.name(row) == Some("tr")
                        && !self.node_context(row).excluded()
                    {
                        self.reserve_vector(&mut rows, 1)?;
                        rows.push(SourceTableRow { node: row, group: child });
                    }
                }
            }
        }
        Ok(rows)
    }
    fn direct_table_cells(&mut self, row: usize) -> Result<Vec<usize>, ConversionError> {
        let mut cells = Vec::new();
        for cell in self.nodes[row].children.iter().copied() {
            if self.is_html_element(cell)
                && matches!(self.name(cell), Some("td" | "th"))
                && !self.node_context(cell).excluded()
            {
                self.reserve_vector(&mut cells, 1)?;
                cells.push(cell);
            }
        }
        Ok(cells)
    }
    fn code_language_part(&self, id: usize) -> Option<&str> {
        self.attr(id, "class").and_then(|v| {
            v.split_ascii_whitespace().find_map(|part| part.strip_prefix("language-"))
        })
    }
    fn code_language_output(&mut self, id: usize) -> Result<Option<String>, ConversionError> {
        let Some(length) = self.code_language_part(id).map(str::len) else {
            return Ok(None);
        };
        let mut output = if let Some(budget) = self.feed_budget.as_deref_mut() {
            budget.new_output_string(length)?
        } else {
            String::with_capacity(length)
        };
        output.push_str(self.code_language_part(id).unwrap_or_default());
        Ok(Some(output))
    }
    fn limit<T>(limit: &'static str, detail: &str) -> Result<T, ConversionError> {
        Err(ConversionError::ResourceLimit { limit, detail: detail.into() })
    }
}

fn warning(code: &str, message: String) -> Diagnostic {
    Diagnostic { code: code.into(), severity: DiagnosticSeverity::Warning, message, locator: None }
}

fn budgeted_warning<F>(
    budget: &mut FeedHtmlBudget,
    code: &str,
    message_len: usize,
    fill_message: F,
) -> Result<Diagnostic, ConversionError>
where
    F: FnOnce(&mut String) -> std::fmt::Result,
{
    let memory_mark = budget.memory.mark();
    let persistent_mark = budget.persistent_memory_bytes;
    let mut owned_code = budget.new_precounted_string(code.len())?;
    owned_code.push_str(code);
    let mut message = match budget.new_precounted_string(message_len) {
        Ok(message) => message,
        Err(error) => {
            budget.memory.rewind(memory_mark)?;
            budget.persistent_memory_bytes = persistent_mark;
            return Err(error);
        }
    };
    if fill_message(&mut message).is_err() {
        budget.memory.rewind(memory_mark)?;
        budget.persistent_memory_bytes = persistent_mark;
        return Err(ConversionError::Internal {
            detail: "failed to construct budgeted HTML diagnostic".into(),
        });
    }
    FeedHtmlBudget::constructed(FeedHtmlObjectKind::Diagnostic);
    Ok(Diagnostic {
        code: owned_code,
        severity: DiagnosticSeverity::Warning,
        message,
        locator: None,
    })
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}
fn normalize(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    normalize_into_unchecked(value, &mut output);
    output
}

fn normalized_len(value: &str, context: &ExecutionContext) -> Result<usize, ConversionError> {
    let mut bytes = 0_usize;
    let mut pending_space = false;
    for (index, character) in value.chars().enumerate() {
        if index.is_multiple_of(CHECKPOINT_EVENTS) {
            context.checkpoint()?;
        }
        if character.is_whitespace() {
            pending_space = bytes != 0;
        } else {
            if pending_space {
                bytes = bytes.saturating_add(1);
                pending_space = false;
            }
            bytes = bytes.saturating_add(character.len_utf8());
        }
    }
    Ok(bytes)
}

fn normalize_into(
    value: &str,
    output: &mut String,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut pending_space = false;
    for (index, character) in value.chars().enumerate() {
        if index.is_multiple_of(CHECKPOINT_EVENTS) {
            context.checkpoint()?;
        }
        if character.is_whitespace() {
            pending_space = !output.is_empty();
        } else {
            if pending_space {
                output.push(' ');
                pending_space = false;
            }
            output.push(character);
        }
    }
    Ok(())
}

fn normalize_into_unchecked(value: &str, output: &mut String) {
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_whitespace() {
            pending_space = !output.is_empty();
        } else {
            if pending_space {
                output.push(' ');
                pending_space = false;
            }
            output.push(character);
        }
    }
}
fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}
fn table_span(value: Option<&str>) -> u32 {
    value.and_then(|v| v.parse::<u32>().ok()).unwrap_or(1)
}
fn is_boilerplate_token(token: &str) -> bool {
    [
        "ad",
        "ads",
        "advert",
        "advertisement",
        "advertising",
        "cookie",
        "modal",
        "navigation",
        "popup",
        "recommend",
        "recommended",
        "related",
        "sidebar",
    ]
    .iter()
    .any(|candidate| token.eq_ignore_ascii_case(candidate))
}
pub(crate) fn valid_http_base(mut url: Url) -> Option<Url> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || denied_base_host(url.host_str()?)
    {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    Some(url)
}
fn denied_base_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return true;
    }
    let Ok(address) = host.parse::<std::net::IpAddr>() else { return false };
    match address {
        std::net::IpAddr::V4(value) => {
            value.is_private()
                || value.is_loopback()
                || value.is_link_local()
                || value.is_unspecified()
                || value.is_multicast()
        }
        std::net::IpAddr::V6(value) => {
            value.is_loopback()
                || value.is_unspecified()
                || value.is_multicast()
                || value.is_unique_local()
                || value.is_unicast_link_local()
        }
    }
}
pub(crate) fn canonical_base_url(value: &str) -> Option<Url> {
    valid_http_base(Url::parse(value).ok()?)
}
fn image_media_type(uri: &str) -> &'static str {
    let extension = Url::parse(uri)
        .ok()
        .and_then(|url| std::path::Path::new(url.path()).extension()?.to_str().map(str::to_owned));
    if extension.as_deref().is_some_and(|value| value.eq_ignore_ascii_case("png")) {
        "image/png"
    } else if extension.as_deref().is_some_and(|value| value.eq_ignore_ascii_case("gif")) {
        "image/gif"
    } else if extension.as_deref().is_some_and(|value| value.eq_ignore_ascii_case("webp")) {
        "image/webp"
    } else if extension.as_deref().is_some_and(|value| value.eq_ignore_ascii_case("svg")) {
        "image/svg+xml"
    } else {
        "image/jpeg"
    }
}
pub(crate) fn safe_link_target(value: &str) -> bool {
    if value.chars().any(char::is_control) || value.contains('&') {
        return false;
    }
    let Some(colon) = value.find(':') else { return true };
    let scheme = &value[..colon];
    scheme.bytes().enumerate().all(|(index, byte)| {
        byte.is_ascii_alphabetic() || index > 0 && matches!(byte, b'+' | b'-' | b'.')
    }) && !matches!(
        scheme.to_ascii_lowercase().as_str(),
        "javascript" | "vbscript" | "data" | "file" | "blob"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Frame, RgbaImage, codecs::gif::GifEncoder};
    use into_markdown_core::{ExecutionOptions, ResourceLimits, SourceMetadata};
    use std::sync::Arc;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }
    fn convert(source: &str) -> ConverterOutput {
        convert_html(
            &ResolvedInput {
                bytes: Arc::from(source.as_bytes()),
                metadata: SourceMetadata::default(),
            },
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap()
    }

    #[test]
    fn feed_capacity_budget_precedes_first_vector_growth_and_is_not_double_charged() {
        let context = context();
        let options = ConversionOptions::default();
        let mut budget = FeedHtmlBudget::new(
            options.limits.max_feed_text_bytes,
            16,
            options.limits.max_memory_bytes,
            &context,
        )
        .unwrap();
        budget.set_test_limits(FeedHtmlBudgetSnapshot {
            nodes: usize::MAX,
            inlines: usize::MAX,
            assets: usize::MAX,
            diagnostics: usize::MAX,
            strings: usize::MAX,
            output_bytes: u64::MAX,
            persistent_memory_bytes: 0,
        });
        let mut blocks = Vec::<BlockNode>::new();
        reset_feed_html_object_count();
        let error = budget.reserve_vec(&mut blocks, 1).unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
        assert_eq!(blocks.capacity(), 0);
        assert_eq!(feed_html_object_count().capacity_growths, 0);
        assert_eq!(feed_html_object_count().capacity_reserve_calls, 0);

        let exact_bytes = 4 * size_of::<BlockNode>();
        budget.set_test_limits(FeedHtmlBudgetSnapshot {
            nodes: usize::MAX,
            inlines: usize::MAX,
            assets: usize::MAX,
            diagnostics: usize::MAX,
            strings: usize::MAX,
            output_bytes: u64::MAX,
            persistent_memory_bytes: exact_bytes,
        });
        budget.reserve_vec(&mut blocks, 1).unwrap();
        assert_eq!(blocks.capacity(), 4);
        assert_eq!(budget.snapshot().persistent_memory_bytes, exact_bytes);
        assert_eq!(feed_html_object_count().capacity_growths, 1);
        assert_eq!(feed_html_object_count().capacity_reserve_calls, 1);

        let mut string_budget = FeedHtmlBudget::new(
            options.limits.max_feed_text_bytes,
            16,
            options.limits.max_memory_bytes,
            &context,
        )
        .unwrap();
        string_budget.set_test_limits(FeedHtmlBudgetSnapshot {
            nodes: usize::MAX,
            inlines: usize::MAX,
            assets: usize::MAX,
            diagnostics: usize::MAX,
            strings: usize::MAX,
            output_bytes: u64::MAX,
            persistent_memory_bytes: 0,
        });
        reset_feed_html_object_count();
        string_budget.new_output_string(1).unwrap_err();
        assert_eq!(string_budget.snapshot().strings, 0);
        assert_eq!(feed_html_object_count(), FeedHtmlObjectCounts::default());
        string_budget.set_test_limits(FeedHtmlBudgetSnapshot {
            nodes: usize::MAX,
            inlines: usize::MAX,
            assets: usize::MAX,
            diagnostics: usize::MAX,
            strings: usize::MAX,
            output_bytes: u64::MAX,
            persistent_memory_bytes: 64,
        });
        let output = string_budget.new_output_string(1).unwrap();
        assert_eq!(output.capacity(), 64);
        assert_eq!(string_budget.snapshot().persistent_memory_bytes, 64);
        assert_eq!(feed_html_object_count().capacity_growths, 1);
        assert_eq!(feed_html_object_count().capacity_reserve_calls, 1);
        assert_eq!(feed_html_object_count().strings, 1);
    }

    #[test]
    fn feed_capacity_snapshots_follow_actual_allocator_capacity_for_varied_growth() {
        let context = context();
        let options = ConversionOptions::default();
        let mut budget = FeedHtmlBudget::new(
            options.limits.max_feed_text_bytes,
            16,
            options.limits.max_memory_bytes,
            &context,
        )
        .unwrap();
        for requested in [1_usize, 4, 5, 17, 63, 64, 65, 129] {
            let before = budget.snapshot().persistent_memory_bytes;
            let mut values = Vec::<u32>::new();
            budget.reserve_vec(&mut values, requested).unwrap();
            assert_eq!(
                budget.snapshot().persistent_memory_bytes - before,
                values.capacity() * size_of::<u32>()
            );
        }

        let mut values = Vec::<u8>::new();
        for additional in [1_usize, 4, 5, 31, 67] {
            values.resize(values.capacity(), 0);
            let old_capacity = values.capacity();
            let before = budget.snapshot().persistent_memory_bytes;
            budget.reserve_vec(&mut values, additional).unwrap();
            assert_eq!(
                budget.snapshot().persistent_memory_bytes - before,
                values.capacity() - old_capacity
            );
        }

        for requested in [1_usize, 63, 64, 65, 67, 129] {
            let before = budget.snapshot().persistent_memory_bytes;
            let mut value = String::new();
            budget.reserve_string_capacity(&mut value, requested).unwrap();
            assert_eq!(budget.snapshot().persistent_memory_bytes - before, value.capacity());
        }
    }

    #[test]
    fn feed_parser_preflight_is_pinned_checked_and_precedes_parser_construction() {
        let workspace = include_str!("../../../Cargo.toml");
        let lock = include_str!("../../../Cargo.lock");
        assert!(workspace.contains("html5ever = \"=0.39.0\""));
        assert!(lock.contains(
            "name = \"html5ever\"\nversion = \"0.39.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"46a1761807faccc9a19e86944bbf40610014066306f96edcdedc2fb714bcb7b8\""
        ));
        assert!(lock.contains(
            "name = \"markup5ever\"\nversion = \"0.39.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"7122d987ec5f704ee56f6e5b41a7d93722e9aae27ae07cafa4036c4d3f9757de\""
        ));
        assert!(lock.contains(
            "name = \"tendril\"\nversion = \"0.5.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"5fed54709c5b3a53d09bb1c113ea4f5ceafd1e772ddcb0030a82e1d56c087b08\""
        ));
        assert_eq!(BUFFER_QUEUE_SLOTS, 16);
        assert_eq!(TOKENIZER_TENDRILS, 9);
        assert_eq!(TREE_BUILDER_VECTORS, 4);
        assert_eq!(VEC_GROWTH_FACTOR, 2);
        assert_eq!(TENDRIL_GROWTH_FACTOR, 2);
        assert_eq!(ADOPTION_AGENCY_ROUNDS, 8);
        assert_eq!(MUTATIONS_PER_TOKEN, 64);
        assert!(HTML5EVER_MODEL_ID.contains("html5ever@ce64836c"));
        assert!(HTML5EVER_MODEL_ID.contains("tendril@d64dfd4c"));
        assert!(validate_html_model_id("html5ever-0.40.0").is_err());

        let mut fragment = String::from("<div");
        for index in 0..64 {
            write!(&mut fragment, " a{index}='{}'", "x".repeat(16)).unwrap();
        }
        fragment.push_str("><table><tr><td><template>");
        for _ in 0..32 {
            fragment.push_str("<b><i>");
        }
        fragment.push_str("deep");
        for _ in 0..32 {
            fragment.push_str("</b></i>");
        }
        fragment.push_str("</template></td></tr></table><script>");
        fragment.push_str(&"raw<&text".repeat(512));
        fragment.push_str("</script><p>safe</p></div>");

        let probe_context = context();
        let bound = feed_html_parser_memory_bound(&fragment, &probe_context).unwrap();
        let limits = ResourceLimits {
            max_memory_bytes: u64::try_from(bound - 1).unwrap(),
            ..ResourceLimits::default()
        };
        let fail_context = ExecutionContext::new(ExecutionOptions::default(), limits);
        let options = ConversionOptions::default();
        let mut fail_budget = FeedHtmlBudget::new(
            options.limits.max_feed_text_bytes,
            16,
            u64::try_from(bound - 1).unwrap(),
            &fail_context,
        )
        .unwrap();
        reset_feed_html_object_count();
        let error =
            convert_feed_html_fragment(&fragment, None, &options, &fail_context, &mut fail_budget)
                .unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
        assert_eq!(feed_html_object_count().parser_constructions, 0);

        let success_limit = bound.checked_add(2 * 1024 * 1024).unwrap();
        let success_context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits {
                max_memory_bytes: u64::try_from(success_limit).unwrap(),
                ..ResourceLimits::default()
            },
        );
        let mut success_budget = FeedHtmlBudget::new(
            options.limits.max_feed_text_bytes,
            16,
            u64::try_from(success_limit).unwrap(),
            &success_context,
        )
        .unwrap();
        reset_feed_html_object_count();
        let output = convert_feed_html_fragment(
            &fragment,
            None,
            &options,
            &success_context,
            &mut success_budget,
        )
        .unwrap();
        assert!(!output.document.blocks.is_empty());
        assert_eq!(feed_html_object_count().parser_constructions, 1);
    }

    #[test]
    fn malformed_fragment_drops_and_rewinds_before_exact_memory_reuse() {
        let options = ConversionOptions::default();
        let safe = "<p>safe</p>";
        let malformed = "<nav><p>discard</p></nav>";
        let measuring_context = context();
        let mut measuring_budget = FeedHtmlBudget::new(
            options.limits.max_feed_text_bytes,
            16,
            options.limits.max_memory_bytes,
            &measuring_context,
        )
        .unwrap();
        convert_feed_html_fragment(safe, None, &options, &measuring_context, &mut measuring_budget)
            .unwrap();
        let safe_persistent = measuring_budget.snapshot().persistent_memory_bytes;
        let safe_parser = feed_html_parser_memory_bound(safe, &measuring_context).unwrap();
        let malformed_parser =
            feed_html_parser_memory_bound(malformed, &measuring_context).unwrap();
        let exact = safe_parser.max(malformed_parser).checked_add(safe_persistent).unwrap();

        let exact_context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits {
                max_memory_bytes: u64::try_from(exact).unwrap(),
                ..ResourceLimits::default()
            },
        );
        let mut budget = FeedHtmlBudget::new(
            options.limits.max_feed_text_bytes,
            16,
            u64::try_from(exact).unwrap(),
            &exact_context,
        )
        .unwrap();
        let initial = budget.snapshot();
        assert!(matches!(
            convert_feed_html_fragment(malformed, None, &options, &exact_context, &mut budget,),
            Err(ConversionError::Malformed { .. })
        ));
        assert_eq!(budget.snapshot().nodes, initial.nodes);
        assert_eq!(budget.snapshot().persistent_memory_bytes, initial.persistent_memory_bytes);
        convert_feed_html_fragment(safe, None, &options, &exact_context, &mut budget).unwrap();
        assert_eq!(budget.snapshot().persistent_memory_bytes, safe_persistent);
    }

    #[test]
    fn extracts_semantics_and_omits_active_or_boilerplate_content() {
        let output = convert(
            "<!doctype html><title>T</title><nav>menu</nav><main><h1>Hello</h1><p>A <strong>safe</strong> body.</p><script>bad()</script></main>",
        );
        assert_eq!(output.document.metadata.title.as_deref(), Some("T"));
        let rendered = format!("{:?}", output.document.blocks);
        assert!(rendered.contains("Hello") && rendered.contains("safe"));
        assert!(!rendered.contains("menu") && !rendered.contains("bad"));
    }

    #[test]
    fn relative_external_image_is_audited_but_never_fetched() {
        let input = ResolvedInput {
            bytes: Arc::from(b"<main><img src='a.png' alt='A'></main>".as_slice()),
            metadata: SourceMetadata {
                uri: Some("https://example.invalid/docs/page.html".into()),
                ..SourceMetadata::default()
            },
        };
        let output = convert_html(&input, &ConversionOptions::default(), &context()).unwrap();
        assert_eq!(
            output.assets[0].external_uri.as_deref(),
            Some("https://example.invalid/docs/a.png")
        );
        assert!(output.assets[0].bytes.is_empty());
    }

    #[test]
    fn standalone_data_gif_is_fully_audited_and_trailing_bytes_are_rejected() {
        let mut gif = Vec::new();
        GifEncoder::new(&mut gif)
            .encode_frame(Frame::new(RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))))
            .unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&gif);
        let output = convert(&format!(
            "<main><img src='data:image/gif;base64,{encoded}' alt='safe'></main>"
        ));
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].media_type, "image/gif");
        assert_eq!(output.assets[0].bytes, gif);

        let mut trailing = output.assets[0].bytes.clone();
        trailing.push(0);
        let encoded = base64::engine::general_purpose::STANDARD.encode(trailing);
        let source =
            format!("<main><img src='data:image/gif;base64,{encoded}' alt='fallback'></main>");
        let error = convert_html(
            &ResolvedInput {
                bytes: Arc::from(source.as_bytes()),
                metadata: SourceMetadata::default(),
            },
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap_err();
        assert!(matches!(error, ConversionError::Malformed { .. }));
    }

    #[test]
    fn canonical_base64_padding_is_not_counted_as_decoded_asset_bytes() {
        let gif = (1..=8)
            .find_map(|width| {
                let mut bytes = Vec::new();
                GifEncoder::new(&mut bytes)
                    .encode_frame(Frame::new(RgbaImage::from_pixel(
                        width,
                        1,
                        image::Rgba([1, 2, 3, 255]),
                    )))
                    .unwrap();
                let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                encoded.ends_with('=').then_some((bytes, encoded))
            })
            .expect("a small GIF with canonical base64 padding");
        let source = format!("<main><img src='data:image/gif;base64,{}' alt='safe'></main>", gif.1);
        let mut options = ConversionOptions::default();
        options.limits.max_asset_bytes = u64::try_from(gif.0.len()).unwrap();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let output = convert_html(
            &ResolvedInput {
                bytes: Arc::from(source.as_bytes()),
                metadata: SourceMetadata::default(),
            },
            &options,
            &context,
        )
        .unwrap();
        assert_eq!(output.assets[0].bytes, gif.0);
    }

    #[test]
    fn empty_or_multiple_main_uses_deterministic_nonempty_choice() {
        let output = convert("<main></main><main><p>chosen</p></main><nav><p>noise</p></nav>");
        assert!(format!("{:?}", output.document.blocks).contains("chosen"));
        assert!(!format!("{:?}", output.document.blocks).contains("noise"));
    }

    #[test]
    fn svg_descendants_do_not_become_assets() {
        let output = convert(
            "<main><svg><a href='https://e.invalid'><image href='https://e.invalid/a.png'/></a><text>x</text></svg><p>ok</p></main>",
        );
        assert!(output.assets.is_empty());
        assert!(output.diagnostics.iter().any(|d| d.code == "html.activeForeignContentOmitted"));
    }

    #[test]
    fn hidden_repeated_entity_and_implicit_nodes_are_safe() {
        let source = "<main><p>A &amp; B<p hidden>hidden<p aria-hidden=true>aria<p inert>inert<p>C";
        let output = convert(source);
        let rendered = format!("{:?}", output.document.blocks);
        assert!(rendered.contains("A & B") && rendered.contains('C'));
        assert!(
            !rendered.contains("hidden")
                && !rendered.contains("aria")
                && !rendered.contains("inert")
        );
        assert!(output.document.blocks.iter().all(|node| {
            node.provenance.locator.byte_start == Some(0)
                && node.provenance.locator.byte_end == u64::try_from(source.len()).ok()
        }));
    }

    #[test]
    fn unsafe_links_and_bases_are_data_not_authority() {
        let output = convert(
            "<base href='http://127.0.0.1/private/'><main><p><a href='javascript:bad()'>safe label</a></p><img src='x.png' alt='x'></main>",
        );
        assert!(output.assets.is_empty());
        assert!(output.diagnostics.iter().any(|d| d.code == "html.baseRejected"));
        assert!(output.diagnostics.iter().any(|d| d.code == "html.linkUriRejected"));
    }

    #[test]
    fn tables_with_spans_pass_core_grid_validation() {
        let output = convert(
            "<main><table><tr><th rowspan=2>A</th><th colspan=2>B</th></tr><tr><td>C</td><td>D</td></tr></table></main>",
        );
        output.document.validate().unwrap();
    }

    #[test]
    fn only_navigation_has_stable_empty_body_failure() {
        let error = convert_html(
            &ResolvedInput {
                bytes: Arc::from(b"<nav><p>menu only</p></nav>".as_slice()),
                metadata: SourceMetadata::default(),
            },
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap_err();
        assert!(
            matches!(error, ConversionError::Malformed { part: Some(part), .. } if part == "html")
        );
    }

    #[test]
    fn detector_evidence_does_not_capture_xml_text_or_markdown() {
        assert!(
            crate::html_document_evidence("<!doctype html><html><body>x</body></html>", &context())
                .unwrap()
        );
        assert!(
            crate::html_document_evidence("<article><h1>x</h1><p>y</p></article>", &context())
                .unwrap()
        );
        assert!(
            !crate::html_document_evidence(
                "<?xml version='1.0'?><rss><item>x</item></rss>",
                &context()
            )
            .unwrap()
        );
        assert!(!crate::html_document_evidence("ordinary <x> text", &context()).unwrap());
        assert!(
            !crate::html_document_evidence("# Markdown\n\n<div>raw</div>", &context()).unwrap()
        );
        let candidate =
            crate::structured_text_candidate(b"<article><h1>x</h1><p>y</p></article>", &context())
                .unwrap()
                .unwrap();
        assert_eq!(candidate.format, InputFormat::Html);
    }

    #[test]
    fn explicit_charset_wins_over_meta_with_diagnostic() {
        let input = ResolvedInput {
            bytes: Arc::from(
                b"<meta http-equiv='content-type' content='text/html; charset=windows-1252'><main><p>safe</p></main>".as_slice(),
            ),
            metadata: SourceMetadata::default(),
        };
        let mut options = ConversionOptions::default();
        options.text.charset = Some("utf-8".into());
        let output = convert_html(&input, &options, &context()).unwrap();
        assert!(output.diagnostics.iter().any(|d| d.code == "html.metaCharsetIgnored"));
    }

    #[test]
    fn review_p1_1_meta_invalid_attribute_bytes_always_advance() {
        for source in [
            b"<meta @ charset=utf-8><main><p>x</p></main>".as_slice(),
            b"<meta _ : . charset=utf-8><main><p>x</p></main>".as_slice(),
            b"<meta \xc3\xa9 charset=utf-8><main><p>x</p></main>".as_slice(),
        ] {
            assert_eq!(prescan_meta_charset(source, &context()).unwrap().as_deref(), Some("utf-8"));
        }
        assert_eq!(
            prescan_meta_charset(
                b"<meta data-name charset = 'windows-1252'><main>x</main>",
                &context()
            )
            .unwrap()
            .as_deref(),
            Some("windows-1252")
        );
    }

    #[test]
    fn review_p1_2_poisoned_sink_is_constant_noop_and_preserves_first_error() {
        let dom = Dom::new(&ConversionOptions::default(), &context()).unwrap();
        dom.set_error_once(ConversionError::ResourceLimit {
            limit: "first",
            detail: "first error".into(),
        });
        let node_count = dom.nodes.borrow().len();
        let child_count = dom.nodes.borrow()[0].children.len();
        let memory = dom.memory.borrow().mark();
        dom.append(&0, NodeOrText::AppendText(StrTendril::from_slice("ignored")));
        dom.create_element(
            QualName::new(None, html5ever::ns!(html), html5ever::local_name!("div")),
            Vec::new(),
            ElementFlags::default(),
        );
        dom.set_error_once(ConversionError::Internal { detail: "replacement".into() });
        assert_eq!(dom.nodes.borrow().len(), node_count);
        assert_eq!(dom.nodes.borrow()[0].children.len(), child_count);
        assert_eq!(dom.memory.borrow().mark(), memory);
        assert!(matches!(
            dom.error.borrow().as_ref(),
            Some(ConversionError::ResourceLimit { limit: "first", .. })
        ));
    }

    #[test]
    fn review_p1_3_ancestor_exclusion_covers_candidates_and_assets() {
        let output = convert(
            "<div hidden><main><p>secret</p><img src='https://e.invalid/s.png'></main></div><p>real</p>",
        );
        let rendered = format!("{:?}", output.document.blocks);
        assert!(rendered.contains("real"));
        assert!(!rendered.contains("secret"));
        assert!(output.assets.is_empty());

        let output = convert(
            "<nav><main><p>nav secret</p></main></nav><aside><main>aside secret</main></aside><p>real</p>",
        );
        let rendered = format!("{:?}", output.document.blocks);
        assert!(rendered.contains("real"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn review_p1_4_scoring_excludes_boilerplate_and_tokenizes_labels() {
        for label in ["id=advert", "class=ad", "class='hero ad-slot'"] {
            let source = format!("<main><div {label}>noise</div></main><p>real</p>");
            let rendered = format!("{:?}", convert(&source).document.blocks);
            assert!(rendered.contains("real"));
            assert!(!rendered.contains("noise"));
        }
        let rendered =
            format!("{:?}", convert("<main><nav>noise</nav></main><p>real</p>").document.blocks);
        assert!(rendered.contains("real") && !rendered.contains("noise"));
        assert!(
            format!("{:?}", convert("<main><p class=shadow>kept</p></main>").document.blocks)
                .contains("kept")
        );
    }

    #[test]
    fn review_p1_5_direct_and_mixed_container_text_is_preserved() {
        assert!(
            format!("{:?}", convert("<main>Hello world</main>").document.blocks)
                .contains("Hello world")
        );
        assert!(
            format!("{:?}", convert("<body>Body text</body>").document.blocks)
                .contains("Body text")
        );
        let blocks = convert("<main>before<p>middle</p>after</main>").document.blocks;
        assert_eq!(blocks.len(), 3);
        let rendered = format!("{blocks:?}");
        assert!(
            rendered.contains("before")
                && rendered.contains("middle")
                && rendered.contains("after")
        );
    }

    #[test]
    fn review_p1_6_table_occupancy_clamps_and_ignores_nested_rows() {
        let output = convert(
            "<main><table><tr><td rowspan=2>A</td><td>B</td></tr><tr><td colspan=2>C</td></tr></table></main>",
        );
        output.document.validate().unwrap();
        let output = convert("<main><table><tr><td rowspan=2>A</td></tr></table></main>");
        output.document.validate().unwrap();
        assert!(output.diagnostics.iter().any(|d| d.code == "html.tableRowspanClamped"));
        let output = convert(
            "<main><table><tr><td>outer<table><tr><td>inner</td></tr></table></td></tr></table></main>",
        );
        output.document.validate().unwrap();
        assert_eq!(count_tables(&output.document.blocks), 2);
        let output = convert(
            "<main><table><thead><tr><td rowspan=2>A</td></tr></thead><tbody><tr><td rowspan=0>B</td><td colspan=0>C</td></tr><tr><td>D</td></tr></tbody></table></main>",
        );
        output.document.validate().unwrap();
        assert!(output.diagnostics.iter().any(|d| d.code == "html.tableRowspanClamped"));
        assert!(output.diagnostics.iter().any(|d| d.code == "html.tableColspanNormalized"));
    }

    #[test]
    fn review_p1_7_and_8_detector_prefers_markdown_and_stays_bounded() {
        for source in [
            "# Title\n\n<article><p>x</p></article>",
            "```html\n<article><p>x</p></article>\n```",
            "~~~html\n<article><p>x</p></article>\n~~~",
            "    <article><p>x</p></article>",
            "\t<!-- <article><p>x</p></article> -->",
        ] {
            assert_eq!(detected_format(source.as_bytes()), InputFormat::Markdown);
        }
        assert_ne!(
            crate::structured_text_candidate(
                b"<!-- <article><p>x</p></article> --> ordinary text",
                &context()
            )
            .unwrap()
            .map(|candidate| candidate.format),
            Some(InputFormat::Html)
        );
        let mut large = b"<article><p>x</p></article>".to_vec();
        large.resize(super::super::TEXT_INSPECTION_BYTE_LIMIT + 2 * 1024 * 1024, b' ');
        assert_eq!(detected_format(&large), InputFormat::Html);
    }

    #[test]
    fn review_p1_9_meta_prescan_skips_comments_scripts_and_metadata() {
        let mut source = b"<!-- <meta charset=utf-8> --><script><meta charset=utf-8></script><metadata charset=utf-8></metadata><meta charset=windows-1252><main><p>caf".to_vec();
        source.push(0xe9);
        source.extend_from_slice(b"</p></main>");
        let output = convert_html(
            &ResolvedInput { bytes: Arc::from(source), metadata: SourceMetadata::default() },
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert!(format!("{:?}", output.document.blocks).contains("caf\u{e9}"));
        assert_eq!(
            prescan_meta_charset(b"<metadata charset=utf-8><meta charset=big5>", &context())
                .unwrap()
                .as_deref(),
            Some("big5")
        );
        assert_eq!(
            prescan_meta_charset(
                b"<script><meta charset=utf-8></scripture><meta charset=big5></script><meta charset=windows-1252>",
                &context(),
            )
            .unwrap()
            .as_deref(),
            Some("windows-1252")
        );
    }

    #[test]
    fn review_p1_10_metadata_requires_visible_html_head_context() {
        let output = convert(
            "<!doctype html><html><head><template><meta name=author content=evil></template><title>real</title><meta name=author content=good></head><body><svg><title>svg-title</title></svg><p>x</p></body></html>",
        );
        assert_eq!(output.document.metadata.title.as_deref(), Some("real"));
        assert_eq!(output.document.metadata.authors, ["good"]);
    }

    #[test]
    fn review_p2_11_nested_lists_remain_nested_blocks() {
        let output = convert("<main><ul><li>one<ul><li>two</li></ul></li></ul></main>");
        let Block::List { items, .. } = &output.document.blocks[0].block else {
            panic!("expected list")
        };
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].blocks[0].block, Block::Paragraph(_)));
        assert!(matches!(items[0].blocks[1].block, Block::List { .. }));
        output.document.validate().unwrap();
    }

    #[test]
    fn review_p2_12_invalid_base_does_not_hide_first_valid_base() {
        let input = ResolvedInput {
            bytes: Arc::from(
                b"<head><base href='http://127.0.0.1/private/'><base href='https://example.invalid/docs/'></head><body><main><img src='a.png'></main></body>".as_slice(),
            ),
            metadata: SourceMetadata {
                uri: Some("https://source.invalid/root.html".into()),
                ..SourceMetadata::default()
            },
        };
        let output = convert_html(&input, &ConversionOptions::default(), &context()).unwrap();
        assert_eq!(
            output.assets[0].external_uri.as_deref(),
            Some("https://example.invalid/docs/a.png")
        );
        assert!(output.diagnostics.iter().any(|d| d.code == "html.baseRejected"));
    }

    fn detected_format(source: &[u8]) -> InputFormat {
        crate::structured_text_candidate(source, &context()).unwrap().unwrap().format
    }

    fn count_tables(blocks: &[BlockNode]) -> usize {
        blocks
            .iter()
            .map(|node| match &node.block {
                Block::Table { rows, .. } => {
                    1 + rows
                        .iter()
                        .flat_map(|row| &row.cells)
                        .map(|cell| count_tables(&cell.blocks))
                        .sum::<usize>()
                }
                Block::List { items, .. } => {
                    items.iter().map(|item| count_tables(&item.blocks)).sum()
                }
                _ => 0,
            })
            .sum()
    }
}

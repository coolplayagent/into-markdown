use super::allocation::try_clone_string;
use super::error::{limit, malformed};
use super::schema::PROVIDER_ID;
use into_markdown_core::{
    Asset, Block, BlockNode, ConversionError, Diagnostic, Document, Inline, InlineMark, ListKind,
    MAX_DOCUMENT_INLINES, MAX_DOCUMENT_NODES, NodeId, Provenance, ProvenanceKind, Rect,
    SourceLocator,
};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Geometry {
    pub(super) x: i64,
    pub(super) y: i64,
    pub(super) cx: i64,
    pub(super) cy: i64,
    pub(super) rotation: i32,
    pub(super) flip_h: bool,
    pub(super) flip_v: bool,
    pub(super) presence: u8,
    pub(super) transformed_corners: Option<[DisplayPoint; 4]>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct DisplayPoint {
    pub(super) x: f64,
    pub(super) y: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct GroupTransform {
    pub(super) offset_x: i64,
    pub(super) offset_y: i64,
    pub(super) extent_x: i64,
    pub(super) extent_y: i64,
    pub(super) child_x: i64,
    pub(super) child_y: i64,
    pub(super) child_extent_x: i64,
    pub(super) child_extent_y: i64,
    pub(super) rotation: i32,
    pub(super) flip_h: bool,
    pub(super) flip_v: bool,
    pub(super) hidden: bool,
    pub(super) semantic_seen: u8,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct DisplayRect {
    pub(super) left: f64,
    pub(super) top: f64,
    pub(super) right: f64,
    pub(super) bottom: f64,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ContentTypes {
    pub(super) overrides: Vec<(String, String)>,
    pub(super) defaults: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Relationship {
    pub(super) id: String,
    pub(super) target: String,
    pub(super) kind: String,
    pub(super) external: bool,
}

pub(super) type Relationships = Vec<Relationship>;

#[derive(Debug, Clone, Copy)]
pub(super) struct EntryMetadata {
    pub(super) index: usize,
    pub(super) expanded: u64,
    pub(super) compressed: u64,
}

#[derive(Debug)]
pub(super) struct LoadedPart {
    pub(super) bytes: Vec<u8>,
    /// The live package reservation attributable to this owned key, map entry and byte buffer.
    pub(super) charge: u64,
    /// Conservative retained parser-result envelope admitted before parsing begins.
    pub(super) parse_charge: u64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PackageOpenPlan {
    pub(super) entry_count: u32,
    pub(super) name_bytes: u64,
    pub(super) memory_charge: u64,
    pub(super) archive_len: usize,
}

#[derive(Debug)]
pub(super) struct Package<'a> {
    pub(super) source: &'a [u8],
    pub(super) trailing_whitespace_bytes: usize,
    pub(super) entries: Vec<(String, EntryMetadata)>,
    // Lookup-only indexes are never iterated into output, so hash randomization cannot affect
    // conversion order. Fallible capacity admission precedes every new key.
    pub(super) parts: HashMap<String, LoadedPart>,
    pub(super) content_types: ContentTypes,
    pub(super) excluded: HashSet<String>,
    pub(super) dangerous_present: bool,
    pub(super) external_relationships_omitted: bool,
    pub(super) loaded_bytes: u64,
    pub(super) memory: into_markdown_core::ResourceReservation,
    pub(super) memory_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RichStyle {
    pub(super) bold: Option<bool>,
    pub(super) italic: Option<bool>,
    pub(super) underline: Option<bool>,
    pub(super) strike: Option<bool>,
}

impl RichStyle {
    pub(super) fn inherit(&mut self, lower: Self) {
        self.bold = self.bold.or(lower.bold);
        self.italic = self.italic.or(lower.italic);
        self.underline = self.underline.or(lower.underline);
        self.strike = self.strike.or(lower.strike);
    }

    pub(super) fn with_lower(mut self, lower: Self) -> Self {
        self.inherit(lower);
        self
    }

    pub(super) fn from_marks(marks: &[InlineMark]) -> Self {
        Self {
            bold: marks.contains(&InlineMark::Bold).then_some(true),
            italic: marks.contains(&InlineMark::Italic).then_some(true),
            underline: marks.contains(&InlineMark::Underline).then_some(true),
            strike: marks.contains(&InlineMark::Strikethrough).then_some(true),
        }
    }

    pub(super) fn is_absent(self) -> bool {
        self.bold.is_none()
            && self.italic.is_none()
            && self.underline.is_none()
            && self.strike.is_none()
    }
}

#[derive(Default)]
pub(super) struct TextParagraph {
    pub(super) text: Vec<Inline>,
    pub(super) default_marks: Vec<InlineMark>,
    pub(super) default_style: RichStyle,
    pub(super) run_styles: Vec<RichStyle>,
    pub(super) level: u8,
    pub(super) level_explicit: bool,
    pub(super) bullet: Option<ListKind>,
    pub(super) bullet_explicit: bool,
    pub(super) start: u64,
    pub(super) numbering: Option<String>,
    pub(super) bullet_recovered: bool,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(super) enum ShapeKind {
    #[default]
    Text,
    Picture,
    GraphicFrame,
}

#[derive(Default)]
pub(super) struct Shape {
    pub(super) kind: ShapeKind,
    pub(super) z_order: usize,
    pub(super) geometry: Geometry,
    pub(super) pending_groups: Vec<GroupTransform>,
    pub(super) text: Vec<Inline>,
    pub(super) run_styles: Vec<RichStyle>,
    pub(super) paragraphs: Vec<TextParagraph>,
    pub(super) level: u8,
    pub(super) bullet: Option<ListKind>,
    pub(super) paragraph_explicit: u8,
    pub(super) semantic_seen: u8,
    pub(super) list_start: u64,
    pub(super) numbering: Option<String>,
    pub(super) title: bool,
    pub(super) hidden: bool,
    pub(super) placeholder: Option<String>,
    pub(super) placeholder_index: u32,
    pub(super) alt: Option<String>,
    pub(super) image: Option<(String, Option<String>)>,
    pub(super) chart: Option<String>,
    pub(super) table: Option<Vec<Vec<PresentationCell>>>,
    pub(super) languages: Vec<String>,
    pub(super) recoveries: Vec<ShapeRecovery>,
    pub(super) ignore_transform_children: bool,
}

#[derive(Default)]
pub(super) struct PresentationCell {
    pub(super) inlines: Vec<Inline>,
    pub(super) row_span: u32,
    pub(super) column_span: u32,
    pub(super) horizontal_continuation: bool,
    pub(super) vertical_continuation: bool,
}

pub(super) struct ShapeRecovery {
    pub(super) code: &'static str,
    pub(super) message: String,
}

#[derive(Default)]
pub(super) struct ShapeStyle {
    pub(super) geometry: Option<Geometry>,
    pub(super) pending_groups: Vec<GroupTransform>,
    pub(super) paragraphs: Vec<TextParagraph>,
    pub(super) title: bool,
    pub(super) hidden: bool,
    pub(super) languages: Vec<String>,
    pub(super) class: PlaceholderClass,
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct PlaceholderKey {
    pub(super) index: u32,
    pub(super) class: PlaceholderClass,
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum PlaceholderClass {
    Title,
    #[default]
    Body,
    Date,
    Footer,
    SlideNumber,
    Header,
}

#[derive(Default)]
pub(super) struct ParseState {
    pub(super) document: Document,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) assets: Vec<Asset>,
    // These indexes are lookup-only and never iterated into output, so randomized hash order
    // cannot affect determinism. `try_reserve` precedes every new key and avoids the quadratic
    // shifting cost of sorted vectors for presentations containing many unique images.
    pub(super) assets_by_part: HashMap<String, String>,
    pub(super) assets_by_digest: HashMap<[u8; 32], Vec<usize>>,
    pub(super) nodes: usize,
    pub(super) inlines: usize,
    pub(super) asset_bytes: u64,
}

impl ParseState {
    pub(super) fn notes_heading(
        &mut self,
        part: &str,
        slide: u32,
    ) -> Result<BlockNode, ConversionError> {
        self.add_inlines(1)?;
        let mut heading = self.node(
            Block::Heading {
                level: 3,
                content: vec![into_markdown_core::Inline::Text {
                    value: try_clone_string("Speaker notes", "notes heading")?,
                    marks: Vec::new(),
                }],
            },
            part,
            slide,
            None,
            None,
            None,
        )?;
        into_markdown_core::speaker_notes::mark_heading(&mut heading)?;
        Ok(heading)
    }

    pub(super) fn mark_note_body(&mut self, node: &mut BlockNode) -> Result<(), ConversionError> {
        let previous = node.id.0.clone();
        into_markdown_core::speaker_notes::mark_body(node)?;
        for prefix in ["presentation.zOrder.", "presentation.languages."] {
            if let Some(value) =
                self.document.metadata.properties.remove(&format!("{prefix}{previous}"))
            {
                self.document.metadata.properties.insert(format!("{prefix}{}", node.id.0), value);
            }
        }
        Ok(())
    }

    pub(super) fn node(
        &mut self,
        block: Block,
        part: &str,
        slide: u32,
        bounds: Option<Rect>,
        source_order: Option<usize>,
        source_languages: Option<&[String]>,
    ) -> Result<BlockNode, ConversionError> {
        self.nodes =
            self.nodes.checked_add(1).ok_or_else(|| limit("max_document_nodes", "overflow"))?;
        if self.nodes > MAX_DOCUMENT_NODES {
            return Err(limit(
                "max_document_nodes",
                format!("{} > {MAX_DOCUMENT_NODES}", self.nodes),
            ));
        }
        let mut node_id = String::new();
        node_id.try_reserve(96).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve node identifier: {error}"))
        })?;
        if let Some(z_order) = source_order {
            write!(node_id, "presentation-slide-{slide}-z-{z_order}-node-{}", self.nodes)
                .map_err(|_| malformed(Some(part), "cannot format node identifier"))?;
        } else {
            write!(node_id, "presentation-node-{}", self.nodes)
                .map_err(|_| malformed(Some(part), "cannot format node identifier"))?;
        }
        if let Some(z_order) = source_order {
            let mut key = String::new();
            key.try_reserve(node_id.len().saturating_add(20)).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve z-order metadata: {error}"))
            })?;
            key.push_str("presentation.zOrder.");
            key.push_str(&node_id);
            let mut value = String::new();
            value.try_reserve(20).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve z-order value: {error}"))
            })?;
            write!(value, "{z_order}")
                .map_err(|_| malformed(Some(part), "cannot format z-order metadata"))?;
            self.document.metadata.properties.insert(key, value);
        }
        if let Some(languages) = source_languages.filter(|values| !values.is_empty()) {
            let capacity = languages.iter().try_fold(
                languages.len().saturating_sub(1),
                |total, language| {
                    total.checked_add(language.len()).ok_or_else(|| {
                        limit("max_memory_bytes", "language metadata length overflow")
                    })
                },
            )?;
            let mut value = String::new();
            value.try_reserve_exact(capacity).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve language metadata: {error}"))
            })?;
            for (index, language) in languages.iter().enumerate() {
                if index != 0 {
                    value.push(',');
                }
                value.push_str(language);
            }
            let mut key = String::new();
            key.try_reserve(node_id.len().saturating_add(23)).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve language key: {error}"))
            })?;
            key.push_str("presentation.languages.");
            key.push_str(&node_id);
            self.document.metadata.properties.insert(key, value);
        }
        Ok(BlockNode {
            id: NodeId(node_id),
            block,
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: try_clone_string(PROVIDER_ID, "provenance provider")?,
                locator: SourceLocator {
                    slide: (slide != 0).then_some(slide),
                    bounds,
                    part: Some(try_clone_string(part, "provenance part")?),
                    ..SourceLocator::default()
                },
                confidence: Some(1.0),
            },
        })
    }

    pub(super) fn add_inlines(&mut self, count: usize) -> Result<(), ConversionError> {
        self.inlines = self
            .inlines
            .checked_add(count)
            .ok_or_else(|| limit("max_document_inlines", "overflow"))?;
        if self.inlines > MAX_DOCUMENT_INLINES {
            return Err(limit(
                "max_document_inlines",
                format!("{} > {MAX_DOCUMENT_INLINES}", self.inlines),
            ));
        }
        Ok(())
    }
}

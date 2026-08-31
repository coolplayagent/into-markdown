//! Bounded, offline OPC/PresentationML conversion.

mod allocation;
mod budget;
mod charts_notes;
mod content_types;
mod error;
mod geometry;
mod images;
mod mce;
mod model;
mod opc_package;
mod output;
mod raw_zip;
mod relationships;
mod schema;
mod shape_elements;
mod slides;
mod styles;
mod tables;
mod text;
mod xml;
mod xml_base;

#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod test_observer;

use allocation::try_clone_string;
use error::{limit, malformed};
use geometry::sort_shapes_for_reading;
use into_markdown_core::{
    Block, BoxFuture, ConversionError, ConversionOptions, Converter, ConverterEventSink,
    ConverterOutput, ConverterStream, ConverterStreamCompletion, ConverterStreamMode, Diagnostic,
    DiagnosticSeverity, ExecutionContext, FormatCandidate, Inline, InputFormat, LocalBoxFuture,
    ProbeOutcome, ResolvedInput, Services, SourceContentEvidence, SourceLocator,
    StreamConsumerKind, document_is_empty, estimate_retained_output,
    estimate_validation_working_set, stream_converter_output,
};
use model::{Package, ParseState};
use output::shapes_to_blocks;
use raw_zip::package_open_plan;
use relationships::{
    is_presentation_main_type, relationship_by_id, relationship_part, require_content_type,
    resolve_target, unique_internal, unique_relationship,
};
use schema::{COMPOUND_FILE_SIGNATURE, FORMATS, NOTES_REL, OFFICE_REL, PROVIDER_ID, SLIDE_REL};
use shape_elements::validate_shape_relationships;
use slides::{parse_shapes, parse_slide_order, slide_is_hidden};
use std::fmt::Write as _;
use styles::{apply_inheritance, apply_pending_group_transforms, inherited_styles};
use text::shape_plain_text;
use xml::{XmlProfile, preflight_xml};
// LIVE-MEMORY INVENTORY:
// - output-retained: Document blocks/inlines/metadata/provenance/NodeId/string capacities,
//   diagnostics (including locators), and Asset ids/filenames/media types/byte capacities;
// - conversion-lifetime: Package entry names/nodes, content-type keys/values, excluded names,
//   loaded part keys/Vec capacities, relationship/style/placeholder/slide/shape/table/chart
//   collections, language sets, and both asset indexes (part and digest candidates);
// - transient high-water: ZipArchive/read buffers, XML namespace/name/attribute identities plus
//   stack/width/MC state, decoded text/attribute buffers, image-codec pixels/tables, SHA-256
//   hashing/collision confirmation, and geometry rect/order/union-find/component/corner storage.
// The conservative Package reservation is acquired before ZIP materialization, grown before each
// reachable part or retained index, held across all parser/geometry/codec/output allocations, then
// authenticated and shrunk by `ConverterOutput::certify_preflight_reservation` to the central
// retained estimate. Validation receives a separate preflight reservation while the package peak
// remains live. Exact/boundary-minus-one/drop tests exercise the output lease; peak tests exercise
// the conversion and transient categories above.

/// Bounded, non-networking `PresentationML` converter. It never opens macro or embedded-object parts.
#[derive(Debug, Default)]
pub struct PresentationConverter;

impl Converter for PresentationConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        250
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        FORMATS
    }

    fn stream_support(&self) -> Option<&dyn ConverterStream> {
        Some(self)
    }

    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            if candidate.format != InputFormat::Pptx {
                return Ok(ProbeOutcome::NotApplicable);
            }
            let zip = input.bytes.starts_with(b"PK\x03\x04")
                || input.bytes.starts_with(b"PK\x05\x06")
                || input.bytes.starts_with(b"PK\x07\x08");
            Ok(if candidate.explicit || candidate.detector_id == "builtin.detector.hints" || zip {
                ProbeOutcome::Match { confidence: 1.0 }
            } else {
                ProbeOutcome::NotApplicable
            })
        })
    }

    fn planned_output_bytes(
        &self,
        input: &ResolvedInput,
        _: &FormatCandidate,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        // A compound-file wrapper must reach `convert` so it keeps the stable `encrypted` error.
        // For ZIP input, the allocation-free central-directory plan establishes the minimum safe
        // entry point. Once admitted, the outer engine reservation lends the converter all
        // remaining request memory as an authenticated credit, so internal incremental permits do
        // not double-charge and the final retained lease can be certified by the engine.
        if input.bytes.starts_with(COMPOUND_FILE_SIGNATURE) {
            return Ok(0);
        }
        let minimum = package_open_plan(&input.bytes, options, context)?.memory_charge;
        Ok(minimum.max(context.available_memory_bytes()))
    }

    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move { convert_presentation(&input.bytes, options, context) })
    }
}

impl ConverterStream for PresentationConverter {
    fn stream_mode(&self) -> ConverterStreamMode {
        ConverterStreamMode::Native
    }

    fn stream_mode_for(
        &self,
        input: &ResolvedInput,
        _: &FormatCandidate,
        _: &ConversionOptions,
        consumer: StreamConsumerKind,
    ) -> ConverterStreamMode {
        if consumer == StreamConsumerKind::Collecting
            && input.bytes.starts_with(COMPOUND_FILE_SIGNATURE)
        {
            ConverterStreamMode::AggregateAdapter
        } else {
            ConverterStreamMode::Native
        }
    }

    fn convert_stream<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
        sink: &'a mut dyn ConverterEventSink,
    ) -> LocalBoxFuture<'a, Result<ConverterStreamCompletion, ConversionError>> {
        Box::pin(async move {
            let output = convert_presentation(&input.bytes, options, context)?;
            stream_converter_output(output, sink)
        })
    }
}

#[allow(clippy::too_many_lines)]
fn convert_presentation(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let mut package = Package::open(bytes, options, context)?;
    let root_relationships = package.relationships("", options, context)?;
    let main = unique_internal(&root_relationships, OFFICE_REL, "")?
        .ok_or_else(|| malformed(Some("_rels/.rels"), "officeDocument relationship is missing"))?;
    let main_part = resolve_target("", &main.target)?;
    let main_type = package.content_types.content_type(&main_part).ok_or_else(|| {
        malformed(Some("[Content_Types].xml"), "presentation main part lacks content type")
    })?;
    if !is_presentation_main_type(main_type) {
        return Err(malformed(
            Some("[Content_Types].xml"),
            "officeDocument does not target a supported PresentationML main part",
        ));
    }
    let (slides, omitted_slide_references) = {
        let main_bytes = package.load_for_parse(&main_part, options, context)?;
        preflight_xml(main_bytes, &main_part, XmlProfile::Presentation, options, context)?;
        parse_slide_order(main_bytes, &main_part, options, context)?
    };
    package.release_parsed(&main_part)?;
    if u32::try_from(slides.len()).unwrap_or(u32::MAX) > options.limits.max_pages {
        return Err(limit("max_pages", format!("{} slides", slides.len())));
    }
    let main_relationships = package.relationships(&main_part, options, context)?;
    let main_relationship_part = relationship_part(&main_part)?;
    let mut state = ParseState::default();
    if package.trailing_whitespace_bytes != 0 {
        state.diagnostics.push(Diagnostic {
            code: "presentation.zipTrailingWhitespaceIgnored".into(),
            severity: DiagnosticSeverity::Info,
            message: format!(
                "ignored {} whitespace byte(s) after ZIP end record and comment",
                package.trailing_whitespace_bytes
            ),
            locator: None,
        });
    }
    if omitted_slide_references > 0 {
        state.diagnostics.try_reserve(1).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve slide omission diagnostic: {error}"))
        })?;
        state.diagnostics.push(Diagnostic {
            code: "office.relationshipOmitted".into(),
            severity: DiagnosticSeverity::Warning,
            message: format!(
                "{omitted_slide_references} slide-list entries without relationships were omitted"
            ),
            locator: Some(into_markdown_core::SourceLocator {
                part: Some(main_part.clone()),
                ..into_markdown_core::SourceLocator::default()
            }),
        });
    }
    for (index, slide_reference) in slides.into_iter().enumerate() {
        context.checkpoint()?;
        let slide_number =
            u32::try_from(index + 1).map_err(|_| limit("max_pages", "slide number overflow"))?;
        let relationship = relationship_by_id(
            &main_relationships,
            &slide_reference.relationship_id,
        )
        .ok_or_else(|| {
            malformed(
                Some(&main_relationship_part),
                format!("slide relationship {} is missing", slide_reference.relationship_id),
            )
        })?;
        if relationship.external || relationship.kind != SLIDE_REL {
            return Err(malformed(
                Some(&main_relationship_part),
                "slide relationship has wrong type or mode",
            ));
        }
        let slide_part = resolve_target(&main_part, &relationship.target)?;
        require_content_type(
            &package,
            &slide_part,
            "application/vnd.openxmlformats-officedocument.presentationml.slide+xml",
        )?;
        let (hidden, mut shapes) = {
            let bytes = package.load_for_parse(&slide_part, options, context)?;
            let hidden = slide_is_hidden(bytes, &slide_part, context)?;
            let shapes = parse_shapes(bytes, &slide_part, XmlProfile::Slide, options, context)?;
            (hidden, shapes)
        };
        package.release_parsed(&slide_part)?;
        let slide_relationships = package.relationships_optional(&slide_part, options, context)?;
        validate_shape_relationships(
            &shapes,
            &mut package,
            &slide_relationships,
            &slide_part,
            options,
            context,
        )?;
        if hidden {
            state.diagnostics.try_reserve(1).map_err(|error| {
                limit(
                    "max_memory_bytes",
                    format!("cannot reserve hidden-slide diagnostic: {error}"),
                )
            })?;
            state.diagnostics.push(Diagnostic {
                code: "presentation.hiddenSlideSkipped".into(),
                severity: DiagnosticSeverity::Warning,
                message: format!("hidden slide {slide_number} was deterministically omitted"),
                locator: Some(SourceLocator {
                    slide: Some(slide_number),
                    part: Some(slide_part),
                    ..SourceLocator::default()
                }),
            });
            continue;
        }
        let (layout, master, theme_name) =
            inherited_styles(&mut package, &slide_part, &slide_relationships, options, context)?;
        if let Some(theme_name) = theme_name {
            let mut key = String::new();
            key.try_reserve(48).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve theme metadata key: {error}"))
            })?;
            write!(key, "presentation.theme.slide-{slide_number}")
                .map_err(|_| malformed(Some(&slide_part), "cannot format theme metadata key"))?;
            state.document.metadata.properties.insert(key, theme_name);
        }
        apply_inheritance(&mut shapes, &layout, &master)?;
        apply_pending_group_transforms(&mut shapes)?;
        shapes.retain(|shape| !shape.hidden);
        sort_shapes_for_reading(&mut shapes, context)?;
        let mut title_positions = shapes.iter().enumerate().filter(|(_, shape)| shape.title);
        let title_position = title_positions.next().map(|(index, _)| index);
        if title_positions.next().is_some() {
            return Err(malformed(Some(&slide_part), "slide has multiple title placeholders"));
        }
        let title_shape = title_position.map(|index| shapes.remove(index));
        let (title, title_bounds, title_z_order, title_languages) =
            if let Some(title_shape) = title_shape {
                (
                    Some(shape_plain_text(&title_shape)?),
                    Some(title_shape.geometry.bounds()?),
                    Some(title_shape.z_order),
                    Some(title_shape.languages),
                )
            } else {
                (None, None, None, None)
            };
        let mut slide_blocks = shapes_to_blocks(
            shapes,
            &mut package,
            &slide_relationships,
            &slide_part,
            slide_number,
            options,
            context,
            &mut state,
        )?;
        if let Some(notes_rel) = unique_relationship(&slide_relationships, NOTES_REL, &slide_part)?
        {
            let notes_part = resolve_target(&slide_part, &notes_rel.target)?;
            require_content_type(
                &package,
                &notes_part,
                "application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml",
            )?;
            let notes_relationships =
                package.relationships_optional(&notes_part, options, context)?;
            let mut notes = {
                let bytes = package.load_for_parse(&notes_part, options, context)?;
                parse_shapes(bytes, &notes_part, XmlProfile::Notes, options, context)?
            };
            package.release_parsed(&notes_part)?;
            validate_shape_relationships(
                &notes,
                &mut package,
                &notes_relationships,
                &notes_part,
                options,
                context,
            )?;
            apply_pending_group_transforms(&mut notes)?;
            notes.retain(|shape| {
                !shape.hidden
                    && !matches!(
                        shape.placeholder.as_deref(),
                        Some("sldNum" | "dt" | "hdr" | "ftr")
                    )
            });
            sort_shapes_for_reading(&mut notes, context)?;
            let mut note_blocks = shapes_to_blocks(
                notes,
                &mut package,
                &notes_relationships,
                &notes_part,
                slide_number,
                options,
                context,
                &mut state,
            )?;
            if into_markdown_core::speaker_notes::has_visible_content(
                &note_blocks,
                into_markdown_core::AssetMode::Extract,
            ) {
                slide_blocks.try_reserve(note_blocks.len().saturating_add(1)).map_err(|error| {
                    limit("max_memory_bytes", format!("cannot reserve note blocks: {error}"))
                })?;
                state.add_inlines(1)?;
                let mut heading = state.node(
                    Block::Heading {
                        level: 3,
                        content: vec![Inline::Text {
                            value: try_clone_string("Speaker notes", "notes heading")?,
                            marks: Vec::new(),
                        }],
                    },
                    &notes_part,
                    slide_number,
                    None,
                    None,
                    None,
                )?;
                into_markdown_core::speaker_notes::mark_heading(&mut heading)?;
                for block in &mut note_blocks {
                    state.mark_note_body(block)?;
                }
                slide_blocks.push(heading);
                slide_blocks.append(&mut note_blocks);
            }
        }
        let slide = state.node(
            Block::Slide { number: slide_number, title, blocks: slide_blocks },
            &slide_part,
            slide_number,
            title_bounds,
            title_z_order,
            title_languages.as_deref(),
        )?;
        state.document.blocks.try_reserve(1).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve slide output: {error}"))
        })?;
        state.document.blocks.push(slide);
    }
    if package.dangerous_present {
        state.diagnostics.try_reserve(1).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve security diagnostic: {error}"))
        })?;
        state.diagnostics.push(Diagnostic {
            code: "presentation.dangerousPartsIgnored".into(),
            severity: DiagnosticSeverity::Warning,
            message: "macro, ActiveX, OLE or embedded-package parts were isolated and not read"
                .into(),
            locator: Some(SourceLocator {
                part: Some("[Content_Types].xml".into()),
                ..SourceLocator::default()
            }),
        });
        state
            .document
            .metadata
            .properties
            .insert("presentation.dangerousPartsPresent".into(), "true".into());
    }
    if package.external_relationships_omitted {
        state.diagnostics.try_reserve(1).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve external-link diagnostic: {error}"))
        })?;
        state.diagnostics.push(Diagnostic {
            code: "presentation.externalRelationshipsOmitted".into(),
            severity: DiagnosticSeverity::Warning,
            message: "external relationships were not fetched; visible document text was retained"
                .into(),
            locator: Some(SourceLocator { part: Some("ppt".into()), ..SourceLocator::default() }),
        });
    }
    let validation_bytes =
        estimate_validation_working_set(&state.document, &state.assets, &state.diagnostics)?;
    let validation_memory = context.reserve_memory(validation_bytes)?;
    state.document.validate().map_err(|error| ConversionError::Internal {
        detail: format!(
            "PresentationML emitted invalid IR ({} at {}): {}",
            error.code.as_str(),
            error.path,
            error.detail
        ),
    })?;
    drop(validation_memory);

    // No allocation may occur between releasing the temporary package graph and attaching its
    // authenticated parent reservation. Construct the output first while the full peak remains
    // charged, then consume `package` and transfer the reservation through the core authority.
    let retained = estimate_retained_output(&state.document, &state.assets, &state.diagnostics)?;
    if package.memory_bytes < retained {
        package.grow_memory(retained - package.memory_bytes)?;
    }
    let evidence = if document_is_empty(&state.document)
        && state.assets.is_empty()
        && state.diagnostics.is_empty()
    {
        SourceContentEvidence::Empty
    } else {
        SourceContentEvidence::Unknown
    };
    let output = ConverterOutput::new(state.document, state.assets, state.diagnostics)
        .with_source_content_evidence(evidence);
    let Package { memory, .. } = package;
    output.certify_preflight_reservation(context, memory)
}

#[cfg(test)]
mod tests;

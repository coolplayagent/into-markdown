//! Bounded, offline `OpenDocument` text, spreadsheet, and presentation conversion.
//!
//! The accepted profile is intentionally smaller than the complete ODF grammar. It accepts
//! package-based ODT/ODS/ODP documents with the standard content, styles, metadata, settings,
//! list, table, drawing, annotation, and image vocabulary. Encryption, signatures, DTDs,
//! processing instructions, unknown namespaces, external images, unsafe paths and CRC failures
//! remain errors. Best-effort conversion diagnoses omitted optional scripts, embedded objects,
//! animation and unsupported graphics while retaining current text, cached fields and tables;
//! strict conversion rejects these lossy projections. No scripts or formulas are executed.
//! Hyperlinks remain inert HTTP(S), mailto or same-document fragment references.

use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput, ExecutionContext,
    FormatCandidate, InputFormat, ProbeOutcome, ResolvedInput, Services,
};

mod annotations;
mod compatibility;
mod content;
mod geometry;
mod image_validation;
mod images;
mod manifest;
mod metadata;
mod model;
mod package;
mod paragraphs;
mod paths;
mod presentation;
mod profile;
mod raw_zip;
mod recovery;
mod semantic;
mod sheets;
mod sparse;
mod styles;
mod tables;
mod text;
mod xml;

use annotations::{collect_image_anchors, validate_ranged_annotations};
use content::parse_content;
use metadata::parse_metadata;
use model::{FORMATS, OFFICE_NS, PROVIDER_ID, ParseState, limit, malformed, part_locator};
use package::Package;
use profile::{OdfXmlPart, validate_tree_profile};
use styles::{collect_styles, validate_document_versions};
use xml::parse_xml;

#[cfg(test)]
mod tests;

/// Strict, deterministic `OpenDocument` converter. It never invokes an office application.
#[derive(Debug, Default)]
pub struct OdfConverter;

impl Converter for OdfConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn priority(&self) -> i32 {
        245
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
            if !FORMATS.contains(&candidate.format) {
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
        _: &ResolvedInput,
        _: &FormatCandidate,
        _: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        // The engine authenticates this outer permit and lends it back as a scoped credit. The
        // converter takes one child reservation before constructing ZipArchive/XML/image objects,
        // then shrinks that same owner to the central retained-output estimate after temporaries
        // have been destroyed.
        Ok(context.available_memory_bytes())
    }

    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move { convert_odf(&input.bytes, candidate.format, options, context) })
    }
}

fn convert_odf(
    bytes: &[u8],
    format: InputFormat,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let planned = context.available_memory_bytes();
    if planned == 0 {
        return Err(limit(
            "max_memory_bytes",
            "ODF conversion has no authenticated preflight credit",
        ));
    }
    // This is the sole child of the Engine's authenticated preflight reservation. It precedes
    // every third-party container/parser allocation and remains live until all parsing
    // temporaries have dropped and retained output is centrally certified.
    let memory = context.reserve_memory(planned)?;
    let (document, assets, diagnostics) = {
        let mut package = Package::open(bytes, format, options, context, planned)?;
        let content_bytes = package
            .parts
            .get("content.xml")
            .ok_or_else(|| malformed(Some("content.xml"), "content part is missing"))?;
        let mut content_root = parse_xml(content_bytes, "content.xml", options, context)?;
        let mut styles_root = package
            .parts
            .get("styles.xml")
            .map(|bytes| parse_xml(bytes, "styles.xml", options, context))
            .transpose()?;
        if styles_root.as_ref().is_some_and(|root| !root.is(OFFICE_NS, "document-styles")) {
            return Err(malformed(Some("styles.xml"), "unexpected styles root"));
        }
        let metadata_root = package
            .parts
            .get("meta.xml")
            .map(|bytes| parse_xml(bytes, "meta.xml", options, context))
            .transpose()?;
        let settings_root = package
            .parts
            .get("settings.xml")
            .map(|bytes| parse_xml(bytes, "settings.xml", options, context))
            .transpose()?;
        let mut state = ParseState::default();
        if package.noncanonical_mimetype {
            state.warning("odf.noncanonicalMimetype", "Noncanonical mimetype order/compression/descriptor accepted after complete ZIP integrity and exact media-type validation", part_locator("mimetype"));
        }
        for part in &package.missing_optional_parts {
            state.warning(
                "odf.optionalPartMissing",
                "Missing optional metadata, UI configuration or unreferenced image; referenced images and document body remain required",
                part_locator(part),
            );
        }
        recovery::project_static_content(
            &mut content_root,
            "content.xml",
            &mut state,
            options,
            context,
            &package.manifest,
        )?;
        if let Some(root) = &mut styles_root {
            recovery::project_static_content(
                root,
                "styles.xml",
                &mut state,
                options,
                context,
                &package.manifest,
            )?;
        }
        for (root, profile, part) in [
            (Some(&content_root), OdfXmlPart::Content, "content.xml"),
            (styles_root.as_ref(), OdfXmlPart::Styles, "styles.xml"),
            (metadata_root.as_ref(), OdfXmlPart::Meta, "meta.xml"),
            (settings_root.as_ref(), OdfXmlPart::Settings, "settings.xml"),
        ] {
            if let Some(root) = root {
                let count = validate_tree_profile(root, profile, part)?;
                if count > 0 {
                    state.warning("odf.layoutMetadata", format!("{count} layout definitions/producer hints are not represented in Markdown; text and table semantics are parsed separately"), part_locator(part));
                }
            }
        }
        validate_document_versions(
            &package.odf_version,
            &content_root,
            styles_root.as_ref(),
            metadata_root.as_ref(),
            settings_root.as_ref(),
        )?;
        validate_ranged_annotations(&content_root, options, context)?;
        let image_anchors = collect_image_anchors(&content_root, &package.manifest)?;
        package.load_reachable_images(bytes, &image_anchors, options, context)?;
        let mut styles = collect_styles(styles_root.as_ref(), &content_root)?;
        if styles.identical_duplicates > 0 {
            state.warning("odf.identicalStyle", format!("{} identical duplicate style definitions reused; conflicting definitions remain errors", styles.identical_duplicates), part_locator("styles.xml"));
        }
        state.list_styles = std::mem::take(&mut styles.lists);
        parse_metadata(metadata_root.as_ref(), settings_root.as_ref(), &mut state, options)?;
        parse_content(&content_root, format, &styles, &package, &mut state, options, context)?;
        state.document.blocks.append(&mut state.deferred);
        recovery::ensure_static_body(&state)?;
        (state.document, state.assets, state.diagnostics)
    };
    ConverterOutput::new_with_memory_reservation(document, assets, diagnostics, context, memory)
}

//! Bounded, offline `OpenDocument` 1.3 text, spreadsheet, and presentation conversion.
//!
//! The accepted profile is intentionally smaller than the complete ODF grammar. It accepts
//! package-based ODT/ODS/ODP documents with the standard content, styles, metadata, settings,
//! list, table, drawing, annotation, and image vocabulary. It rejects encryption, signatures,
//! scripts/macros, embedded documents, DTDs, processing instructions, undeclared/foreign
//! namespaces, external images, and unsafe package paths. Hyperlinks are retained as inert data
//! only when they are absolute HTTP(S), mailto, or same-document fragment references.

use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput, ExecutionContext,
    FormatCandidate, InputFormat, ProbeOutcome, ResolvedInput, Services,
};

mod annotations;
mod content;
mod geometry;
mod image_validation;
mod images;
mod manifest;
mod metadata;
mod model;
mod package;
mod paths;
mod presentation;
mod profile;
mod raw_zip;
mod semantic;
mod sheets;
mod styles;
mod tables;
mod text;
mod xml;

use annotations::{collect_image_anchors, validate_ranged_annotations};
use content::parse_content;
use metadata::parse_metadata;
use model::{FORMATS, OFFICE_NS, PROVIDER_ID, ParseState, limit, malformed};
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
        let content_root = parse_xml(content_bytes, "content.xml", options, context)?;
        let styles_root = package
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
        validate_tree_profile(&content_root, OdfXmlPart::Content, "content.xml")?;
        if let Some(root) = styles_root.as_ref() {
            validate_tree_profile(root, OdfXmlPart::Styles, "styles.xml")?;
        }
        if let Some(root) = metadata_root.as_ref() {
            validate_tree_profile(root, OdfXmlPart::Meta, "meta.xml")?;
        }
        if let Some(root) = settings_root.as_ref() {
            validate_tree_profile(root, OdfXmlPart::Settings, "settings.xml")?;
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
        let styles = collect_styles(styles_root.as_ref(), &content_root)?;
        let mut state = ParseState { list_styles: styles.lists, ..ParseState::default() };
        parse_metadata(metadata_root.as_ref(), settings_root.as_ref(), &mut state, options)?;
        parse_content(&content_root, format, &styles.text, &package, &mut state, options, context)?;
        state.document.blocks.append(&mut state.deferred);
        (state.document, state.assets, state.diagnostics)
    };
    ConverterOutput::new_with_memory_reservation(document, assets, diagnostics, context, memory)
}

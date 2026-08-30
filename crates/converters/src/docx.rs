//! Bounded, offline `WordprocessingML` (`.docx`/`.docm`) conversion.

use image::{ImageDecoder as _, Limits as ImageLimits, codecs::jpeg::JpegDecoder};
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, BoxFuture, Cell, ConversionError, ConversionOptions,
    Converter, ConverterOutput, Diagnostic, DiagnosticSeverity, Document, ExecutionContext,
    FormatCandidate, Inline, InlineMark, InputFormat, ListItem, ListKind, MAX_DOCUMENT_INLINES,
    MAX_DOCUMENT_NODES, MAX_TABLE_COLUMNS, NodeId, ProbeOutcome, Provenance, ProvenanceKind,
    ResolvedInput, Services, SourceContentEvidence, SourceLocator, TableAlignment, TableRow,
};
use quick_xml::events::{BytesCData, BytesRef, BytesStart, BytesText, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use quick_xml::reader::Reader;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Cursor, Read};
use std::path::{Component, Path};

const FORMATS: &[InputFormat] = &[InputFormat::Docx];
const PROVIDER_ID: &str = "builtin.converter.docx";
const XML_EVENT_FACTOR: u64 = 8;
const WORD_NS: &[u8] = b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const STRICT_WORD_NS: &[u8] = b"http://purl.oclc.org/ooxml/wordprocessingml/main";
const OFFICE_REL_NS: &[u8] = b"http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const STRICT_OFFICE_REL_NS: &[u8] = b"http://purl.oclc.org/ooxml/officeDocument/relationships";
const PACKAGE_REL_NS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/relationships";
const CONTENT_TYPES_NS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/content-types";
const MATH_NS: &[u8] = b"http://schemas.openxmlformats.org/officeDocument/2006/math";
const MC_NS: &[u8] = b"http://schemas.openxmlformats.org/markup-compatibility/2006";
const DRAWING_NS: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/main";
const WORD_DRAWING_NS: &[u8] =
    b"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
const VML_NS: &[u8] = b"urn:schemas-microsoft-com:vml";
const CHART_NS: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/chart";
const DIAGRAM_NS: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/diagram";
const OFFICE_VML_NS: &[u8] = b"urn:schemas-microsoft-com:office:office";
const CORE_PROPERTIES_NS: &[u8] =
    b"http://schemas.openxmlformats.org/package/2006/metadata/core-properties";
const DUBLIN_CORE_NS: &[u8] = b"http://purl.org/dc/elements/1.1/";
const DUBLIN_CORE_TERMS_NS: &[u8] = b"http://purl.org/dc/terms/";
const OFFICE_REL_TYPE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
const REL_TYPE_PREFIX: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";
const STRICT_REL_TYPE_PREFIX: &str = "http://purl.oclc.org/ooxml/officeDocument/relationships/";
const MAX_IMAGE_DIMENSION: u32 = 32_768;
const MAX_IMAGE_PIXELS: u64 = 100_000_000;

/// Bounded, non-networking Word Open XML converter. Macro parts are never opened.
#[derive(Debug, Default)]
pub struct DocxConverter;

impl Converter for DocxConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn priority(&self) -> i32 {
        250
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
            if candidate.format != InputFormat::Docx {
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
        Box::pin(async move { convert_docx(&input.bytes, options, context) })
    }
}

include!("docx/package.rs");
include!("docx/content_types.rs");
include!("docx/alt_chunk.rs");
include!("docx/styles_numbering.rs");
include!("docx/word.rs");
include!("docx/fields.rs");
include!("docx/tables.rs");
include!("docx/media.rs");
include!("docx/relationships.rs");
include!("docx/xml.rs");

fn malformed(part: Option<&str>, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: part.map(str::to_owned), detail: detail.into() }
}

fn limit(name: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}

include!("docx/tests.rs");
include!("docx/fixture_tests.rs");

//! Authoritative inventory and assembly of the offline core conversion surface.

use super::{
    ContentFormatDetector, DelimitedTextConverter, DocxConverter, EpubConverter, FeedConverter,
    HintFormatDetector, HtmlConverter, HttpSourceResolver, ImageConverter, LegacyOfficeConverter,
    LocalFileSourceResolver, MarkdownConverter, MemorySourceResolver, MsgConverter,
    NotebookConverter, OdfConverter, PdfConverter, PresentationConverter, RtfConverter,
    StdinSourceResolver, StructuredDataConverter, TextConverter, WorkbookConverter, ZipConverter,
};
use into_markdown_core::{ConversionError, Converter, FormatDetector, InputFormat, SourceResolver};
use into_markdown_engine::RegistryBuilder;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Provenance boundary for a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilitySource {
    /// Shipped and statically assembled by the core package.
    Core,
    /// Separately installed native runtime or model bundle.
    OptionalRuntime,
    /// Explicitly installed plugin package.
    Plugin,
}

impl CapabilitySource {
    /// Stable machine-readable value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::OptionalRuntime => "optional_runtime",
            Self::Plugin => "plugin",
        }
    }
}

/// Component role in the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityKind {
    /// Authenticated input acquisition.
    SourceResolver,
    /// Bounded format detection.
    FormatDetector,
    /// Input-to-IR conversion.
    Converter,
    /// Separately distributed native/model dependency.
    Runtime,
}

impl CapabilityKind {
    /// Stable machine-readable value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceResolver => "source_resolver",
            Self::FormatDetector => "format_detector",
            Self::Converter => "converter",
            Self::Runtime => "runtime",
        }
    }
}

/// Whether a catalog entry is self-contained or supplied separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityAvailability {
    /// Present in the core package.
    Available,
    /// Installed and verified separately.
    OptionalRuntime,
}

impl CapabilityAvailability {
    /// Stable machine-readable value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::OptionalRuntime => "optional_runtime",
        }
    }
}

/// Stable installation guidance for a separately distributed runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeRequirement {
    /// Stable component name used by conversion errors.
    pub component: &'static str,
    /// Stable actionable installation guidance.
    pub install_hint: &'static str,
}

/// One registered component or optional runtime in the core inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityDescriptor {
    /// Stable globally unique component ID.
    pub id: &'static str,
    /// Component role.
    pub kind: CapabilityKind,
    /// Package provenance boundary.
    pub source: CapabilitySource,
    /// Static distribution availability.
    pub availability: CapabilityAvailability,
    /// Deterministic detector/converter precedence, or zero when inapplicable.
    pub priority: i32,
    /// Formats routed by this converter.
    pub formats: &'static [InputFormat],
    /// Separately installed dependency, when required.
    pub runtime: Option<RuntimeRequirement>,
}

/// Converter implementation status exposed by `into-md formats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatStatus {
    /// A real converter is registered in the default engine.
    Available,
    /// Compatibility value for extension catalogs; never emitted by the core catalog.
    Planned,
}

impl FormatStatus {
    /// Stable lowercase display value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Planned => "planned",
        }
    }
}

/// User-facing core format descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatDescriptor {
    /// Stable format identifier.
    pub format: InputFormat,
    /// User-facing format family.
    pub family: &'static str,
    /// Recognized filename extensions.
    pub extensions: &'static [&'static str],
    /// Converter registration status.
    pub status: FormatStatus,
    /// Package provenance boundary.
    pub source: CapabilitySource,
    /// Separately installed dependency, when required.
    pub runtime: Option<RuntimeRequirement>,
}

pub(crate) const PDFIUM: RuntimeRequirement = RuntimeRequirement {
    component: "pdfium",
    install_hint: "install the pinned PDFium runtime and set PDFIUM_LIBRARY to its exact file",
};
pub(crate) const LEGACY_OFFICE: RuntimeRequirement = RuntimeRequirement {
    component: "legacy-office",
    install_hint: "install the authority-verified legacy Office runtime for this platform",
};
const OCR: RuntimeRequirement = RuntimeRequirement {
    component: "onnxruntime",
    install_hint: "run `into-md models install pp-ocrv6-tiny-zh-en` and configure the pinned ONNX Runtime worker",
};

const AVAILABLE: FormatStatus = FormatStatus::Available;
const CORE: CapabilitySource = CapabilitySource::Core;

const FORMATS: &[FormatDescriptor] = &[
    format(InputFormat::Pdf, "document", &["pdf"], Some(PDFIUM)),
    format(InputFormat::Doc, "document", &["doc"], Some(LEGACY_OFFICE)),
    format(InputFormat::Docx, "document", &["docx", "docm"], None),
    format(InputFormat::Ppt, "document", &["ppt", "pps", "pot"], Some(LEGACY_OFFICE)),
    format(InputFormat::Pptx, "document", &["pptx", "pptm", "ppsx", "ppsm", "potx"], None),
    format(InputFormat::Xls, "document", &["xls"], Some(LEGACY_OFFICE)),
    format(InputFormat::Xlsx, "document", &["xlsx", "xlsm", "xlsb"], None),
    format(InputFormat::Odt, "document", &["odt"], None),
    format(InputFormat::Ods, "document", &["ods"], None),
    format(InputFormat::Odp, "document", &["odp"], None),
    format(InputFormat::Rtf, "document", &["rtf"], None),
    format(InputFormat::Epub, "document", &["epub"], None),
    format(InputFormat::Text, "text", &["txt", "text", "log"], None),
    format(InputFormat::Markdown, "text", &["md", "markdown", "mdown"], None),
    format(InputFormat::Html, "text", &["html", "htm"], None),
    format(InputFormat::Csv, "data", &["csv"], None),
    format(InputFormat::Tsv, "data", &["tsv"], None),
    format(InputFormat::Json, "data", &["json"], None),
    format(InputFormat::Xml, "data", &["xml"], None),
    format(InputFormat::Feed, "data", &["rss", "atom"], None),
    format(InputFormat::Ipynb, "data", &["ipynb"], None),
    format(
        InputFormat::Image,
        "image",
        &["png", "jpg", "jpeg", "tif", "tiff", "webp", "bmp"],
        None,
    ),
    format(InputFormat::Zip, "container", &["zip"], None),
    format(InputFormat::OutlookMsg, "message", &["msg"], None),
];

const fn format(
    format: InputFormat,
    family: &'static str,
    extensions: &'static [&'static str],
    runtime: Option<RuntimeRequirement>,
) -> FormatDescriptor {
    FormatDescriptor { format, family, extensions, status: AVAILABLE, source: CORE, runtime }
}

const CAPABILITIES: &[CapabilityDescriptor] = &[
    component("builtin.source.memory", CapabilityKind::SourceResolver, 0, &[]),
    component("builtin.source.local-file", CapabilityKind::SourceResolver, 0, &[]),
    component("builtin.source.stdin", CapabilityKind::SourceResolver, 0, &[]),
    component("builtin.source.http", CapabilityKind::SourceResolver, 0, &[]),
    component("builtin.detector.content", CapabilityKind::FormatDetector, 200, &[]),
    component("builtin.detector.hints", CapabilityKind::FormatDetector, 100, &[]),
    component("builtin.converter.image", CapabilityKind::Converter, 260, &[InputFormat::Image]),
    component("builtin.converter.docx", CapabilityKind::Converter, 250, &[InputFormat::Docx]),
    component(
        "builtin.converter.jupyter-notebook",
        CapabilityKind::Converter,
        250,
        &[InputFormat::Ipynb],
    ),
    component("builtin.converter.msg", CapabilityKind::Converter, 250, &[InputFormat::OutlookMsg]),
    component(
        "builtin.converter.presentationml",
        CapabilityKind::Converter,
        250,
        &[InputFormat::Pptx],
    ),
    component("builtin.converter.workbook", CapabilityKind::Converter, 250, &[InputFormat::Xlsx]),
    component(
        "builtin.converter.odf",
        CapabilityKind::Converter,
        245,
        &[InputFormat::Odt, InputFormat::Ods, InputFormat::Odp],
    ),
    component("builtin.converter.epub", CapabilityKind::Converter, 240, &[InputFormat::Epub]),
    component("builtin.converter.rtf", CapabilityKind::Converter, 240, &[InputFormat::Rtf]),
    runtime_converter(
        "builtin.converter.legacy-office",
        230,
        &[InputFormat::Doc, InputFormat::Ppt, InputFormat::Xls],
        LEGACY_OFFICE,
    ),
    component("builtin.converter.feed", CapabilityKind::Converter, 220, &[InputFormat::Feed]),
    component("builtin.converter.zip", CapabilityKind::Converter, 220, &[InputFormat::Zip]),
    component("builtin.converter.html", CapabilityKind::Converter, 210, &[InputFormat::Html]),
    runtime_converter("builtin.converter.pdfium", 200, &[InputFormat::Pdf], PDFIUM),
    component(
        "builtin.converter.structured-data",
        CapabilityKind::Converter,
        200,
        &[InputFormat::Json, InputFormat::Xml],
    ),
    component(
        "builtin.converter.markdown-gfm",
        CapabilityKind::Converter,
        120,
        &[InputFormat::Markdown],
    ),
    component(
        "builtin.converter.delimited-text",
        CapabilityKind::Converter,
        110,
        &[InputFormat::Csv, InputFormat::Tsv],
    ),
    component("builtin.converter.text", CapabilityKind::Converter, 100, &[InputFormat::Text]),
    runtime("runtime.pdfium", PDFIUM),
    runtime("runtime.ocr", OCR),
    runtime("runtime.legacy-office", LEGACY_OFFICE),
];

const fn component(
    id: &'static str,
    kind: CapabilityKind,
    priority: i32,
    formats: &'static [InputFormat],
) -> CapabilityDescriptor {
    CapabilityDescriptor {
        id,
        kind,
        source: CORE,
        availability: CapabilityAvailability::Available,
        priority,
        formats,
        runtime: None,
    }
}

const fn runtime_converter(
    id: &'static str,
    priority: i32,
    formats: &'static [InputFormat],
    runtime: RuntimeRequirement,
) -> CapabilityDescriptor {
    CapabilityDescriptor {
        id,
        kind: CapabilityKind::Converter,
        source: CORE,
        availability: CapabilityAvailability::OptionalRuntime,
        priority,
        formats,
        runtime: Some(runtime),
    }
}

const fn runtime(id: &'static str, runtime: RuntimeRequirement) -> CapabilityDescriptor {
    CapabilityDescriptor {
        id,
        kind: CapabilityKind::Runtime,
        source: CapabilitySource::OptionalRuntime,
        availability: CapabilityAvailability::OptionalRuntime,
        priority: 0,
        formats: &[],
        runtime: Some(runtime),
    }
}

/// Formats actually routed by the default engine.
#[must_use]
pub const fn core_formats() -> &'static [FormatDescriptor] {
    FORMATS
}

/// Components actually shipped or consumed by the core package.
#[must_use]
pub const fn core_capabilities() -> &'static [CapabilityDescriptor] {
    CAPABILITIES
}

/// Verify the packaged legacy Office runtime without launching its worker.
///
/// # Errors
///
/// Returns a stable catalog component error and install hint when the runtime
/// is absent or fails its embedded authority.
pub fn verify_packaged_legacy_office_runtime(
    context: &into_markdown_core::ExecutionContext,
) -> Result<(), ConversionError> {
    let runtime = into_markdown_legacy_office::LegacyOfficeRuntime::packaged()
        .map_err(remap_legacy_runtime_error)?;
    runtime.verify(context).map(drop).map_err(remap_legacy_runtime_error)
}

fn remap_legacy_runtime_error(error: ConversionError) -> ConversionError {
    match error {
        ConversionError::ComponentUnavailable { component, detail } => {
            ConversionError::ComponentUnavailable {
                component: LEGACY_OFFICE.component.into(),
                detail: format!("{}; cause: {component}/{detail}", LEGACY_OFFICE.install_hint),
            }
        }
        error => error,
    }
}

/// Validate an inventory before it is used as a core or release claim.
///
/// # Errors
///
/// Fails closed on duplicate IDs, forged provenance, invalid priority, runtime
/// entries claiming availability, or missing/duplicate converter coverage.
pub fn validate_core_capabilities(
    capabilities: &[CapabilityDescriptor],
) -> Result<(), ConversionError> {
    let mut ids = BTreeSet::new();
    let mut coverage = BTreeMap::new();
    for capability in capabilities {
        if capability.id.is_empty() || !ids.insert(capability.id) {
            return catalog_error(format!("duplicate or empty capability ID: {}", capability.id));
        }
        match capability.kind {
            CapabilityKind::Runtime => {
                if capability.source != CapabilitySource::OptionalRuntime
                    || capability.availability != CapabilityAvailability::OptionalRuntime
                    || capability.priority != 0
                    || capability.runtime.is_none()
                {
                    return catalog_error(format!(
                        "runtime capability {} has a forged core status",
                        capability.id
                    ));
                }
            }
            kind => {
                if capability.source != CapabilitySource::Core {
                    return catalog_error(format!(
                        "registered capability {} is not core",
                        capability.id
                    ));
                }
                let expected_prefix = match kind {
                    CapabilityKind::SourceResolver => "builtin.source.",
                    CapabilityKind::FormatDetector => "builtin.detector.",
                    CapabilityKind::Converter => "builtin.converter.",
                    CapabilityKind::Runtime => unreachable!(),
                };
                if !capability.id.starts_with(expected_prefix) {
                    return catalog_error(format!(
                        "capability {} has a forged kind",
                        capability.id
                    ));
                }
                let priority_valid = match kind {
                    CapabilityKind::SourceResolver => capability.priority == 0,
                    CapabilityKind::FormatDetector | CapabilityKind::Converter => {
                        (1..=999).contains(&capability.priority)
                    }
                    CapabilityKind::Runtime => false,
                };
                if !priority_valid {
                    return catalog_error(format!(
                        "capability {} has invalid priority {}",
                        capability.id, capability.priority
                    ));
                }
                if capability.availability == CapabilityAvailability::OptionalRuntime
                    && capability.runtime.is_none()
                {
                    return catalog_error(format!(
                        "capability {} omits its runtime requirement",
                        capability.id
                    ));
                }
                if capability.availability == CapabilityAvailability::Available
                    && capability.runtime.is_some()
                {
                    return catalog_error(format!(
                        "capability {} claims available while requiring a runtime",
                        capability.id
                    ));
                }
            }
        }
        if capability.kind == CapabilityKind::Converter {
            for format in capability.formats {
                if coverage.insert(*format, capability.id).is_some() {
                    return catalog_error(format!(
                        "core format {format} has duplicate converter coverage"
                    ));
                }
            }
        }
    }
    validate_format_coverage(capabilities, &coverage)?;
    if capabilities != CAPABILITIES {
        return catalog_error("capability inventory differs from the authoritative core catalog");
    }
    Ok(())
}

fn validate_format_coverage(
    capabilities: &[CapabilityDescriptor],
    coverage: &BTreeMap<InputFormat, &str>,
) -> Result<(), ConversionError> {
    for descriptor in FORMATS {
        if descriptor.source != CapabilitySource::Core
            || descriptor.status != FormatStatus::Available
        {
            return catalog_error(format!(
                "core format {} has a forged availability claim",
                descriptor.format
            ));
        }
        if !coverage.contains_key(&descriptor.format) {
            return catalog_error(format!("core format {} has no converter", descriptor.format));
        }
        let converter = capabilities
            .iter()
            .find(|capability| coverage.get(&descriptor.format) == Some(&capability.id))
            .ok_or_else(|| ConversionError::Internal {
                detail: format!(
                    "invalid core capability catalog: core format {} lost its converter",
                    descriptor.format
                ),
            })?;
        if converter.runtime != descriptor.runtime {
            return catalog_error(format!(
                "core format {} runtime requirement drifted",
                descriptor.format
            ));
        }
    }
    if coverage.len() != FORMATS.len() {
        return catalog_error("converter inventory exposes a non-core format");
    }
    Ok(())
}

/// Register the validated core inventory in stable catalog order.
///
/// # Errors
///
/// Returns an internal error if a concrete component drifts from the catalog.
pub fn register_core_components(registry: &mut RegistryBuilder) -> Result<(), ConversionError> {
    validate_core_capabilities(CAPABILITIES)?;
    for capability in CAPABILITIES {
        match (capability.kind, capability.id) {
            (CapabilityKind::SourceResolver, "builtin.source.memory") => {
                register_source(registry, Arc::new(MemorySourceResolver))?;
            }
            (CapabilityKind::SourceResolver, "builtin.source.local-file") => {
                register_source(registry, Arc::new(LocalFileSourceResolver))?;
            }
            (CapabilityKind::SourceResolver, "builtin.source.stdin") => {
                register_source(registry, Arc::new(StdinSourceResolver))?;
            }
            (CapabilityKind::SourceResolver, "builtin.source.http") => {
                register_source(registry, Arc::new(HttpSourceResolver::default()))?;
            }
            (CapabilityKind::FormatDetector, "builtin.detector.content") => {
                register_detector(registry, Arc::new(ContentFormatDetector))?;
            }
            (CapabilityKind::FormatDetector, "builtin.detector.hints") => {
                register_detector(registry, Arc::new(HintFormatDetector))?;
            }
            (CapabilityKind::Converter, id) => register_converter_by_id(registry, id)?,
            (CapabilityKind::Runtime, _) => {}
            (_, id) => return catalog_error(format!("no core factory for {id}")),
        }
    }
    Ok(())
}

fn register_converter_by_id(
    registry: &mut RegistryBuilder,
    id: &str,
) -> Result<(), ConversionError> {
    let converter: Arc<dyn Converter> = match id {
        "builtin.converter.image" => Arc::new(ImageConverter),
        "builtin.converter.docx" => Arc::new(DocxConverter),
        "builtin.converter.jupyter-notebook" => Arc::new(NotebookConverter),
        "builtin.converter.msg" => Arc::new(MsgConverter),
        "builtin.converter.presentationml" => Arc::new(PresentationConverter),
        "builtin.converter.workbook" => Arc::new(WorkbookConverter),
        "builtin.converter.odf" => Arc::new(OdfConverter),
        "builtin.converter.epub" => Arc::new(EpubConverter),
        "builtin.converter.rtf" => Arc::new(RtfConverter),
        "builtin.converter.legacy-office" => Arc::new(LegacyOfficeConverter::default()),
        "builtin.converter.feed" => Arc::new(FeedConverter),
        "builtin.converter.zip" => Arc::new(ZipConverter),
        "builtin.converter.html" => Arc::new(HtmlConverter),
        "builtin.converter.pdfium" => Arc::new(PdfConverter::default()),
        "builtin.converter.structured-data" => Arc::new(StructuredDataConverter),
        "builtin.converter.markdown-gfm" => Arc::new(MarkdownConverter),
        "builtin.converter.delimited-text" => Arc::new(DelimitedTextConverter),
        "builtin.converter.text" => Arc::new(TextConverter),
        _ => return catalog_error(format!("no core converter factory for {id}")),
    };
    register_converter(registry, converter)
}

fn descriptor(
    id: &str,
    kind: CapabilityKind,
) -> Result<&'static CapabilityDescriptor, ConversionError> {
    CAPABILITIES.iter().find(|value| value.id == id && value.kind == kind).ok_or_else(|| {
        ConversionError::Internal {
            detail: format!("component {id} is absent from the core catalog"),
        }
    })
}

fn register_source(
    registry: &mut RegistryBuilder,
    source: Arc<dyn SourceResolver>,
) -> Result<(), ConversionError> {
    let entry = descriptor(source.id(), CapabilityKind::SourceResolver)?;
    if entry.priority != source.priority() || !entry.formats.is_empty() {
        return catalog_error(format!("source {} drifted from the core catalog", source.id()));
    }
    registry.register_source_resolver(source);
    Ok(())
}

fn register_detector(
    registry: &mut RegistryBuilder,
    detector: Arc<dyn FormatDetector>,
) -> Result<(), ConversionError> {
    let entry = descriptor(detector.id(), CapabilityKind::FormatDetector)?;
    if entry.priority != detector.priority() {
        return catalog_error(format!(
            "detector {} priority drifted from the core catalog",
            detector.id()
        ));
    }
    registry.register_format_detector(detector);
    Ok(())
}

fn register_converter(
    registry: &mut RegistryBuilder,
    converter: Arc<dyn Converter>,
) -> Result<(), ConversionError> {
    let entry = descriptor(converter.id(), CapabilityKind::Converter)?;
    if entry.priority != converter.priority() || entry.formats != converter.supported_formats() {
        return catalog_error(format!(
            "converter {} drifted from the core catalog",
            converter.id()
        ));
    }
    registry.register_converter(converter);
    Ok(())
}

fn catalog_error(detail: impl Into<String>) -> Result<(), ConversionError> {
    let detail = detail.into();
    Err(ConversionError::Internal { detail: format!("invalid core capability catalog: {detail}") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authoritative_catalog_registers_without_drift() {
        let mut registry = RegistryBuilder::new();
        register_core_components(&mut registry).unwrap();
    }

    #[test]
    fn malformed_catalog_claims_fail_closed() {
        let mut forged = CAPABILITIES.to_vec();
        forged[0].source = CapabilitySource::Plugin;
        assert!(validate_core_capabilities(&forged).is_err());

        let mut duplicate = CAPABILITIES.to_vec();
        duplicate[1].id = duplicate[0].id;
        assert!(validate_core_capabilities(&duplicate).is_err());

        let mut priority = CAPABILITIES.to_vec();
        priority[4].priority = 201;
        assert!(validate_core_capabilities(&priority).is_err());

        let mut runtime = CAPABILITIES.to_vec();
        let index = runtime.iter().position(|entry| entry.kind == CapabilityKind::Runtime).unwrap();
        runtime[index].availability = CapabilityAvailability::Available;
        assert!(validate_core_capabilities(&runtime).is_err());
    }

    #[test]
    fn plugins_and_media_are_absent_from_core_release_inventory() {
        assert!(FORMATS.iter().all(|entry| !matches!(
            entry.format,
            InputFormat::Audio | InputFormat::Video | InputFormat::YouTube | InputFormat::Wikipedia
        )));
        assert!(CAPABILITIES.iter().all(
            |entry| !entry.id.contains("mediawiki") && entry.source != CapabilitySource::Plugin
        ));
    }
}

//! Security-hardened, offline EPUB 2/3 conversion.

mod budget;
mod container;
mod encryption;
pub(crate) mod image;
#[cfg(test)]
mod image_tests;
mod merge;
mod navigation;
mod package;
mod path;
mod reachability;
mod resources;
mod spine;
mod xhtml;
mod xml;

#[cfg(test)]
mod tests;

use crate::zip_converter::archive_api::{OwnedEntry, SafeArchive};
use budget::EpubBudget;
use encryption::EncryptionPolicy;
use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput, Diagnostic,
    DiagnosticSeverity, ErrorPolicy, ExecutionContext, FormatCandidate, InputFormat, ProbeOutcome,
    ResolvedInput, Services, SourceLocator,
};
use navigation::Navigation;
use resources::ResourceStore;

const FORMATS: &[InputFormat] = &[InputFormat::Epub];
const MIMETYPE: &[u8] = b"application/epub+zip";
const CONTAINER_PATH: &str = "META-INF/container.xml";
const ENCRYPTION_PATH: &str = "META-INF/encryption.xml";
const RIGHTS_PATH: &str = "META-INF/rights.xml";

/// Security-hardened EPUB 2/3 converter.
#[derive(Debug, Default)]
pub struct EpubConverter;

impl Converter for EpubConverter {
    fn id(&self) -> &'static str {
        "builtin.converter.epub"
    }

    fn priority(&self) -> i32 {
        240
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        FORMATS
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

    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            if candidate.format != InputFormat::Epub {
                return Ok(ProbeOutcome::NotApplicable);
            }
            Ok(if input.bytes.starts_with(b"PK\x03\x04") {
                ProbeOutcome::Match { confidence: 1.0 }
            } else {
                ProbeOutcome::NotApplicable
            })
        })
    }

    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        services: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move { convert_epub(&input.bytes, options, services, context).await })
    }
}

async fn convert_epub(
    bytes: &[u8],
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let mut archive = SafeArchive::open(bytes, options, context)?;
    let mimetype_diagnostic = validate_mimetype(&mut archive, options.error_policy)?;
    if !archive.contains(CONTAINER_PATH) {
        return Err(malformed(CONTAINER_PATH, "OCF container document is missing"));
    }
    let mut budget = EpubBudget::new(options, context);
    let container = archive.read(CONTAINER_PATH)?;
    let package_path = container::rootfile(&container.bytes, &mut budget)?;
    drop(container);
    if !archive.contains(&package_path) {
        return Err(malformed(&package_path, "package rootfile is missing"));
    }
    let package_entry = archive.read(&package_path)?;
    let mut package = package::parse(
        &package_path,
        &package_entry.bytes,
        &archive,
        &mut budget,
        options.error_policy,
    )?;
    drop(package_entry);
    if let Some(diagnostic) = mimetype_diagnostic {
        package.diagnostics.push(diagnostic);
    }

    let encryption = if archive.contains(ENCRYPTION_PATH) {
        let entry = archive.read(ENCRYPTION_PATH)?;
        let policy = encryption::parse(&entry.bytes, &package, &mut budget)?;
        drop(entry);
        policy
    } else {
        EncryptionPolicy::default()
    };
    let rights_metadata = archive.contains(RIGHTS_PATH);
    let NavigationRead { navigation, chapter_entry } =
        read_navigation(&mut package, &mut archive, &mut budget, options.error_policy)?;
    let mut spine = spine::convert(
        &package,
        &mut archive,
        chapter_entry,
        options,
        services,
        &mut budget,
        context,
    )
    .await?;
    let omitted = reachability::omitted_resources(
        &package,
        navigation.as_ref(),
        &spine,
        &mut archive,
        context,
    )?;
    let mut resources = ResourceStore::new(options);
    for chapter in &mut spine.chapters {
        resources.bind_chapter_images(
            &mut chapter.output,
            &chapter.references,
            &package,
            &mut archive,
            context,
        )?;
    }
    let cover = resources.cover(&package, &mut archive, context)?;
    archive.validate_remaining()?;
    merge::assemble(
        package,
        navigation,
        spine,
        resources,
        cover,
        encryption,
        rights_metadata,
        omitted,
        context,
    )
}

fn validate_mimetype(
    archive: &mut SafeArchive<'_, '_>,
    error_policy: ErrorPolicy,
) -> Result<Option<Diagnostic>, ConversionError> {
    let canonical_layout = {
        let first = archive
            .first_physical_entry()
            .ok_or_else(|| malformed("mimetype", "EPUB archive is empty"))?;
        let mimetype = archive
            .info("mimetype")
            .ok_or_else(|| malformed("mimetype", "EPUB mimetype entry is missing"))?;
        if mimetype.directory {
            return Err(malformed("mimetype", "mimetype must be a file"));
        }
        first.path == "mimetype"
            && mimetype.stored
            && mimetype.physical_start == 0
            && mimetype.central_extra_len == 0
            && mimetype.local_extra_len == 0
            && mimetype.expanded_size == u64::try_from(MIMETYPE.len()).unwrap_or(u64::MAX)
            && mimetype.compressed_size == mimetype.expanded_size
    };
    let entry = archive.read("mimetype")?;
    let canonical_content = entry.bytes == MIMETYPE;
    let crlf_content = entry.bytes.strip_suffix(b"\r\n") == Some(MIMETYPE);
    if !canonical_content && !crlf_content {
        return Err(malformed("mimetype", "mimetype content is not application/epub+zip"));
    }
    if canonical_layout && canonical_content {
        return Ok(None);
    }
    if error_policy == ErrorPolicy::Strict {
        return Err(malformed(
            "mimetype",
            "mimetype must be the physical first entry, stored, have no extra fields, and use the exact length",
        ));
    }
    Ok(Some(Diagnostic {
        code: "epub.mimetypeLayoutRecovered".into(),
        severity: DiagnosticSeverity::Info,
        message: if crlf_content {
            "accepted an EPUB mimetype entry with a non-canonical trailing CRLF".into()
        } else {
            "accepted a valid EPUB mimetype entry with a non-canonical ZIP layout".into()
        },
        locator: Some(SourceLocator { part: Some("mimetype".into()), ..SourceLocator::default() }),
    }))
}

fn read_navigation(
    package: &mut package::Package,
    archive: &mut SafeArchive<'_, '_>,
    budget: &mut EpubBudget<'_>,
    error_policy: ErrorPolicy,
) -> Result<NavigationRead, ConversionError> {
    let version =
        package.metadata.properties.get("epub.version").map(String::as_str).unwrap_or_default();
    if version.starts_with('3') {
        if let Some(id) = package.nav_id.as_deref() {
            let item = package.item(id)?;
            if item.media_type != "application/xhtml+xml" {
                return Err(malformed(&item.path, "EPUB navigation item is not XHTML"));
            }
            let path = item.path.clone();
            let entry = archive.read(&path)?;
            let navigation =
                navigation::parse_nav(&path, &entry.bytes, archive, budget, error_policy)?;
            if navigation.entries.is_empty() {
                package.diagnostics.push(Diagnostic {
                    code: "epub.navigationOmitted".into(),
                    severity: DiagnosticSeverity::Info,
                    message: "an empty EPUB navigation document was omitted".into(),
                    locator: Some(SourceLocator {
                        part: Some(path.clone()),
                        ..SourceLocator::default()
                    }),
                });
                return Ok(NavigationRead { navigation: None, chapter_entry: Some((path, entry)) });
            }
            return Ok(NavigationRead {
                navigation: Some(navigation),
                chapter_entry: Some((path, entry)),
            });
        }
        return Err(malformed(&package.path, "EPUB 3 navigation document is missing"));
    }
    if version == "2.0" {
        if let Some(id) = package.ncx_id.as_deref() {
            let item = package.item(id)?;
            if item.media_type != "application/x-dtbncx+xml" {
                return Err(malformed(&item.path, "EPUB NCX item has the wrong media type"));
            }
            let entry = archive.read(&item.path)?;
            return navigation::parse_ncx(&item.path, &entry.bytes, archive, budget).map(
                |navigation| NavigationRead { navigation: Some(navigation), chapter_entry: None },
            );
        }
        return Err(malformed(&package.path, "EPUB 2 NCX navigation document is missing"));
    }
    Err(malformed(&package.path, "unsupported EPUB package version"))
}

struct NavigationRead {
    navigation: Option<Navigation>,
    chapter_entry: Option<(String, OwnedEntry)>,
}

fn malformed(part: &str, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some(part.into()), detail: detail.into() }
}

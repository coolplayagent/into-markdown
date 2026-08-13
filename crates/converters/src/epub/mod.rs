//! Strict, offline EPUB 2/3 conversion.

mod budget;
mod container;
mod encryption;
mod merge;
mod navigation;
mod package;
mod path;
mod resources;
mod spine;
mod xhtml;
mod xml;

#[cfg(test)]
mod tests;

use crate::zip_converter::archive_api::SafeArchive;
use budget::EpubBudget;
use encryption::EncryptionPolicy;
use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput, ExecutionContext,
    FormatCandidate, InputFormat, ProbeOutcome, ResolvedInput, Services,
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
    validate_mimetype(&mut archive)?;
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
    let package = package::parse(&package_path, &package_entry.bytes, &archive, &mut budget)?;
    drop(package_entry);

    let encryption = if archive.contains(ENCRYPTION_PATH) {
        let entry = archive.read(ENCRYPTION_PATH)?;
        let policy = encryption::parse(&entry.bytes, &package, &mut budget)?;
        drop(entry);
        policy
    } else {
        EncryptionPolicy::default()
    };
    let rights_metadata = archive.contains(RIGHTS_PATH);
    let navigation = read_navigation(&package, &mut archive, &mut budget)?;
    let mut spine =
        spine::convert(&package, &mut archive, options, services, &mut budget, context).await?;
    let mut resources = ResourceStore::new(options);
    for chapter in &mut spine.chapters {
        resources.bind_chapter_images(
            &mut chapter.output,
            &chapter.references,
            &package,
            &mut archive,
        )?;
    }
    let cover = resources.cover(&package, &mut archive)?;
    merge::assemble(
        package,
        navigation,
        spine,
        resources,
        cover,
        encryption,
        rights_metadata,
        context,
    )
}

fn validate_mimetype(archive: &mut SafeArchive<'_, '_>) -> Result<(), ConversionError> {
    let first = archive
        .first_physical_entry()
        .ok_or_else(|| malformed("mimetype", "EPUB archive is empty"))?;
    if first.path != "mimetype"
        || first.directory
        || !first.stored
        || first.physical_start != 0
        || first.central_extra_len != 0
        || first.local_extra_len != 0
        || first.expanded_size != u64::try_from(MIMETYPE.len()).unwrap_or(u64::MAX)
        || first.compressed_size != first.expanded_size
    {
        return Err(malformed(
            "mimetype",
            "mimetype must be the physical first entry, stored, have no extra fields, and use the exact length",
        ));
    }
    let entry = archive.read("mimetype")?;
    if entry.bytes != MIMETYPE {
        return Err(malformed("mimetype", "mimetype content is not application/epub+zip"));
    }
    Ok(())
}

fn read_navigation(
    package: &package::Package,
    archive: &mut SafeArchive<'_, '_>,
    budget: &mut EpubBudget<'_>,
) -> Result<Option<Navigation>, ConversionError> {
    let version =
        package.metadata.properties.get("epub.version").map(String::as_str).unwrap_or_default();
    if version.starts_with('3') {
        if let Some(id) = package.nav_id.as_deref() {
            let item = package.item(id)?;
            if item.media_type != "application/xhtml+xml" {
                return Err(malformed(&item.path, "EPUB navigation item is not XHTML"));
            }
            let entry = archive.read(&item.path)?;
            return navigation::parse_nav(&item.path, &entry.bytes, archive, budget).map(Some);
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
            return navigation::parse_ncx(&item.path, &entry.bytes, archive, budget).map(Some);
        }
        return Err(malformed(&package.path, "EPUB 2 NCX navigation document is missing"));
    }
    Err(malformed(&package.path, "unsupported EPUB package version"))
}

fn malformed(part: &str, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some(part.into()), detail: detail.into() }
}

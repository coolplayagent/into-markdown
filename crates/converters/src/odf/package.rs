use crate::odf::image_validation::{image_profile, validate_image};
use crate::odf::manifest::{ManifestEntry, parse_manifest};
use crate::odf::model::{ZIP_STREAM_CHUNK, limit, malformed};
use crate::odf::paths::canonical_part_name;
use crate::odf::raw_zip::{
    bind_mimetype_central, conservative_vec_capacity, package_index_peak, package_logical_peak,
    reachable_image_peak, validate_raw_mimetype_central, validate_zip_directory_layout,
    validated_local_zip_name,
};
use crate::odf::xml::parse_xml;
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext, InputFormat};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

#[cfg(test)]
use std::cell::Cell as CounterCell;

#[cfg(test)]
std::thread_local! {
    pub(super) static REACHABLE_IMAGE_ALLOCATION_ATTEMPTS: CounterCell<usize> = const { CounterCell::new(0) };
}

#[derive(Clone, Copy, Debug)]
struct ZipPart {
    index: usize,
    size: u64,
    directory: bool,
}

#[derive(Debug)]
pub(super) struct Package {
    pub(super) parts: BTreeMap<String, Vec<u8>>,
    pub(super) manifest: BTreeMap<String, ManifestEntry>,
    indexes: BTreeMap<String, ZipPart>,
    pub(super) logical_peak: u64,
    preflight: u64,
    pub(super) odf_version: String,
    pub(super) missing_optional_parts: Vec<String>,
    pub(super) noncanonical_mimetype: bool,
}

impl Package {
    #[allow(clippy::too_many_lines)]
    pub(super) fn open(
        bytes: &[u8],
        format: InputFormat,
        options: &ConversionOptions,
        context: &ExecutionContext,
        planned: u64,
    ) -> Result<Self, ConversionError> {
        let input_bytes = u64::try_from(bytes.len())
            .map_err(|_| limit("max_input_bytes", "ODF input size overflow"))?;
        if input_bytes > options.limits.max_input_bytes {
            return Err(limit(
                "max_input_bytes",
                format!("{input_bytes} > {}", options.limits.max_input_bytes),
            ));
        }
        let expected = media_type_for(format)?;
        let local_mimetype = super::raw_zip::mimetype_header_for_policy(bytes, expected, options)?;
        if let Some(local) = &local_mimetype {
            validate_raw_mimetype_central(bytes, local)?;
        }
        validate_zip_directory_layout(bytes, options.limits.max_archive_entries, planned, context)?;
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|error| malformed(None, format!("invalid ODF ZIP container: {error}")))?;
        let count = u32::try_from(archive.len()).unwrap_or(u32::MAX);
        if count > options.limits.max_archive_entries {
            return Err(limit(
                "max_archive_entries",
                format!("{count} > {}", options.limits.max_archive_entries),
            ));
        }
        // BTreeMap cannot reserve. Authenticate a conservative per-index working plan before its
        // first node allocation; the raw central-layout Vec was independently checked earlier.
        let index_peak = package_index_peak(u64::from(count))?;
        if index_peak > planned {
            return Err(limit(
                "max_memory_bytes",
                format!("ODF ZIP index plan {index_peak} > preflight {planned}"),
            ));
        }
        let mut indexes = BTreeMap::new();
        let mut total = 0_u64;
        for index in 0..archive.len() {
            if index % 256 == 0 {
                context.checkpoint()?;
            }
            let entry = archive.by_index_raw(index).map_err(|error| {
                malformed(None, format!("cannot inspect ZIP entry {index}: {error}"))
            })?;
            if entry.encrypted() {
                return Err(ConversionError::Encrypted);
            }
            if entry.is_symlink() {
                return Err(malformed(Some(entry.name()), "symbolic links are forbidden in ODF"));
            }
            let raw_name = validated_local_zip_name(bytes, entry.header_start())?;
            if entry.name_raw() != raw_name.as_bytes() || entry.name() != raw_name {
                return Err(malformed(
                    Some(raw_name),
                    "ZIP library name differs from the authenticated raw UTF-8 name",
                ));
            }
            let name = canonical_part_name(raw_name, entry.is_dir())?;
            let part = ZipPart { index, size: entry.size(), directory: entry.is_dir() };
            if indexes.insert(name.clone(), part).is_some() {
                return Err(malformed(Some(&name), "duplicate ZIP part name"));
            }
            total = total
                .checked_add(entry.size())
                .ok_or_else(|| limit("max_decompressed_bytes", "ODF expanded size overflow"))?;
            if total > options.limits.max_decompressed_bytes {
                return Err(limit(
                    "max_decompressed_bytes",
                    format!("{total} > {}", options.limits.max_decompressed_bytes),
                ));
            }
        }
        let mime_part = indexes.get("mimetype").copied().ok_or_else(|| {
            malformed(Some("mimetype"), "required uncompressed mimetype part is missing")
        })?;
        if (local_mimetype.is_some() && mime_part.index != 0) || mime_part.directory {
            return Err(malformed(
                Some("mimetype"),
                "mimetype must be the first non-directory package entry",
            ));
        }
        let mime_entry = archive.by_index_raw(mime_part.index).map_err(|error| {
            malformed(Some("mimetype"), format!("cannot inspect mimetype: {error}"))
        })?;
        if let Some(local) = &local_mimetype {
            bind_mimetype_central(bytes, &mime_entry, local)?;
        } else {
            super::raw_zip::validate_relaxed_mimetype_extras(bytes, &mime_entry)?;
        }
        drop(mime_entry);
        if mime_part.size > 256 {
            return Err(malformed(Some("mimetype"), "mimetype entry is too large"));
        }
        let mimetype =
            read_entry(&mut archive, mime_part.index, "mimetype", mime_part.size, context)?;
        if mimetype != expected.as_bytes() {
            return Err(malformed(Some("mimetype"), format!("expected exact {expected}")));
        }
        let manifest_part = indexes.get("META-INF/manifest.xml").copied().ok_or_else(|| {
            malformed(Some("META-INF/manifest.xml"), "required manifest is missing")
        })?;
        if manifest_part.directory {
            return Err(malformed(Some("META-INF/manifest.xml"), "manifest is a directory"));
        }
        let core_expanded = ["content.xml", "styles.xml", "meta.xml", "settings.xml"]
            .iter()
            .filter_map(|name| indexes.get(*name))
            .try_fold(0_u64, |total, part| total.checked_add(part.size))
            .and_then(|total| total.checked_add(manifest_part.size))
            .ok_or_else(|| limit("max_memory_bytes", "ODF core working size overflow"))?;
        let metadata_bytes = indexes
            .keys()
            .try_fold(0_u64, |total, name| {
                total.checked_add(
                    u64::try_from(name.capacity()).unwrap_or(u64::MAX).saturating_mul(2),
                )
            })
            .ok_or_else(|| limit("max_memory_bytes", "ODF ZIP metadata plan overflow"))?;
        let logical_peak = package_logical_peak(core_expanded, metadata_bytes, u64::from(count))?;
        if logical_peak > planned {
            return Err(limit(
                "max_memory_bytes",
                format!(
                    "ODF reachable package/XML working plan {logical_peak} > preflight {planned}"
                ),
            ));
        }
        let manifest_bytes = read_entry(
            &mut archive,
            manifest_part.index,
            "META-INF/manifest.xml",
            manifest_part.size,
            context,
        )?;
        let manifest_root = parse_xml(&manifest_bytes, "META-INF/manifest.xml", options, context)?;
        let (manifest, odf_version) = parse_manifest(&manifest_root, expected)?;
        for (name, part) in &indexes {
            if !part.directory
                && !matches!(name.as_str(), "mimetype" | "META-INF/manifest.xml")
                && !manifest.contains_key(name)
            {
                return Err(malformed(Some(name), "ZIP part is not declared in ODF manifest"));
            }
        }
        let mut missing_optional_parts = Vec::new();
        for name in manifest.keys().filter(|name| name.as_str() != "/") {
            // Manifest directories describe subdocuments/configuration trees; ZIP does not
            // require a physical directory record, including for an empty directory.
            if name != "META-INF/manifest.xml"
                && !name.ends_with('/')
                && !indexes.contains_key(name)
            {
                if name == "meta.xml"
                    || manifest
                        .get(name)
                        .is_some_and(|entry| entry.media_type.starts_with("image/"))
                    || manifest.iter().any(|(parent, entry)| {
                        parent.ends_with('/')
                            && entry.media_type == "application/vnd.sun.xml.ui.configuration"
                            && name.starts_with(parent)
                    })
                {
                    super::recovery::require_best_effort(
                        options,
                        name,
                        "missing optional metadata, unreferenced image or UI configuration member",
                    )?;
                    missing_optional_parts.push(name.clone());
                    continue;
                }
                return Err(malformed(Some(name), "manifest-declared ODF part is missing"));
            }
        }
        for required in ["content.xml"] {
            if !indexes.contains_key(required) || !manifest.contains_key(required) {
                return Err(malformed(
                    Some(required),
                    "required ODF part is missing or undeclared",
                ));
            }
        }
        validate_package_graph(&indexes, &manifest)?;
        let mut parts = BTreeMap::new();
        for name in ["content.xml", "styles.xml", "meta.xml", "settings.xml"] {
            if let Some(part) = indexes.get(name).copied() {
                if part.directory {
                    return Err(malformed(Some(name), "required XML part is a directory"));
                }
                let bytes = read_entry(&mut archive, part.index, name, part.size, context)?;
                if !manifest.contains_key(name) {
                    return Err(malformed(
                        Some(name),
                        "consumed ODF part is not declared in manifest",
                    ));
                }
                if manifest.get(name).map(|entry| entry.media_type.as_str()) != Some("text/xml") {
                    return Err(malformed(
                        Some(name),
                        "ODF core XML part must have manifest media type text/xml",
                    ));
                }
                parts.insert(name.to_owned(), bytes);
            }
        }
        // CRC and expanded-size validation is mandatory for every accepted real file, including
        // unreferenced images. Validation streams into a fixed buffer and does not add the part's
        // expanded size to the retained/working-set plan.
        for (name, part) in &indexes {
            if matches!(
                name.as_str(),
                "mimetype"
                    | "META-INF/manifest.xml"
                    | "content.xml"
                    | "styles.xml"
                    | "meta.xml"
                    | "settings.xml"
            ) {
                continue;
            }
            validate_entry_stream(&mut archive, part.index, name, part.size, context)?;
        }
        Ok(Self {
            parts,
            manifest,
            indexes,
            logical_peak,
            preflight: planned,
            odf_version,
            missing_optional_parts,
            noncanonical_mimetype: local_mimetype.is_none(),
        })
    }

    pub(super) fn load_reachable_images(
        &mut self,
        source: &[u8],
        anchors: &BTreeSet<String>,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        let mut archive = zip::ZipArchive::new(Cursor::new(source))
            .map_err(|error| malformed(None, format!("invalid ODF ZIP container: {error}")))?;
        let base_peak = self.logical_peak;
        let mut reachable_capacity = 0_u64;
        let mut planned_reachable_capacity = 0_u64;
        for path in anchors {
            context.checkpoint()?;
            let part = self
                .indexes
                .get(path)
                .copied()
                .ok_or_else(|| malformed(Some(path), "referenced image has no ZIP part"))?;
            if self.skip_unsupported_image(path, options)? {
                continue;
            }
            if part.directory {
                return Err(malformed(Some(path), "referenced image is a directory"));
            }
            if part.size > options.limits.max_asset_bytes {
                return Err(limit(
                    "max_asset_bytes",
                    format!("{path}: {} > {}", part.size, options.limits.max_asset_bytes),
                ));
            }
            let planned_capacity = conservative_vec_capacity(part.size)?;
            planned_reachable_capacity =
                planned_reachable_capacity
                    .checked_add(planned_capacity)
                    .ok_or_else(|| limit("max_memory_bytes", "reachable image bytes overflow"))?;
            let declared_peak = reachable_image_peak(base_peak, planned_reachable_capacity)?;
            if declared_peak > self.preflight {
                return Err(limit(
                    "max_memory_bytes",
                    format!(
                        "ODF declared reachable image plan {declared_peak} > preflight {}",
                        self.preflight
                    ),
                ));
            }
            #[cfg(test)]
            REACHABLE_IMAGE_ALLOCATION_ATTEMPTS
                .with(|count| count.set(count.get().saturating_add(1)));
            let bytes = read_entry(&mut archive, part.index, path, part.size, context)?;
            let allocated = u64::try_from(bytes.capacity())
                .map_err(|_| limit("max_memory_bytes", "image capacity cannot be represented"))?;
            reachable_capacity = reachable_capacity
                .checked_add(allocated)
                .ok_or_else(|| limit("max_memory_bytes", "reachable image bytes overflow"))?;
            let actual_peak = reachable_image_peak(base_peak, reachable_capacity)?;
            if actual_peak > self.preflight {
                return Err(limit(
                    "max_memory_bytes",
                    format!(
                        "ODF actual reachable image capacity plan {actual_peak} > preflight {}",
                        self.preflight
                    ),
                ));
            }
            let package_peak = base_peak.checked_add(reachable_capacity).ok_or_else(|| {
                limit("max_memory_bytes", "reachable image working plan overflow")
            })?;
            let media_type = &self
                .manifest
                .get(path)
                .ok_or_else(|| {
                    malformed(Some(path), "referenced image is not declared in manifest")
                })?
                .media_type;
            validate_image(
                &bytes,
                media_type,
                path,
                options,
                context,
                package_peak,
                self.preflight,
            )?;
            self.parts.insert(path.clone(), bytes);
        }
        self.logical_peak = base_peak
            .checked_add(
                reachable_capacity.checked_mul(2).ok_or_else(|| {
                    limit("max_memory_bytes", "reachable image clone plan overflow")
                })?,
            )
            .ok_or_else(|| limit("max_memory_bytes", "reachable image working plan overflow"))?;
        if self.logical_peak > self.preflight {
            return Err(limit(
                "max_memory_bytes",
                format!(
                    "ODF reachable package/asset working plan {} > preflight {}",
                    self.logical_peak, self.preflight
                ),
            ));
        }
        Ok(())
    }
}

impl Package {
    fn skip_unsupported_image(
        &self,
        path: &str,
        options: &ConversionOptions,
    ) -> Result<bool, ConversionError> {
        let unsupported = self
            .manifest
            .get(path)
            .is_some_and(|entry| super::image_validation::unsupported_media(&entry.media_type));
        if unsupported {
            super::recovery::require_best_effort(
                options,
                path,
                "unsupported image media requires a static placeholder",
            )?;
        }
        Ok(unsupported)
    }
}

fn validate_package_graph(
    indexes: &BTreeMap<String, ZipPart>,
    manifest: &BTreeMap<String, ManifestEntry>,
) -> Result<(), ConversionError> {
    for (name, part) in indexes {
        if matches!(name.as_str(), "mimetype" | "META-INF/manifest.xml") {
            continue;
        }
        if part.directory != name.ends_with('/') {
            return Err(malformed(Some(name), "ZIP directory naming is inconsistent"));
        }
        if part.directory {
            if part.size != 0 {
                return Err(malformed(Some(name), "ODF ZIP directories must be empty"));
            }
            continue;
        }
        let declared = manifest
            .get(name)
            .ok_or_else(|| malformed(Some(name), "ZIP part is not declared in manifest"))?;
        if matches!(name.as_str(), "content.xml" | "styles.xml" | "meta.xml" | "settings.xml") {
            if declared.media_type != "text/xml" {
                return Err(malformed(Some(name), "core ODF parts require text/xml media type"));
            }
            continue;
        }
        if declared.media_type.starts_with("image/")
            && !super::image_validation::unsupported_media(&declared.media_type)
        {
            image_profile(name, &declared.media_type)?;
        }
    }
    if let Some(entry) = manifest.get("META-INF/manifest.xml")
        && !entry.media_type.is_empty()
        && entry.media_type != "text/xml"
    {
        return Err(malformed(
            Some("META-INF/manifest.xml"),
            "manifest self-entry has an invalid media type",
        ));
    }
    Ok(())
}

pub(super) fn media_type_for(format: InputFormat) -> Result<&'static str, ConversionError> {
    match format {
        InputFormat::Odt => Ok("application/vnd.oasis.opendocument.text"),
        InputFormat::Ods => Ok("application/vnd.oasis.opendocument.spreadsheet"),
        InputFormat::Odp => Ok("application/vnd.oasis.opendocument.presentation"),
        _ => Err(ConversionError::Internal {
            detail: "ODF converter received a non-ODF format".into(),
        }),
    }
}

fn read_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    index: usize,
    name: &str,
    expected: u64,
    context: &ExecutionContext,
) -> Result<Vec<u8>, ConversionError> {
    let mut entry = archive
        .by_index(index)
        .map_err(|error| malformed(Some(name), format!("cannot open ZIP entry: {error}")))?;
    let length = usize::try_from(expected).map_err(|_| {
        limit("max_decompressed_bytes", format!("{name} size cannot be represented"))
    })?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|error| limit("max_memory_bytes", format!("cannot reserve {name}: {error}")))?;
    let mut buffer = [0_u8; ZIP_STREAM_CHUNK];
    loop {
        context.checkpoint()?;
        let count = entry.read(&mut buffer).map_err(|error| {
            malformed(Some(name), format!("ZIP CRC or decompression failure: {error}"))
        })?;
        if count == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..count]);
        if u64::try_from(output.len()).unwrap_or(u64::MAX) > expected {
            return Err(malformed(
                Some(name),
                "ZIP entry expands beyond its central-directory size",
            ));
        }
    }
    if output.len() != length {
        return Err(malformed(Some(name), "ZIP entry size disagrees with central directory"));
    }
    Ok(output)
}

fn validate_entry_stream(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    index: usize,
    name: &str,
    expected: u64,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut entry = archive
        .by_index(index)
        .map_err(|error| malformed(Some(name), format!("cannot open ZIP entry: {error}")))?;
    let mut actual = 0_u64;
    let mut buffer = [0_u8; ZIP_STREAM_CHUNK];
    loop {
        context.checkpoint()?;
        let count = entry.read(&mut buffer).map_err(|error| {
            malformed(Some(name), format!("ZIP CRC or decompression failure: {error}"))
        })?;
        if count == 0 {
            break;
        }
        actual = actual
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or_else(|| limit("max_decompressed_bytes", "ZIP stream length overflow"))?;
        if actual > expected {
            return Err(malformed(
                Some(name),
                "ZIP entry expands beyond its central-directory size",
            ));
        }
    }
    if actual != expected {
        return Err(malformed(Some(name), "ZIP entry size disagrees with central directory"));
    }
    Ok(())
}

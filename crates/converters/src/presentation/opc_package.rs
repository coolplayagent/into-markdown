use super::allocation::try_clone_string;
use super::budget::ZIP_READ_CHUNK_BYTES;
use super::content_types::parse_content_types;
use super::error::{limit, malformed};
use super::model::{EntryMetadata, LoadedPart, Package, Relationships};
use super::raw_zip::package_open_plan;
use super::relationships::parse_relationships;
use super::relationships::{
    ascii_contains_ignore_case, dangerous_relationship_type, relationship_part, resolve_target,
    validate_compression_ratio, validate_part_name,
};
use super::schema::{COMPOUND_FILE_SIGNATURE, RELATIONSHIPS_CONTENT_TYPE};
#[cfg(test)]
use super::test_observer::PART_MATERIALIZATIONS;
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};

impl<'a> Package<'a> {
    #[allow(clippy::too_many_lines)]
    pub(super) fn open(
        bytes: &'a [u8],
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        if bytes.starts_with(COMPOUND_FILE_SIGNATURE) {
            return Err(ConversionError::Encrypted);
        }
        let plan = package_open_plan(bytes, options, context)?;
        // This reservation intentionally precedes `ZipArchive::new`: the raw, allocation-free
        // central-directory plan above bounds zip-rs's internal metadata materialization.
        let mut memory = context.reserve_memory(plan.memory_charge)?;
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|error| malformed(None, format!("invalid ZIP container: {error}")))?;
        let count = u32::try_from(archive.len()).unwrap_or(u32::MAX);
        if count != plan.entry_count {
            return Err(malformed(None, "ZIP parser disagrees with central-directory entry count"));
        }
        // First inspect only borrowed central-directory metadata. The second pass creates owned
        // names and map nodes only after a conservative plan for both copies (entries/excluded)
        // has been admitted. Unreferenced payload size therefore never inflates the live-memory
        // reservation, while the declared aggregate is still subject to the decompression limit.
        let mut name_bytes = 0_u64;
        let mut declared_total = 0_u64;
        for index in 0..archive.len() {
            context.checkpoint()?;
            let entry = archive.by_index_raw(index).map_err(|error| {
                malformed(None, format!("cannot inspect ZIP entry {index}: {error}"))
            })?;
            if entry.encrypted() {
                return Err(ConversionError::Encrypted);
            }
            let entry_name = strict_zip_entry_name(&entry)?;
            validate_zip_entry_kind(&entry)?;
            validate_compression_ratio(entry_name, entry.size(), entry.compressed_size())?;
            name_bytes = name_bytes
                .checked_add(u64::try_from(entry_name.len()).unwrap_or(u64::MAX))
                .ok_or_else(|| limit("max_memory_bytes", "ZIP name budget overflow"))?;
            declared_total = declared_total
                .checked_add(entry.size())
                .ok_or_else(|| limit("max_decompressed_bytes", "expanded ZIP size overflow"))?;
            if entry.is_dir() {
                validate_part_name(entry_name.strip_suffix('/').unwrap_or(entry_name))?;
                continue;
            }
            validate_part_name(entry_name)?;
        }
        if declared_total > options.limits.max_decompressed_bytes {
            return Err(limit(
                "max_decompressed_bytes",
                format!("{declared_total} > {}", options.limits.max_decompressed_bytes),
            ));
        }
        if name_bytes != plan.name_bytes {
            return Err(malformed(None, "ZIP parser disagrees with central-directory name bytes"));
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(usize::try_from(count).map_err(|_| {
                limit("max_archive_entries", "ZIP entry count cannot be represented")
            })?)
            .map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve ZIP entry index: {error}"))
            })?;
        for index in 0..archive.len() {
            context.checkpoint()?;
            let entry = archive.by_index_raw(index).map_err(|error| {
                malformed(None, format!("cannot inspect ZIP entry {index}: {error}"))
            })?;
            if entry.encrypted() {
                return Err(ConversionError::Encrypted);
            }
            let entry_name = strict_zip_entry_name(&entry)?;
            validate_zip_entry_kind(&entry)?;
            if entry.is_dir() {
                validate_part_name(entry_name.strip_suffix('/').unwrap_or(entry_name))?;
            } else {
                validate_part_name(entry_name)?;
            }
            let name = try_clone_string(entry_name, "ZIP entry name")?;
            let metadata = EntryMetadata {
                index,
                expanded: entry.size(),
                compressed: entry.compressed_size(),
            };
            entries.push((name, metadata));
        }
        entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        if let Some(duplicate) = entries.windows(2).find(|values| values[0].0 == values[1].0) {
            return Err(malformed(Some(&duplicate[0].0), "duplicate ZIP part name"));
        }

        let types_metadata = entries
            .binary_search_by(|(name, _)| name.as_str().cmp("[Content_Types].xml"))
            .ok()
            .map(|index| entries[index].1)
            .ok_or_else(|| malformed(Some("[Content_Types].xml"), "required part is missing"))?;
        let types_charge = part_allocation_charge("[Content_Types].xml", types_metadata.expanded)?;
        memory.grow(types_charge)?;
        let types_bytes = read_entry(
            &mut archive,
            types_metadata.index,
            "[Content_Types].xml",
            types_charge,
            context,
        )?;
        let types_parse_charge =
            parsed_part_charge("[Content_Types].xml", types_metadata.expanded)?;
        memory.grow(types_parse_charge)?;
        let loaded_bytes = types_metadata.expanded;
        if loaded_bytes > options.limits.max_decompressed_bytes {
            return Err(limit(
                "max_decompressed_bytes",
                "content types part exceeds request budget",
            ));
        }
        let content_types = parse_content_types(&types_bytes, options, context)?;
        drop(types_bytes);
        memory.shrink(types_charge)?;

        // Content-type-declared dangerous parts are classified without opening them. Relationship
        // type classification happens lazily, immediately after each authorized `.rels` part is
        // read and before any of its targets can be materialized.
        let mut dangerous_count = 0_usize;
        for (index, (name, _)) in entries.iter().enumerate() {
            if index.is_multiple_of(1024) {
                context.checkpoint()?;
            }
            if content_types.dangerous(name) {
                dangerous_count = dangerous_count
                    .checked_add(1)
                    .ok_or_else(|| limit("max_archive_entries", "dangerous part count overflow"))?;
            }
        }
        let mut excluded = HashSet::new();
        excluded.try_reserve(dangerous_count).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve excluded parts: {error}"))
        })?;
        for (index, (name, _)) in entries.iter().enumerate() {
            if index.is_multiple_of(1024) {
                context.checkpoint()?;
            }
            if content_types.dangerous(name) {
                excluded.insert(try_clone_string(name, "dangerous part name")?);
            }
        }
        let dangerous_present = !excluded.is_empty()
            || content_types
                .overrides
                .iter()
                .any(|(_, value)| ascii_contains_ignore_case(value, "macroenabled"));
        let mut parts = HashMap::new();
        parts.try_reserve(1).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve loaded part index: {error}"))
        })?;
        Ok(Self {
            source: bytes,
            entries,
            parts,
            content_types,
            excluded,
            dangerous_present,
            external_relationships_omitted: false,
            loaded_bytes,
            memory,
            memory_bytes: plan
                .memory_charge
                .checked_add(types_parse_charge)
                .ok_or_else(|| limit("max_memory_bytes", "package reservation overflow"))?,
        })
    }

    pub(super) fn load(
        &mut self,
        part: &str,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<&[u8], ConversionError> {
        context.checkpoint()?;
        if self.excluded.contains(part) {
            return Err(malformed(Some(part), "dangerous package part cannot be materialized"));
        }
        if !self.parts.contains_key(part) {
            let metadata = self
                .entries
                .binary_search_by(|(name, _)| name.as_str().cmp(part))
                .ok()
                .map(|index| self.entries[index].1)
                .ok_or_else(|| malformed(Some(part), "required package part is missing"))?;
            let next = self.loaded_bytes.checked_add(metadata.expanded).ok_or_else(|| {
                limit("max_decompressed_bytes", "loaded part byte count overflow")
            })?;
            if next > options.limits.max_decompressed_bytes {
                return Err(limit(
                    "max_decompressed_bytes",
                    format!("loading {part}: {next} > {}", options.limits.max_decompressed_bytes),
                ));
            }
            validate_compression_ratio(part, metadata.expanded, metadata.compressed)?;
            let allocation_charge = part_allocation_charge(part, metadata.expanded)?;
            self.grow_memory(allocation_charge)?;
            let mut archive = zip::ZipArchive::new(Cursor::new(self.source))
                .map_err(|error| malformed(None, format!("invalid ZIP container: {error}")))?;
            let value = read_entry(&mut archive, metadata.index, part, allocation_charge, context)?;
            self.loaded_bytes = next;
            self.parts.try_reserve(1).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve loaded part: {error}"))
            })?;
            self.parts.insert(
                try_clone_string(part, "loaded part key")?,
                LoadedPart { bytes: value, charge: allocation_charge, parse_charge: 0 },
            );
        }
        self.parts
            .get(part)
            .map(|loaded| loaded.bytes.as_slice())
            .ok_or_else(|| malformed(Some(part), "loaded part index is inconsistent"))
    }

    pub(super) fn load_for_parse(
        &mut self,
        part: &str,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<&[u8], ConversionError> {
        self.load(part, options, context)?;
        let needs_charge = self.parts.get(part).is_some_and(|loaded| loaded.parse_charge == 0);
        if needs_charge {
            let expanded = self
                .entries
                .binary_search_by(|(name, _)| name.as_str().cmp(part))
                .ok()
                .map(|index| self.entries[index].1.expanded)
                .ok_or_else(|| malformed(Some(part), "parsed package part is missing"))?;
            let charge = parsed_part_charge(part, expanded)?;
            self.grow_memory(charge)?;
            self.parts.get_mut(part).expect("loaded part exists").parse_charge = charge;
        }
        self.parts
            .get(part)
            .map(|loaded| loaded.bytes.as_slice())
            .ok_or_else(|| malformed(Some(part), "loaded parse part index is inconsistent"))
    }

    pub(super) fn release_parsed(&mut self, part: &str) -> Result<(), ConversionError> {
        let loaded = self
            .parts
            .remove(part)
            .ok_or_else(|| malformed(Some(part), "parsed package part is not loaded"))?;
        if loaded.parse_charge == 0 {
            return Err(malformed(
                Some(part),
                "package part was released without parser admission",
            ));
        }
        let charge = loaded.charge;
        drop(loaded);
        self.shrink_memory(charge)
    }

    pub(super) fn relationships(
        &mut self,
        owner: &str,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<Relationships, ConversionError> {
        let relationship_part = relationship_part(owner)?;
        if self.content_types.content_type(&relationship_part) != Some(RELATIONSHIPS_CONTENT_TYPE) {
            return Err(malformed(
                Some("[Content_Types].xml"),
                format!("relationship part {relationship_part} lacks its exact content type"),
            ));
        }
        let mut relationships = {
            let bytes = self.load_for_parse(&relationship_part, options, context)?;
            parse_relationships(bytes, owner, options, context)?
        };
        self.release_parsed(&relationship_part)?;
        let isolated_external_chart_data = owner.starts_with("ppt/charts/")
            && relationships.iter().any(|relationship| {
                relationship.external && dangerous_relationship_type(&relationship.kind)
            });
        let ordinary_external = relationships.iter().any(|relationship| {
            relationship.external
                && !(owner.starts_with("ppt/charts/")
                    && dangerous_relationship_type(&relationship.kind))
        });
        if ordinary_external && options.error_policy == into_markdown_core::ErrorPolicy::Strict {
            return Err(malformed(
                Some(&relationship_part),
                "external relationships are forbidden by PresentationML conversion policy",
            ));
        }
        if ordinary_external {
            self.external_relationships_omitted = true;
            relationships.retain(|relationship| !relationship.external);
        }
        if isolated_external_chart_data {
            self.dangerous_present = true;
            relationships.retain(|relationship| !relationship.external);
        }
        for relationship in relationships.iter().filter(|relationship| !relationship.external) {
            if super::relationships::internal_hyperlink_fragment(
                &relationship.kind,
                &relationship.target,
            ) {
                continue;
            }
            let target = resolve_target(owner, &relationship.target)?;
            if dangerous_relationship_type(&relationship.kind) {
                if !self.excluded.contains(&target) {
                    self.excluded.try_reserve(1).map_err(|error| {
                        limit(
                            "max_memory_bytes",
                            format!("cannot reserve excluded relationship: {error}"),
                        )
                    })?;
                    self.excluded.insert(target);
                }
                self.dangerous_present = true;
            }
        }
        Ok(relationships)
    }

    pub(super) fn relationships_optional(
        &mut self,
        owner: &str,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<Relationships, ConversionError> {
        let relationship_part = relationship_part(owner)?;
        if self.entries.binary_search_by(|(name, _)| name.as_str().cmp(&relationship_part)).is_ok()
        {
            self.relationships(owner, options, context)
        } else {
            Ok(Vec::new())
        }
    }

    pub(super) fn take_loaded(&mut self, part: &str) -> Option<LoadedPart> {
        self.parts.remove(part)
    }

    pub(super) fn grow_memory(&mut self, bytes: u64) -> Result<(), ConversionError> {
        self.memory.grow(bytes)?;
        self.memory_bytes = self
            .memory_bytes
            .checked_add(bytes)
            .ok_or_else(|| limit("max_memory_bytes", "package reservation overflow"))?;
        Ok(())
    }

    pub(super) fn shrink_memory(&mut self, bytes: u64) -> Result<(), ConversionError> {
        self.memory.shrink(bytes)?;
        self.memory_bytes = self
            .memory_bytes
            .checked_sub(bytes)
            .ok_or_else(|| limit("max_memory_bytes", "package reservation underflow"))?;
        Ok(())
    }

    pub(super) fn authorize_referenced_part(&self, part: &str) -> Result<(), ConversionError> {
        if self.excluded.contains(part) {
            return Err(malformed(Some(part), "referenced package part is isolated as dangerous"));
        }
        if self.entries.binary_search_by(|(name, _)| name.as_str().cmp(part)).is_err() {
            return Err(malformed(Some(part), "referenced package part is missing"));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn is_loaded(&self, part: &str) -> bool {
        self.parts.contains_key(part)
    }
}

fn validate_zip_entry_kind(entry: &zip::read::ZipFile<'_>) -> Result<(), ConversionError> {
    if let Some(mode) = entry.unix_mode() {
        let kind = mode & 0o170_000;
        let valid = if entry.is_dir() {
            matches!(kind, 0 | 0o040_000)
        } else {
            matches!(kind, 0 | 0o100_000)
        };
        if !valid {
            return Err(malformed(
                Some(entry.name()),
                "ZIP entry type disagrees with a regular file or directory",
            ));
        }
    }
    Ok(())
}

fn strict_zip_entry_name<'a>(
    entry: &'a zip::read::ZipFile<'_>,
) -> Result<&'a str, ConversionError> {
    std::str::from_utf8(entry.name_raw())
        .map_err(|_| malformed(None, "ZIP entry name is not valid UTF-8"))
}

fn read_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    index: usize,
    part: &str,
    allocation_charge: u64,
    context: &ExecutionContext,
) -> Result<Vec<u8>, ConversionError> {
    #[cfg(test)]
    PART_MATERIALIZATIONS.with(|count| count.set(count.get().saturating_add(1)));
    let mut entry = archive
        .by_index(index)
        .map_err(|error| malformed(Some(part), format!("cannot open ZIP part: {error}")))?;
    if entry.encrypted() {
        return Err(ConversionError::Encrypted);
    }
    let declared = entry.size();
    validate_compression_ratio(part, declared, entry.compressed_size())?;
    let capacity = usize::try_from(declared)
        .map_err(|_| limit("max_decompressed_bytes", format!("part {part} is too large")))?;
    let mut value = Vec::new();
    value.try_reserve_exact(capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve part {part}: {error}"))
    })?;
    if u64::try_from(value.capacity()).unwrap_or(u64::MAX) > allocation_charge {
        return Err(limit(
            "max_memory_bytes",
            format!("allocator capacity for {part} exceeds its admitted envelope"),
        ));
    }
    let mut limited = entry.by_ref().take(declared.saturating_add(1));
    let mut chunk = [0_u8; ZIP_READ_CHUNK_BYTES];
    loop {
        context.checkpoint()?;
        let remaining = capacity.saturating_sub(value.len());
        let sample = remaining.saturating_add(1).min(chunk.len());
        let read = limited
            .read(&mut chunk[..sample])
            .map_err(|error| malformed(Some(part), format!("cannot decompress part: {error}")))?;
        if read == 0 {
            break;
        }
        if read > remaining {
            return Err(malformed(Some(part), "part expands past its declared ZIP size"));
        }
        value.extend_from_slice(&chunk[..read]);
    }
    if u64::try_from(value.len()).unwrap_or(u64::MAX) != declared {
        return Err(malformed(Some(part), "decompressed size differs from ZIP directory"));
    }
    if u64::try_from(value.capacity()).unwrap_or(u64::MAX) > allocation_charge {
        return Err(limit(
            "max_memory_bytes",
            format!("final allocator capacity for {part} exceeds its admitted envelope"),
        ));
    }
    Ok(value)
}

pub(super) fn part_allocation_charge(part: &str, expanded: u64) -> Result<u64, ConversionError> {
    expanded
        .checked_add(u64::try_from(part.len()).unwrap_or(u64::MAX).checked_mul(2).ok_or_else(
            || limit("max_memory_bytes", format!("allocation plan overflow for {part}")),
        )?)
        .and_then(|value| value.checked_add(4_096))
        .ok_or_else(|| limit("max_memory_bytes", format!("allocation plan overflow for {part}")))
}

fn parsed_part_charge(part: &str, expanded: u64) -> Result<u64, ConversionError> {
    expanded
        .checked_mul(4)
        .and_then(|value| value.checked_add(4_096))
        .ok_or_else(|| limit("max_memory_bytes", format!("parser plan overflow for {part}")))
}

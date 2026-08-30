#[derive(Debug)]
struct Package {
    parts: BTreeMap<String, Vec<u8>>,
    content_types: ContentTypes,
    macro_present: bool,
    _memory: into_markdown_core::ResourceReservation,
}

impl Package {
    #[allow(clippy::too_many_lines)]
    fn open(
        bytes: &[u8],
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        if bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]) {
            // Password-protected OOXML is wrapped in an OLE Compound File containing
            // EncryptionInfo and EncryptedPackage streams. It is never passed to an OLE reader.
            return Err(ConversionError::Encrypted);
        }
        let input_size = u64::try_from(bytes.len())
            .map_err(|_| limit("max_input_bytes", "DOCX size overflow"))?;
        if input_size > options.limits.max_input_bytes {
            return Err(limit(
                "max_input_bytes",
                format!("{input_size} > {}", options.limits.max_input_bytes),
            ));
        }
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|error| malformed(None, format!("invalid ZIP container: {error}")))?;
        let count = u32::try_from(archive.len()).unwrap_or(u32::MAX);
        if count > options.limits.max_archive_entries {
            return Err(limit(
                "max_archive_entries",
                format!("{count} > {}", options.limits.max_archive_entries),
            ));
        }
        let metadata_bytes = u64::try_from(archive.len())
            .unwrap_or(u64::MAX)
            .checked_mul(
                u64::try_from(std::mem::size_of::<(usize, String, u64, bool)>() + 64)
                    .unwrap_or(u64::MAX),
            )
            .ok_or_else(|| limit("max_memory_bytes", "ZIP metadata budget overflow"))?;
        let mut memory = context.reserve_memory(metadata_bytes)?;
        let mut names = BTreeSet::new();
        let mut metadata = Vec::with_capacity(archive.len());
        let mut total = 0_u64;
        for index in 0..archive.len() {
            context.checkpoint()?;
            let entry = archive.by_index_raw(index).map_err(|error| {
                malformed(None, format!("cannot inspect ZIP entry {index}: {error}"))
            })?;
            if entry.encrypted() {
                return Err(ConversionError::Encrypted);
            }
            if entry.is_dir() {
                continue;
            }
            let name = canonical_part_name(entry.name())?;
            let name_bytes = u64::try_from(name.len()).unwrap_or(u64::MAX).saturating_mul(2);
            memory.grow(name_bytes)?;
            if !names.insert(name.clone()) {
                return Err(malformed(Some(&name), "duplicate ZIP part name"));
            }
            total = total
                .checked_add(entry.size())
                .ok_or_else(|| limit("max_decompressed_bytes", "DOCX expanded size overflow"))?;
            if total > options.limits.max_decompressed_bytes {
                return Err(limit(
                    "max_decompressed_bytes",
                    format!("{total} > {}", options.limits.max_decompressed_bytes),
                ));
            }
            metadata.push((index, name, entry.size()));
        }

        let content_index = metadata
            .iter()
            .find(|(_, name, _)| name == "[Content_Types].xml")
            .map(|(index, _, _)| *index)
            .ok_or_else(|| {
                malformed(Some("[Content_Types].xml"), "required package part is missing")
            })?;
        let mut parts = BTreeMap::new();
        let content_types_bytes =
            read_zip_entry(&mut archive, content_index, "[Content_Types].xml", &mut memory)?;
        let content_types = parse_content_types(&content_types_bytes, options, context)?;
        parts.insert("[Content_Types].xml".into(), content_types_bytes);

        let mut excluded = metadata
            .iter()
            .filter(|(_, name, _)| content_types.is_macro_part(name))
            .map(|(_, name, _)| name.clone())
            .collect::<BTreeSet<_>>();

        // Walk reachable relationship parts from the package root before unrelated metadata.
        // A relationship can therefore classify an arbitrarily renamed VBA target (including a
        // misleading `.rels` suffix) before that target is ever opened or decompressed.
        let mut pending_relationships = metadata
            .iter()
            .filter(|(_, name, _)| {
                name != "[Content_Types].xml"
                    && Path::new(name)
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("rels"))
            })
            .map(|(index, name, _)| (name.clone(), *index))
            .collect::<BTreeMap<_, _>>();
        let mut relationship_queue = VecDeque::from(["_rels/.rels".to_owned()]);
        while !pending_relationships.is_empty() {
            let name = relationship_queue.pop_front().or_else(|| {
                pending_relationships.keys().find(|name| !excluded.contains(*name)).cloned()
            });
            let Some(name) = name else {
                break;
            };
            let Some(index) = pending_relationships.remove(&name) else {
                continue;
            };
            if excluded.contains(&name) {
                continue;
            }
            let relationship_bytes = read_zip_entry(&mut archive, index, &name, &mut memory)?;
            let owner = relationship_owner(&name)?;
            let relationships =
                parse_relationships(Some(&relationship_bytes), &owner, options, context)?;
            for relationship in relationships.values().filter(|value| !value.external) {
                let target = resolve_target(&owner, &relationship.target)?;
                if is_macro_relationship_type(&relationship.kind) {
                    excluded.insert(target);
                } else {
                    let target_relationships = relationship_part(&target);
                    if pending_relationships.contains_key(&target_relationships) {
                        relationship_queue.push_back(target_relationships);
                    }
                }
            }
            parts.insert(name, relationship_bytes);
        }

        let macro_present = !excluded.is_empty() || content_types.macro_enabled_main();
        for (index, name, _declared) in metadata {
            context.checkpoint()?;
            if parts.contains_key(&name) || excluded.contains(&name) {
                continue;
            }
            let value = read_zip_entry(&mut archive, index, &name, &mut memory)?;
            parts.insert(name, value);
        }
        Ok(Self { parts, content_types, macro_present, _memory: memory })
    }

    fn required(&self, name: &str) -> Result<&[u8], ConversionError> {
        self.parts
            .get(name)
            .map(Vec::as_slice)
            .ok_or_else(|| malformed(Some(name), "required package part is missing"))
    }

    fn take_required(&mut self, name: &str) -> Result<Vec<u8>, ConversionError> {
        self.parts
            .remove(name)
            .ok_or_else(|| malformed(Some(name), "required package part is missing"))
    }
}

fn read_zip_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    index: usize,
    name: &str,
    memory: &mut into_markdown_core::ResourceReservation,
) -> Result<Vec<u8>, ConversionError> {
    let mut entry = archive
        .by_index(index)
        .map_err(|error| malformed(Some(name), format!("cannot open ZIP part: {error}")))?;
    if entry.encrypted() {
        return Err(ConversionError::Encrypted);
    }
    let declared = entry.size();
    memory.grow(declared)?;
    let cap = usize::try_from(declared)
        .map_err(|_| limit("max_decompressed_bytes", format!("part {name} is too large")))?;
    let mut value = Vec::new();
    value.try_reserve_exact(cap).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve part {name}: {error}"))
    })?;
    entry
        .by_ref()
        .take(declared.saturating_add(1))
        .read_to_end(&mut value)
        .map_err(|error| malformed(Some(name), format!("cannot decompress part: {error}")))?;
    if u64::try_from(value.len()).unwrap_or(u64::MAX) != declared {
        return Err(malformed(Some(name), "decompressed size differs from ZIP directory"));
    }
    Ok(value)
}

fn is_macro_content_type(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("vbaproject")
        || value.contains("vbadata")
        || value.contains("activex")
        || value.contains("macroenabled.template")
}

fn is_macro_relationship_type(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.ends_with("/vbaproject") || value.ends_with("/vbadata") || value.contains("/activex")
}

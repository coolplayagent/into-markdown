use crate::odf::model::{
    IMAGE_DECODER_HEADER_BYTES, PACKAGE_BASE_WORKING_BYTES, ZIP_STREAM_CHUNK, limit, malformed,
};
use into_markdown_core::{ConversionError, ExecutionContext};
use std::mem::size_of;

#[cfg(test)]
use std::cell::Cell as CounterCell;

#[cfg(test)]
std::thread_local! {
    pub(super) static RAW_LAYOUT_ALLOCATION_ATTEMPTS: CounterCell<usize> = const { CounterCell::new(0) };
}

pub(super) fn image_decode_plan(encoded_bytes: u64, pixels: u64) -> Result<u64, ConversionError> {
    pixels
        .checked_mul(32)
        .and_then(|value| encoded_bytes.checked_mul(2).and_then(|size| value.checked_add(size)))
        .and_then(|value| value.checked_add(IMAGE_DECODER_HEADER_BYTES))
        .ok_or_else(|| limit("image_decode_memory", "ODF image decode plan overflow"))
}

pub(super) fn conservative_vec_capacity(declared_bytes: u64) -> Result<u64, ConversionError> {
    declared_bytes
        .checked_mul(2)
        .ok_or_else(|| limit("max_memory_bytes", "declared Vec capacity plan overflow"))
}

pub(super) fn reachable_image_peak(
    base_peak: u64,
    reachable_capacity: u64,
) -> Result<u64, ConversionError> {
    let loaded = base_peak
        .checked_add(reachable_capacity)
        .ok_or_else(|| limit("max_memory_bytes", "reachable image load plan overflow"))?;
    let cloned = base_peak
        .checked_add(
            reachable_capacity
                .checked_mul(2)
                .ok_or_else(|| limit("max_memory_bytes", "reachable image clone plan overflow"))?,
        )
        .ok_or_else(|| limit("max_memory_bytes", "reachable image clone plan overflow"))?;
    Ok(loaded.max(cloned))
}

pub(super) fn package_logical_peak(
    core_expanded: u64,
    metadata_bytes: u64,
    entries: u64,
) -> Result<u64, ConversionError> {
    core_expanded
        .checked_mul(64)
        .and_then(|value| value.checked_add(metadata_bytes))
        .and_then(|value| entries.checked_mul(512).and_then(|entries| value.checked_add(entries)))
        .and_then(|value| value.checked_add(PACKAGE_BASE_WORKING_BYTES))
        .and_then(|value| value.checked_add(u64::try_from(ZIP_STREAM_CHUNK).unwrap_or(u64::MAX)))
        .ok_or_else(|| limit("max_memory_bytes", "ODF package/XML working plan overflow"))
}

pub(super) fn package_index_peak(entries: u64) -> Result<u64, ConversionError> {
    entries
        .checked_mul(512)
        .and_then(|value| value.checked_add(PACKAGE_BASE_WORKING_BYTES))
        .and_then(|value| value.checked_add(u64::try_from(ZIP_STREAM_CHUNK).unwrap_or(u64::MAX)))
        .ok_or_else(|| limit("max_memory_bytes", "ODF ZIP index plan overflow"))
}

fn package_raw_index_peak(entries: u64, central_size: usize) -> Result<u64, ConversionError> {
    let central = u64::try_from(central_size)
        .map_err(|_| limit("max_memory_bytes", "central directory size cannot be represented"))?;
    package_index_peak(entries)?
        .checked_add(
            central
                .checked_mul(4)
                .ok_or_else(|| limit("max_memory_bytes", "raw ZIP name plan overflow"))?,
        )
        .ok_or_else(|| limit("max_memory_bytes", "raw ZIP/name index plan overflow"))
}

#[derive(Clone, Debug)]
pub(super) struct LocalMimetypeHeader {
    flags: u16,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    data_start: u64,
}

pub(super) fn mimetype_header_for_policy(
    bytes: &[u8],
    expected: &str,
    options: &into_markdown_core::ConversionOptions,
) -> Result<Option<LocalMimetypeHeader>, ConversionError> {
    let result = validate_first_mimetype_local_header(bytes, expected);
    if options.error_policy == into_markdown_core::ErrorPolicy::Strict || result.is_ok() {
        return result.map(Some);
    }
    // Only packing deviations are deferred. Complete raw ZIP local/central/descriptor
    // binding and streamed payload CRC checks still run before the package is accepted.
    if read_u32(bytes, 0) == Some(0x0403_4b50)
        && (validated_local_zip_name(bytes, 0)? != "mimetype"
            || read_u16(bytes, 8).is_some_and(|method| method != 0)
            || read_u16(bytes, 6).is_some_and(|flags| flags & 8 != 0))
    {
        return Ok(None);
    }
    result.map(Some)
}

pub(super) fn validate_relaxed_mimetype_extras(
    bytes: &[u8],
    entry: &zip::read::ZipFile<'_>,
) -> Result<(), ConversionError> {
    let local = usize::try_from(entry.header_start())
        .map_err(|_| malformed(Some("mimetype"), "local offset overflow"))?;
    let central = usize::try_from(entry.central_header_start())
        .map_err(|_| malformed(Some("mimetype"), "central offset overflow"))?;
    if read_u16(bytes, local + 28) != Some(0) || read_u16(bytes, central + 30) != Some(0) {
        return Err(malformed(Some("mimetype"), "mimetype extra fields are not supported"));
    }
    Ok(())
}

pub(super) fn validate_first_mimetype_local_header(
    bytes: &[u8],
    expected: &str,
) -> Result<LocalMimetypeHeader, ConversionError> {
    const FIXED: usize = 30;
    if bytes.len() < FIXED || read_u32(bytes, 0) != Some(0x0403_4b50) {
        return Err(malformed(Some("mimetype"), "first ZIP local header is missing"));
    }
    let flags = read_u16(bytes, 6).ok_or_else(|| malformed(None, "truncated ZIP flags"))?;
    if flags & 1 != 0 {
        return Err(ConversionError::Encrypted);
    }
    if flags & (1 << 3) != 0 {
        return Err(malformed(Some("mimetype"), "mimetype may not use a ZIP data descriptor"));
    }
    let method = read_u16(bytes, 8).ok_or_else(|| malformed(None, "truncated ZIP method"))?;
    if method != 0 {
        return Err(malformed(Some("mimetype"), "mimetype must use Stored compression"));
    }
    let crc32 = read_u32(bytes, 14).ok_or_else(|| malformed(None, "truncated ZIP CRC"))?;
    let compressed_size =
        read_u32(bytes, 18).ok_or_else(|| malformed(None, "truncated ZIP size"))?;
    let uncompressed_size =
        read_u32(bytes, 22).ok_or_else(|| malformed(None, "truncated ZIP size"))?;
    let name_len = usize::from(
        read_u16(bytes, 26).ok_or_else(|| malformed(None, "truncated ZIP name length"))?,
    );
    let extra_len = usize::from(
        read_u16(bytes, 28).ok_or_else(|| malformed(None, "truncated ZIP extra length"))?,
    );
    let extra_start = FIXED
        .checked_add(name_len)
        .ok_or_else(|| malformed(None, "ZIP local header length overflow"))?;
    let data_start = extra_start
        .checked_add(extra_len)
        .ok_or_else(|| malformed(None, "ZIP local header length overflow"))?;
    let data_end = data_start
        .checked_add(usize::try_from(compressed_size).unwrap_or(usize::MAX))
        .ok_or_else(|| malformed(None, "ZIP mimetype data length overflow"))?;
    let name = bytes
        .get(FIXED..extra_start)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated mimetype local name"))?;
    let extra = bytes
        .get(extra_start..data_start)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated mimetype local extra field"))?;
    validate_zip_extra(extra, "mimetype")?;
    if extra_len != 0 {
        return Err(malformed(
            Some("mimetype"),
            "mimetype local header must not contain ZIP extra fields",
        ));
    }
    if name != b"mimetype" {
        return Err(malformed(
            Some("mimetype"),
            "the first ZIP local entry must be named exactly mimetype",
        ));
    }
    let payload = bytes
        .get(data_start..data_end)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated mimetype local payload"))?;
    let expected_size = u32::try_from(expected.len())
        .map_err(|_| malformed(Some("mimetype"), "expected media type is too large"))?;
    if compressed_size != expected_size
        || uncompressed_size != expected_size
        || payload != expected.as_bytes()
        || crc32_ieee(payload) != crc32
    {
        return Err(malformed(
            Some("mimetype"),
            "mimetype local CRC, sizes, or payload do not match the exact ODF media type",
        ));
    }
    Ok(LocalMimetypeHeader {
        flags,
        crc32,
        compressed_size,
        uncompressed_size,
        data_start: u64::try_from(data_start).unwrap_or(u64::MAX),
    })
}

pub(super) fn validate_raw_mimetype_central(
    bytes: &[u8],
    local: &LocalMimetypeHeader,
) -> Result<(), ConversionError> {
    let eocd = bytes
        .len()
        .checked_sub(22)
        .ok_or_else(|| malformed(Some("mimetype"), "ZIP EOCD is missing"))?;
    if read_u32(bytes, eocd) != Some(0x0605_4b50) {
        return Err(malformed(Some("mimetype"), "ZIP EOCD is not final"));
    }
    let central = usize::try_from(
        read_u32(bytes, eocd + 16)
            .ok_or_else(|| malformed(Some("mimetype"), "truncated EOCD central offset"))?,
    )
    .map_err(|_| malformed(Some("mimetype"), "central offset cannot be represented"))?;
    if read_u32(bytes, central) != Some(0x0201_4b50) {
        return Err(malformed(Some("mimetype"), "first central-directory entry is missing"));
    }
    let flags = read_u16(bytes, central + 8)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central flags"))?;
    let method = read_u16(bytes, central + 10)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central method"))?;
    let crc32 = read_u32(bytes, central + 16)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central CRC"))?;
    let compressed = read_u32(bytes, central + 20)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central size"))?;
    let uncompressed = read_u32(bytes, central + 24)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central size"))?;
    let name_len = usize::from(
        read_u16(bytes, central + 28)
            .ok_or_else(|| malformed(Some("mimetype"), "truncated central name length"))?,
    );
    let extra_len = usize::from(
        read_u16(bytes, central + 30)
            .ok_or_else(|| malformed(Some("mimetype"), "truncated central extra length"))?,
    );
    let comment_len = usize::from(
        read_u16(bytes, central + 32)
            .ok_or_else(|| malformed(Some("mimetype"), "truncated central comment length"))?,
    );
    let local_offset = read_u32(bytes, central + 42)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central local offset"))?;
    let name_start = central
        .checked_add(46)
        .ok_or_else(|| malformed(Some("mimetype"), "central header overflow"))?;
    let extra_start = name_start
        .checked_add(name_len)
        .ok_or_else(|| malformed(Some("mimetype"), "central header overflow"))?;
    let extra_end = extra_start
        .checked_add(extra_len)
        .ok_or_else(|| malformed(Some("mimetype"), "central header overflow"))?;
    let name = bytes
        .get(name_start..extra_start)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central name"))?;
    let extra = bytes
        .get(extra_start..extra_end)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central extra"))?;
    validate_zip_extra(extra, "mimetype")?;
    if flags != local.flags
        || flags & (1 << 3) != 0
        || method != 0
        || crc32 != local.crc32
        || compressed != local.compressed_size
        || uncompressed != local.uncompressed_size
        || local_offset != 0
        || name != b"mimetype"
        || extra_len != 0
        || comment_len != 0
    {
        return Err(malformed(
            Some("mimetype"),
            "raw mimetype local and first central header disagree",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) fn validate_zip_directory_layout(
    bytes: &[u8],
    max_archive_entries: u32,
    planned: u64,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    #[derive(Clone, Copy)]
    struct CentralLayout {
        local_offset: usize,
        flags: u16,
        method: u16,
        crc32: u32,
        compressed: u32,
        uncompressed: u32,
        name_start: usize,
        name_len: usize,
    }

    const EOCD_LEN: usize = 22;
    let eocd = bytes
        .len()
        .checked_sub(EOCD_LEN)
        .ok_or_else(|| malformed(None, "ZIP container is shorter than EOCD"))?;
    if read_u32(bytes, eocd) != Some(0x0605_4b50) || read_u16(bytes, eocd + 20) != Some(0) {
        return Err(malformed(
            None,
            "ODF ZIP EOCD must be the final unique record and may not have a comment",
        ));
    }
    for offset in 0..=bytes.len().saturating_sub(4) {
        if offset.is_multiple_of(1024 * 1024) {
            context.checkpoint()?;
        }
        if offset != eocd && bytes.get(offset..offset + 4) == Some(b"PK\x05\x06") {
            return Err(malformed(None, "ODF ZIP contains a second EOCD signature"));
        }
    }
    let disk = read_u16(bytes, eocd + 4).unwrap_or(u16::MAX);
    let central_disk = read_u16(bytes, eocd + 6).unwrap_or(u16::MAX);
    let disk_entries = read_u16(bytes, eocd + 8).unwrap_or(u16::MAX);
    let total_entries = read_u16(bytes, eocd + 10).unwrap_or(u16::MAX);
    let central_size = usize::try_from(read_u32(bytes, eocd + 12).unwrap_or(u32::MAX))
        .map_err(|_| malformed(None, "ZIP central size cannot be represented"))?;
    let central_start = usize::try_from(read_u32(bytes, eocd + 16).unwrap_or(u32::MAX))
        .map_err(|_| malformed(None, "ZIP central offset cannot be represented"))?;
    if total_entries == u16::MAX
        || read_u32(bytes, eocd + 12) == Some(u32::MAX)
        || read_u32(bytes, eocd + 16) == Some(u32::MAX)
    {
        return Err(ConversionError::Unsupported {
            detail: "ZIP64 ODF packages are outside the supported profile".into(),
        });
    }
    if u32::from(total_entries) > max_archive_entries {
        return Err(limit(
            "max_archive_entries",
            format!("{total_entries} > {max_archive_entries}"),
        ));
    }
    let index_peak = package_index_peak(u64::from(total_entries))?;
    if index_peak > planned {
        return Err(limit(
            "max_memory_bytes",
            format!("ODF ZIP index plan {index_peak} > preflight {planned}"),
        ));
    }
    if disk != 0
        || central_disk != 0
        || disk_entries != total_entries
        || central_start.checked_add(central_size) != Some(eocd)
    {
        return Err(malformed(
            None,
            "multi-disk, ZIP64, prefixed, overlapping, or trailing ZIP layout is forbidden",
        ));
    }
    let raw_index_peak = package_raw_index_peak(u64::from(total_entries), central_size)?;
    if raw_index_peak > planned {
        return Err(limit(
            "max_memory_bytes",
            format!("raw ZIP/name index plan {raw_index_peak} > preflight {planned}"),
        ));
    }
    let layout_bytes = u64::from(total_entries)
        .checked_mul(u64::try_from(size_of::<CentralLayout>()).unwrap_or(u64::MAX))
        .ok_or_else(|| limit("max_memory_bytes", "raw ZIP layout plan overflow"))?;
    if layout_bytes > planned {
        return Err(limit(
            "max_memory_bytes",
            format!("raw ZIP layout plan {layout_bytes} > preflight {planned}"),
        ));
    }
    let mut cursor = central_start;
    let mut layouts = Vec::new();
    #[cfg(test)]
    RAW_LAYOUT_ALLOCATION_ATTEMPTS.with(|count| count.set(count.get().saturating_add(1)));
    layouts.try_reserve_exact(usize::from(total_entries)).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve raw ZIP layout: {error}"))
    })?;
    for entry_index in 0..total_entries {
        if entry_index.is_multiple_of(256) {
            context.checkpoint()?;
        }
        if read_u32(bytes, cursor) != Some(0x0201_4b50) {
            return Err(malformed(None, "central-directory entry boundary is invalid"));
        }
        let flags = read_u16(bytes, cursor + 8).unwrap_or(u16::MAX);
        let method = read_u16(bytes, cursor + 10).unwrap_or(u16::MAX);
        let crc32 = read_u32(bytes, cursor + 16).unwrap_or(u32::MAX);
        let compressed = read_u32(bytes, cursor + 20).unwrap_or(u32::MAX);
        let uncompressed = read_u32(bytes, cursor + 24).unwrap_or(u32::MAX);
        let name_len = usize::from(read_u16(bytes, cursor + 28).unwrap_or(u16::MAX));
        let extra_len = usize::from(read_u16(bytes, cursor + 30).unwrap_or(u16::MAX));
        let comment_len = usize::from(read_u16(bytes, cursor + 32).unwrap_or(u16::MAX));
        let disk_start = read_u16(bytes, cursor + 34).unwrap_or(u16::MAX);
        let local_offset = read_u32(bytes, cursor + 42).unwrap_or(u32::MAX);
        let extra_start = cursor
            .checked_add(46)
            .and_then(|value| value.checked_add(name_len))
            .ok_or_else(|| malformed(None, "central-directory length overflow"))?;
        let next = extra_start
            .checked_add(extra_len)
            .and_then(|value| value.checked_add(comment_len))
            .ok_or_else(|| malformed(None, "central-directory length overflow"))?;
        let name_start = cursor
            .checked_add(46)
            .ok_or_else(|| malformed(None, "central-directory length overflow"))?;
        let name = bytes
            .get(name_start..extra_start)
            .ok_or_else(|| malformed(None, "truncated central-directory name"))?;
        validate_raw_zip_name(name, flags, "central directory")?;
        let extra = bytes
            .get(extra_start..extra_start + extra_len)
            .ok_or_else(|| malformed(None, "truncated central-directory extra data"))?;
        validate_zip_extra(extra, "central directory")?;
        if compressed == u32::MAX
            || uncompressed == u32::MAX
            || local_offset == u32::MAX
            || disk_start != 0
            || next > eocd
        {
            if compressed == u32::MAX || uncompressed == u32::MAX || local_offset == u32::MAX {
                return Err(ConversionError::Unsupported {
                    detail: "ZIP64 ODF packages are outside the supported profile".into(),
                });
            }
            return Err(malformed(
                None,
                "split ZIP entry or central-directory boundary is invalid",
            ));
        }
        layouts.push(CentralLayout {
            local_offset: usize::try_from(local_offset)
                .map_err(|_| malformed(None, "local-header offset cannot be represented"))?,
            flags,
            method,
            crc32,
            compressed,
            uncompressed,
            name_start,
            name_len,
        });
        cursor = next;
    }
    if cursor != eocd {
        return Err(malformed(
            None,
            "central directory has trailing, duplicate, or uncounted records",
        ));
    }
    layouts.sort_by_key(|layout| layout.local_offset);
    let mut expected_offset = 0_usize;
    for (entry_index, layout) in layouts.into_iter().enumerate() {
        if entry_index.is_multiple_of(256) {
            context.checkpoint()?;
        }
        if layout.local_offset != expected_offset
            || read_u32(bytes, layout.local_offset) != Some(0x0403_4b50)
        {
            return Err(malformed(
                None,
                "ZIP local records contain a preamble, gap, overlap, or fake trailing header",
            ));
        }
        let local_flags = read_u16(bytes, layout.local_offset + 6).unwrap_or(u16::MAX);
        let local_method = read_u16(bytes, layout.local_offset + 8).unwrap_or(u16::MAX);
        let local_crc = read_u32(bytes, layout.local_offset + 14).unwrap_or(u32::MAX);
        let local_compressed = read_u32(bytes, layout.local_offset + 18).unwrap_or(u32::MAX);
        let local_uncompressed = read_u32(bytes, layout.local_offset + 22).unwrap_or(u32::MAX);
        let local_name_len =
            usize::from(read_u16(bytes, layout.local_offset + 26).unwrap_or(u16::MAX));
        let local_extra_len =
            usize::from(read_u16(bytes, layout.local_offset + 28).unwrap_or(u16::MAX));
        let local_name_start = layout
            .local_offset
            .checked_add(30)
            .ok_or_else(|| malformed(None, "local-header length overflow"))?;
        let local_extra_start = local_name_start
            .checked_add(local_name_len)
            .ok_or_else(|| malformed(None, "local-header length overflow"))?;
        let local_data_start = local_extra_start
            .checked_add(local_extra_len)
            .ok_or_else(|| malformed(None, "local-header length overflow"))?;
        let local_data_end = local_data_start
            .checked_add(usize::try_from(layout.compressed).unwrap_or(usize::MAX))
            .ok_or_else(|| malformed(None, "local payload length overflow"))?;
        let local_name = bytes
            .get(local_name_start..local_extra_start)
            .ok_or_else(|| malformed(None, "truncated local name"))?;
        let central_name = bytes
            .get(layout.name_start..layout.name_start + layout.name_len)
            .ok_or_else(|| malformed(None, "truncated central name"))?;
        let local_extra = bytes
            .get(local_extra_start..local_data_start)
            .ok_or_else(|| malformed(None, "truncated local extra field"))?;
        let local_semantic = validate_raw_zip_name(local_name, local_flags, "local header")?;
        let central_semantic =
            validate_raw_zip_name(central_name, layout.flags, "central directory")?;
        validate_zip_extra(local_extra, local_semantic)?;
        let descriptor = local_flags & (1 << 3) != 0;
        let local_sizes_match = if descriptor {
            (local_crc == 0 || local_crc == layout.crc32)
                && (local_compressed == 0 || local_compressed == layout.compressed)
                && (local_uncompressed == 0 || local_uncompressed == layout.uncompressed)
        } else {
            local_crc == layout.crc32
                && local_compressed == layout.compressed
                && local_uncompressed == layout.uncompressed
        };
        if local_flags != layout.flags
            || local_method != layout.method
            || !local_sizes_match
            || local_name != central_name
            || local_semantic != central_semantic
            || local_data_end > central_start
        {
            return Err(malformed(
                None,
                "ZIP local/central headers disagree or exceed the central-directory boundary",
            ));
        }
        expected_offset = if descriptor {
            descriptor_end(
                &bytes[..central_start],
                local_data_end,
                layout.crc32,
                layout.compressed,
                layout.uncompressed,
            )?
        } else {
            local_data_end
        };
    }
    if expected_offset != central_start {
        return Err(malformed(
            None,
            "ZIP local records do not end exactly at the unique central directory",
        ));
    }
    Ok(())
}

// ZipArchive verifies streamed payload CRC/size against the central directory. Bind the
// optional descriptor too, so accepting bit 3 does not introduce unchecked gaps/overlaps.
fn descriptor_end(
    bytes: &[u8],
    offset: usize,
    crc32: u32,
    compressed: u32,
    uncompressed: u32,
) -> Result<usize, ConversionError> {
    for signature_bytes in [0, 4] {
        if signature_bytes == 4 && read_u32(bytes, offset) != Some(0x0807_4b50) {
            continue;
        }
        let start = offset + signature_bytes;
        if read_u32(bytes, start) == Some(crc32)
            && read_u32(bytes, start + 4) == Some(compressed)
            && read_u32(bytes, start + 8) == Some(uncompressed)
        {
            return Ok(start + 12);
        }
    }
    Err(malformed(None, "ZIP data descriptor is truncated or disagrees with central CRC/sizes"))
}

pub(super) fn bind_mimetype_central(
    bytes: &[u8],
    entry: &zip::read::ZipFile<'_>,
    local: &LocalMimetypeHeader,
) -> Result<(), ConversionError> {
    if entry.name() != "mimetype"
        || entry.header_start() != 0
        || entry.data_start() != local.data_start
        || entry.compression() != zip::CompressionMethod::Stored
        || entry.crc32() != local.crc32
        || entry.compressed_size() != u64::from(local.compressed_size)
        || entry.size() != u64::from(local.uncompressed_size)
    {
        return Err(malformed(Some("mimetype"), "mimetype local and central ZIP headers disagree"));
    }
    let offset = usize::try_from(entry.central_header_start())
        .map_err(|_| malformed(Some("mimetype"), "central header offset overflow"))?;
    if read_u32(bytes, offset) != Some(0x0201_4b50) {
        return Err(malformed(Some("mimetype"), "mimetype central header is missing"));
    }
    let central_flags = read_u16(bytes, offset + 8)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central flags"))?;
    let central_method = read_u16(bytes, offset + 10)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central method"))?;
    let central_crc = read_u32(bytes, offset + 16)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central CRC"))?;
    let central_compressed = read_u32(bytes, offset + 20)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central size"))?;
    let central_uncompressed = read_u32(bytes, offset + 24)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central size"))?;
    let name_len = usize::from(
        read_u16(bytes, offset + 28)
            .ok_or_else(|| malformed(Some("mimetype"), "truncated central name length"))?,
    );
    let extra_len = usize::from(
        read_u16(bytes, offset + 30)
            .ok_or_else(|| malformed(Some("mimetype"), "truncated central extra length"))?,
    );
    let comment_len = usize::from(
        read_u16(bytes, offset + 32)
            .ok_or_else(|| malformed(Some("mimetype"), "truncated central comment length"))?,
    );
    let local_offset = read_u32(bytes, offset + 42)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central local offset"))?;
    let name_start = offset
        .checked_add(46)
        .ok_or_else(|| malformed(Some("mimetype"), "central header overflow"))?;
    let extra_start = name_start
        .checked_add(name_len)
        .ok_or_else(|| malformed(Some("mimetype"), "central header overflow"))?;
    let extra_end = extra_start
        .checked_add(extra_len)
        .ok_or_else(|| malformed(Some("mimetype"), "central header overflow"))?;
    let name = bytes
        .get(name_start..extra_start)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central name"))?;
    let extra = bytes
        .get(extra_start..extra_end)
        .ok_or_else(|| malformed(Some("mimetype"), "truncated central extra data"))?;
    validate_zip_extra(extra, "mimetype")?;
    if central_flags != local.flags
        || central_flags & (1 << 3) != 0
        || central_method != 0
        || central_crc != local.crc32
        || central_compressed != local.compressed_size
        || central_uncompressed != local.uncompressed_size
        || local_offset != 0
        || name != b"mimetype"
        || extra_len != 0
        || comment_len != 0
    {
        return Err(malformed(
            Some("mimetype"),
            "mimetype local/central name, flags, method, CRC, sizes, offset, or ZIP64 fields disagree",
        ));
    }
    Ok(())
}

fn validate_zip_extra(mut bytes: &[u8], part: &str) -> Result<(), ConversionError> {
    while !bytes.is_empty() {
        let id =
            read_u16(bytes, 0).ok_or_else(|| malformed(Some(part), "truncated ZIP extra field"))?;
        let length = usize::from(
            read_u16(bytes, 2).ok_or_else(|| malformed(Some(part), "truncated ZIP extra field"))?,
        );
        bytes = bytes
            .get(4 + length..)
            .ok_or_else(|| malformed(Some(part), "truncated ZIP extra field"))?;
        if id == 0x0001 {
            return Err(ConversionError::Unsupported {
                detail: "ZIP64 ODF packages are outside the supported profile".into(),
            });
        }
        if id == 0x7075 {
            return Err(malformed(
                Some(part),
                "Info-ZIP Unicode Path extra fields are forbidden because they can rename parts",
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_raw_zip_name<'a>(
    raw: &'a [u8],
    flags: u16,
    part: &str,
) -> Result<&'a str, ConversionError> {
    if raw.is_empty() {
        return Err(malformed(Some(part), "ZIP entry name is empty"));
    }
    let name = std::str::from_utf8(raw)
        .map_err(|_| malformed(Some(part), "ZIP entry name is not strict UTF-8"))?;
    if raw.iter().any(|byte| !byte.is_ascii()) && flags & (1 << 11) == 0 {
        return Err(malformed(
            Some(part),
            "non-ASCII ZIP entry names must set the UTF-8 language flag",
        ));
    }
    Ok(name)
}

pub(super) fn validated_local_zip_name(
    bytes: &[u8],
    header_start: u64,
) -> Result<&str, ConversionError> {
    let offset = usize::try_from(header_start)
        .map_err(|_| malformed(None, "local ZIP header offset cannot be represented"))?;
    if read_u32(bytes, offset) != Some(0x0403_4b50) {
        return Err(malformed(None, "authenticated local ZIP header is missing"));
    }
    let flags =
        read_u16(bytes, offset + 6).ok_or_else(|| malformed(None, "truncated local ZIP flags"))?;
    let name_len = usize::from(
        read_u16(bytes, offset + 26)
            .ok_or_else(|| malformed(None, "truncated local ZIP name length"))?,
    );
    let name_start =
        offset.checked_add(30).ok_or_else(|| malformed(None, "local ZIP name offset overflow"))?;
    let name_end = name_start
        .checked_add(name_len)
        .ok_or_else(|| malformed(None, "local ZIP name length overflow"))?;
    let raw = bytes
        .get(name_start..name_end)
        .ok_or_else(|| malformed(None, "truncated local ZIP name"))?;
    validate_raw_zip_name(raw, flags, "local header")
}

pub(super) fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let bytes = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

pub(super) fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let bytes = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

pub(super) fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

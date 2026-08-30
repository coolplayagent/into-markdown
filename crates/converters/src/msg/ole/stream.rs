use super::binary::{read_sector, to_usize, to_usize64, try_vec_capacity};
use super::chain::walk_chain_with_declared_tail;
use super::directory::DirectoryEntry;
use super::{CompoundBudget, CompoundCompatibility, ConversionError, EntryKind, limit, malformed};

pub(super) fn cfb_stream_memory_plan(
    entries: &[DirectoryEntry],
    minifat_sectors: usize,
    sector_size: usize,
    mini_sector_size: usize,
    mini_cutoff: u32,
) -> Result<u64, ConversionError> {
    let root = entries.first().ok_or_else(|| malformed("cfb/directory", "missing root entry"))?;
    let minifat = minifat_sectors
        .checked_mul(sector_size)
        .ok_or_else(|| limit("max_memory_bytes", "CFB miniFAT plan overflowed"))?;
    let root_bytes = to_usize64(root.size, "cfb/root")?;
    let mini_owners = root_bytes.div_ceil(mini_sector_size);
    let mut bytes = u64::try_from(minifat)
        .unwrap_or(u64::MAX)
        .checked_add(u64::try_from(root_bytes).unwrap_or(u64::MAX))
        .and_then(|value| value.checked_add(u64::try_from(mini_owners).unwrap_or(u64::MAX)))
        .ok_or_else(|| limit("max_memory_bytes", "CFB stream inventory plan overflowed"))?;
    for entry in entries.iter().filter(|entry| entry.kind == EntryKind::Stream) {
        let logical = to_usize64(entry.size, "cfb/stream")?;
        let capacity = if entry.size < u64::from(mini_cutoff) {
            logical.checked_next_multiple_of(mini_sector_size).ok_or_else(|| {
                limit("max_memory_bytes", "CFB mini-stream capacity plan overflowed")
            })?
        } else {
            logical
        };
        bytes = bytes
            .checked_add(u64::try_from(capacity).unwrap_or(u64::MAX))
            .ok_or_else(|| limit("max_memory_bytes", "CFB stream memory plan overflowed"))?;
    }
    Ok(bytes)
}
pub(super) struct MiniStreamContext<'a> {
    pub(super) minifat: &'a [u32],
    pub(super) root: &'a [u8],
    pub(super) owners: &'a mut [bool],
    pub(super) mini_size: usize,
    pub(super) compatibility: CompoundCompatibility,
    pub(super) pending_tails: &'a mut Vec<u32>,
}

pub(super) fn read_mini_stream(
    entry: &DirectoryEntry,
    part: &str,
    context: &mut MiniStreamContext<'_>,
) -> Result<Vec<u8>, ConversionError> {
    let expected = to_usize64(entry.size, part)?.div_ceil(context.mini_size);
    let (chain, pending_tail) = walk_chain_with_declared_tail(
        entry.start,
        context.minifat,
        context.owners.len(),
        Some(u32::try_from(expected).unwrap_or(u32::MAX)),
        part,
        context.compatibility == CompoundCompatibility::LegacyOfficeBestEffort,
    )?;
    context.pending_tails.extend(pending_tail);
    let capacity = expected
        .checked_mul(context.mini_size)
        .ok_or_else(|| limit("max_decompressed_bytes", "mini-stream capacity overflowed"))?;
    let mut output = try_vec_capacity(capacity, "CFB mini stream")?;
    for id in chain {
        let index = to_usize(id)?;
        if context.owners.get(index).copied().unwrap_or(true) {
            return Err(malformed(part, "mini-sector overlaps another stream"));
        }
        context.owners[index] = true;
        let start = index
            .checked_mul(context.mini_size)
            .ok_or_else(|| limit("max_decompressed_bytes", "mini-sector offset overflowed"))?;
        output.extend_from_slice(
            context
                .root
                .get(start..start + context.mini_size)
                .ok_or_else(|| malformed(part, "mini-sector exceeds root mini stream"))?,
        );
    }
    output.truncate(to_usize64(entry.size, part)?);
    Ok(output)
}

pub(super) fn regular_stream_chain(
    entry: &DirectoryEntry,
    fat: &[u32],
    sector_count: usize,
    sector_size: usize,
    part: &str,
    compatibility: CompoundCompatibility,
) -> Result<(Vec<u32>, Option<u32>), ConversionError> {
    let count = to_usize64(entry.size, part)?.div_ceil(sector_size);
    walk_chain_with_declared_tail(
        entry.start,
        fat,
        sector_count,
        Some(u32::try_from(count).unwrap_or(u32::MAX)),
        part,
        compatibility == CompoundCompatibility::LegacyOfficeBestEffort,
    )
}

pub(super) fn concatenate(
    bytes: &[u8],
    sector_size: usize,
    chain: &[u32],
) -> Result<Vec<u8>, ConversionError> {
    let capacity = chain
        .len()
        .checked_mul(sector_size)
        .ok_or_else(|| limit("max_decompressed_bytes", "sector chain byte size overflowed"))?;
    let mut output = try_vec_capacity(capacity, "CFB sector chain")?;
    for id in chain {
        output.extend_from_slice(read_sector(bytes, sector_size, *id)?);
    }
    Ok(output)
}

pub(super) fn concatenate_regular_stream(
    bytes: &[u8],
    sector_size: usize,
    chain: &[u32],
    logical_size: u64,
    partial_sector: Option<usize>,
    part: &str,
) -> Result<(Vec<u8>, Option<bool>), ConversionError> {
    let logical_size = to_usize64(logical_size, part)?;
    let mut output = try_vec_capacity(logical_size, "CFB regular stream")?;
    let mut partial_tail_consumed = None;
    for (index, id) in chain.iter().enumerate() {
        let physical = to_usize(*id)?;
        if Some(physical) == partial_sector {
            if index + 1 != chain.len() || partial_tail_consumed.is_some() {
                return Err(malformed(part, "partial physical sector is not terminal"));
            }
            let remaining = logical_size
                .checked_sub(output.len())
                .ok_or_else(|| malformed(part, "stream chain exceeds declared size"))?;
            let start = physical
                .checked_add(1)
                .and_then(|value| value.checked_mul(sector_size))
                .ok_or_else(|| malformed(part, "partial sector offset overflowed"))?;
            let tail = bytes
                .get(start..)
                .ok_or_else(|| malformed(part, "partial sector is outside source bytes"))?;
            if remaining == 0 || remaining > tail.len() || remaining >= sector_size {
                return Err(malformed(
                    part,
                    "partial terminal sector does not satisfy the declared stream size",
                ));
            }
            output.extend_from_slice(&tail[..remaining]);
            partial_tail_consumed = Some(remaining == tail.len());
        } else {
            output.extend_from_slice(read_sector(bytes, sector_size, *id)?);
        }
    }
    if output.len() < logical_size {
        return Err(malformed(part, "stream chain is shorter than its declared size"));
    }
    output.truncate(logical_size);
    Ok((output, partial_tail_consumed))
}

pub(super) fn materialize_regular_stream<B: CompoundBudget + ?Sized>(
    bytes: &[u8],
    sector_size: usize,
    chain: &[u32],
    logical_size: u64,
    partial_sector: Option<usize>,
    part: &str,
    budget: &mut B,
) -> Result<(Vec<u8>, Option<bool>), ConversionError> {
    budget.cfb_expanded(logical_size)?;
    concatenate_regular_stream(bytes, sector_size, chain, logical_size, partial_sector, part)
}

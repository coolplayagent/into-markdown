use super::binary::{
    le16, le32, read_sector, to_usize, try_sized_vec, try_vec_capacity, validate_physical,
};
use super::chain::walk_chain;
use super::directory::{
    DirectoryEntry, assign_paths, cfb_directory_memory_plan, parse_directory, stable_path,
};
use super::ownership::{
    claim, fat_target_is_out_of_bounds, validate_fat_targets, validate_pending_tails,
};
use super::stream::{
    MiniStreamContext, cfb_stream_memory_plan, concatenate, concatenate_regular_stream,
    materialize_regular_stream, read_mini_stream, regular_stream_chain,
};
use super::{
    CompoundBudget, CompoundCompatibility, CompoundFile, CompoundMemory, CompoundRecoveries,
    CompoundRecovery, ConversionError, DIFAT, END, EntryKind, FAT, FREE, SIGNATURE, limit,
    malformed,
};

pub(super) fn open<B: CompoundBudget + ?Sized>(
    bytes: &[u8],
    budget: &mut B,
    compatibility: CompoundCompatibility,
) -> Result<CompoundFile, ConversionError> {
    Header::parse(bytes)?.open(bytes, budget, compatibility)
}

#[derive(Clone, Copy)]
pub(super) struct Header {
    major: u16,
    sector_size: usize,
    mini_sector_size: usize,
    directory_sectors: u32,
    fat_sectors: u32,
    first_directory: u32,
    mini_cutoff: u32,
    first_minifat: u32,
    minifat_sectors: u32,
    first_difat: u32,
    difat_sectors: u32,
}

struct AllocationState {
    sector_count: usize,
    stream_sector_count: usize,
    partial_stream_sector: Option<usize>,
    memory: CompoundMemory,
    recoveries: CompoundRecoveries,
    owners: Vec<bool>,
    fat: Vec<u32>,
}

struct StreamState {
    minifat: Vec<u32>,
    root_mini_stream: Vec<u8>,
    regular_tail_targets: Vec<u32>,
    mini_owners: Vec<bool>,
    mini_tail_targets: Vec<u32>,
}

impl Header {
    fn parse(bytes: &[u8]) -> Result<Self, ConversionError> {
        let header =
            bytes.get(..512).ok_or_else(|| malformed("cfb/header", "truncated CFB header"))?;
        if header[..8] != SIGNATURE {
            return Err(malformed("cfb/header", "invalid CFB signature"));
        }
        if header[8..24].iter().any(|byte| *byte != 0) {
            return Err(malformed("cfb/header", "non-zero CFB header CLSID"));
        }
        let major = le16(header, 26, "cfb/header")?;
        if le16(header, 28, "cfb/header")? != 0xfffe {
            return Err(malformed("cfb/header", "unsupported CFB byte order"));
        }
        let sector_shift = le16(header, 30, "cfb/header")?;
        if !matches!((major, sector_shift), (3, 9) | (4, 12)) {
            return Err(malformed("cfb/header", "inconsistent CFB version and sector shift"));
        }
        if le16(header, 32, "cfb/header")? != 6 {
            return Err(malformed("cfb/header", "unsupported CFB mini-sector shift"));
        }
        if header[34..40].iter().any(|byte| *byte != 0) {
            return Err(malformed("cfb/header", "non-zero reserved CFB header bytes"));
        }
        let directory_sectors = le32(header, 40, "cfb/header")?;
        if major == 3 && directory_sectors != 0 {
            return Err(malformed("cfb/header", "version 3 CFB declares directory sector count"));
        }
        let mini_cutoff = le32(header, 56, "cfb/header")?;
        if mini_cutoff != 4096 {
            return Err(malformed("cfb/header", "unsupported CFB mini-stream cutoff"));
        }
        Ok(Self {
            major,
            sector_size: 1_usize << sector_shift,
            mini_sector_size: 64,
            directory_sectors,
            fat_sectors: le32(header, 44, "cfb/header")?,
            first_directory: le32(header, 48, "cfb/header")?,
            mini_cutoff,
            first_minifat: le32(header, 60, "cfb/header")?,
            minifat_sectors: le32(header, 64, "cfb/header")?,
            first_difat: le32(header, 68, "cfb/header")?,
            difat_sectors: le32(header, 72, "cfb/header")?,
        })
    }

    fn open<B: CompoundBudget + ?Sized>(
        self,
        bytes: &[u8],
        budget: &mut B,
        compatibility: CompoundCompatibility,
    ) -> Result<CompoundFile, ConversionError> {
        let mut allocation = self.read_allocation_tables(bytes, budget, compatibility)?;
        let entries = self.read_directory(bytes, budget, compatibility, &mut allocation)?;
        let minifat = self.read_minifat(bytes, budget, &entries, &mut allocation)?;
        let mut stream_state = self.read_root_stream(
            bytes,
            budget,
            compatibility,
            &entries,
            minifat,
            &mut allocation,
        )?;
        let streams = self.read_streams(
            bytes,
            budget,
            compatibility,
            &entries,
            &mut allocation,
            &mut stream_state,
        )?;
        Ok(CompoundFile {
            entries,
            streams,
            recoveries: allocation.recoveries,
            _memory: allocation.memory.into_leases(),
        })
    }

    fn read_allocation_tables<B: CompoundBudget + ?Sized>(
        self,
        bytes: &[u8],
        budget: &mut B,
        compatibility: CompoundCompatibility,
    ) -> Result<AllocationState, ConversionError> {
        if bytes.len() < self.sector_size {
            return Err(malformed("cfb/header", "CFB file length is not sector aligned"));
        }
        let trailing_recovery = !bytes.len().is_multiple_of(self.sector_size);
        if trailing_recovery && compatibility == CompoundCompatibility::Strict {
            return Err(malformed("cfb/header", "CFB file length is not sector aligned"));
        }
        if self.major == 4 && bytes[512..self.sector_size].iter().any(|byte| *byte != 0) {
            return Err(malformed("cfb/header", "non-zero version 4 header padding"));
        }
        let sector_count = bytes.len() / self.sector_size - 1;
        let partial_stream_sector =
            (compatibility == CompoundCompatibility::LegacyOfficeBestEffort && trailing_recovery)
                .then_some(sector_count);
        let stream_sector_count = sector_count + usize::from(partial_stream_sector.is_some());
        let memory =
            CompoundMemory::new(cfb_initial_memory_plan(self, stream_sector_count)?, budget)?;
        let mut recoveries = CompoundRecoveries::default();
        if trailing_recovery {
            recoveries.insert(CompoundRecovery::TrailingFileBytes);
        }
        let mut owners = try_sized_vec(stream_sector_count, false, "CFB sector owner table")?;
        let (fat_sector_ids, difat_sector_ids) = self.read_difat(bytes, sector_count, budget)?;
        for id in &difat_sector_ids {
            claim(&mut owners, *id, "cfb/difat")?;
        }
        for id in &fat_sector_ids {
            claim(&mut owners, *id, "cfb/fat")?;
        }
        let fat_capacity = fat_sector_ids
            .len()
            .checked_mul(self.sector_size / 4)
            .ok_or_else(|| limit("max_decompressed_bytes", "CFB FAT capacity overflowed"))?;
        let mut fat = try_vec_capacity(fat_capacity, "CFB FAT")?;
        for id in &fat_sector_ids {
            fat.extend(
                read_sector(bytes, self.sector_size, *id)?
                    .chunks_exact(4)
                    .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])),
            );
        }
        validate_fat(
            &fat,
            &fat_sector_ids,
            &difat_sector_ids,
            sector_count,
            compatibility,
            &mut recoveries,
        )?;
        Ok(AllocationState {
            sector_count,
            stream_sector_count,
            partial_stream_sector,
            memory,
            recoveries,
            owners,
            fat,
        })
    }

    fn read_directory<B: CompoundBudget + ?Sized>(
        self,
        bytes: &[u8],
        budget: &mut B,
        compatibility: CompoundCompatibility,
        allocation: &mut AllocationState,
    ) -> Result<Vec<DirectoryEntry>, ConversionError> {
        let directory_expected = (self.major == 4).then_some(self.directory_sectors);
        let directory_chain = walk_chain(
            self.first_directory,
            &allocation.fat,
            allocation.stream_sector_count,
            directory_expected,
            "cfb/directory",
        )?;
        if directory_chain.is_empty() {
            return Err(malformed("cfb/directory", "CFB has no directory sector"));
        }
        for id in &directory_chain {
            claim(&mut allocation.owners, *id, "cfb/directory")?;
        }
        let directory_capacity = directory_chain
            .len()
            .checked_mul(self.sector_size)
            .ok_or_else(|| limit("max_memory_bytes", "CFB directory capacity overflowed"))?;
        allocation.memory.grow(cfb_directory_memory_plan(directory_capacity / 128)?, budget)?;
        let scratch = budget.cfb_memory(u64::try_from(directory_capacity).unwrap_or(u64::MAX))?;
        let directory_bytes = concatenate(bytes, self.sector_size, &directory_chain)?;
        let mut entries = parse_directory(
            &directory_bytes,
            self.major,
            budget,
            compatibility,
            &mut allocation.recoveries,
        )?;
        drop(directory_bytes);
        drop(scratch);
        assign_paths(&mut entries, budget)?;
        Ok(entries)
    }

    fn read_minifat<B: CompoundBudget + ?Sized>(
        self,
        bytes: &[u8],
        budget: &mut B,
        entries: &[DirectoryEntry],
        allocation: &mut AllocationState,
    ) -> Result<Vec<u32>, ConversionError> {
        let chain = if self.minifat_sectors == 0 {
            if !matches!(self.first_minifat, END | FREE) {
                return Err(malformed("cfb/minifat", "empty miniFAT has a start sector"));
            }
            Vec::new()
        } else {
            walk_chain(
                self.first_minifat,
                &allocation.fat,
                allocation.sector_count,
                Some(self.minifat_sectors),
                "cfb/minifat",
            )?
        };
        for id in &chain {
            claim(&mut allocation.owners, *id, "cfb/minifat")?;
        }
        allocation.memory.grow(
            cfb_stream_memory_plan(
                entries,
                chain.len(),
                self.sector_size,
                self.mini_sector_size,
                self.mini_cutoff,
            )?,
            budget,
        )?;
        let capacity = chain
            .len()
            .checked_mul(self.sector_size / 4)
            .ok_or_else(|| limit("max_memory_bytes", "CFB miniFAT capacity overflowed"))?;
        let mut minifat = try_vec_capacity(capacity, "CFB miniFAT")?;
        for id in &chain {
            minifat.extend(
                read_sector(bytes, self.sector_size, *id)?
                    .chunks_exact(4)
                    .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])),
            );
        }
        Ok(minifat)
    }

    fn read_root_stream<B: CompoundBudget + ?Sized>(
        self,
        bytes: &[u8],
        budget: &mut B,
        compatibility: CompoundCompatibility,
        entries: &[DirectoryEntry],
        minifat: Vec<u32>,
        allocation: &mut AllocationState,
    ) -> Result<StreamState, ConversionError> {
        let root =
            entries.first().ok_or_else(|| malformed("cfb/directory", "missing root entry"))?;
        let (root_chain, root_tail) = regular_stream_chain(
            root,
            &allocation.fat,
            allocation.sector_count,
            self.sector_size,
            "cfb/root",
            compatibility,
        )?;
        let mut regular_tail_targets =
            try_vec_capacity(entries.len(), "CFB regular stream tail targets")?;
        regular_tail_targets.extend(root_tail);
        for id in &root_chain {
            claim(&mut allocation.owners, *id, "cfb/root-mini-stream")?;
        }
        let (root_mini_stream, partial_tail) = materialize_regular_stream(
            bytes,
            self.sector_size,
            &root_chain,
            root.size,
            allocation.partial_stream_sector,
            "cfb/root",
            budget,
        )?;
        record_partial_tail(partial_tail, &mut allocation.recoveries);
        let mini_owners = try_sized_vec(
            root_mini_stream.len().div_ceil(self.mini_sector_size),
            false,
            "CFB mini-sector owner table",
        )?;
        Ok(StreamState {
            minifat,
            root_mini_stream,
            regular_tail_targets,
            mini_owners,
            mini_tail_targets: try_vec_capacity(entries.len(), "CFB mini stream tail targets")?,
        })
    }

    fn read_streams<B: CompoundBudget + ?Sized>(
        self,
        bytes: &[u8],
        budget: &mut B,
        compatibility: CompoundCompatibility,
        entries: &[DirectoryEntry],
        allocation: &mut AllocationState,
        state: &mut StreamState,
    ) -> Result<Vec<Option<Vec<u8>>>, ConversionError> {
        let mut streams = try_sized_vec(entries.len(), None, "CFB stream slots")?;
        for (index, entry) in
            entries.iter().enumerate().filter(|(_, entry)| entry.kind == EntryKind::Stream)
        {
            budget.cfb_expanded(entry.size)?;
            let part = stable_path(entries, index);
            let data = if entry.size < u64::from(self.mini_cutoff) {
                let mut context = MiniStreamContext {
                    minifat: &state.minifat,
                    root: &state.root_mini_stream,
                    owners: &mut state.mini_owners,
                    mini_size: self.mini_sector_size,
                    compatibility,
                    pending_tails: &mut state.mini_tail_targets,
                };
                read_mini_stream(entry, &part, &mut context)?
            } else {
                self.read_regular_stream(bytes, entry, &part, compatibility, allocation, state)?
            };
            streams[index] = Some(data);
        }
        validate_pending_tails(
            &state.regular_tail_targets,
            &allocation.owners,
            &allocation.fat,
            "cfb/stream-tail",
        )?;
        validate_pending_tails(
            &state.mini_tail_targets,
            &state.mini_owners,
            &state.minifat,
            "cfb/mini-stream-tail",
        )?;
        if !state.regular_tail_targets.is_empty() || !state.mini_tail_targets.is_empty() {
            allocation.recoveries.insert(CompoundRecovery::StreamChainTail);
        }
        Ok(streams)
    }

    fn read_regular_stream(
        self,
        bytes: &[u8],
        entry: &DirectoryEntry,
        part: &str,
        compatibility: CompoundCompatibility,
        allocation: &mut AllocationState,
        state: &mut StreamState,
    ) -> Result<Vec<u8>, ConversionError> {
        let (chain, pending_tail) = regular_stream_chain(
            entry,
            &allocation.fat,
            allocation.stream_sector_count,
            self.sector_size,
            part,
            compatibility,
        )?;
        state.regular_tail_targets.extend(pending_tail);
        for id in &chain {
            claim(&mut allocation.owners, *id, part)?;
        }
        let (data, partial_tail) = concatenate_regular_stream(
            bytes,
            self.sector_size,
            &chain,
            entry.size,
            allocation.partial_stream_sector,
            part,
        )?;
        record_partial_tail(partial_tail, &mut allocation.recoveries);
        Ok(data)
    }

    fn read_difat<B: CompoundBudget + ?Sized>(
        self,
        bytes: &[u8],
        sector_count: usize,
        budget: &mut B,
    ) -> Result<(Vec<u32>, Vec<u32>), ConversionError> {
        let mut fat_ids =
            try_vec_capacity(to_usize(self.fat_sectors)?, "CFB FAT sector identifiers")?;
        for offset in (76..512).step_by(4) {
            let id = le32(bytes, offset, "cfb/difat")?;
            if id != FREE {
                validate_physical(id, sector_count, "cfb/difat")?;
                fat_ids.push(id);
            }
        }
        let mut difat_ids =
            try_vec_capacity(to_usize(self.difat_sectors)?, "CFB DIFAT sector identifiers")?;
        let mut current = self.first_difat;
        for _ in 0..self.difat_sectors {
            budget.cfb_work(1)?;
            validate_physical(current, sector_count, "cfb/difat")?;
            difat_ids.push(current);
            let sector = read_sector(bytes, self.sector_size, current)?;
            for chunk in sector[..self.sector_size - 4].chunks_exact(4) {
                let id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                if id != FREE {
                    validate_physical(id, sector_count, "cfb/difat")?;
                    fat_ids.push(id);
                }
            }
            current = le32(sector, self.sector_size - 4, "cfb/difat")?;
        }
        if self.difat_sectors == 0 {
            if !matches!(current, END | FREE) {
                return Err(malformed("cfb/difat", "empty DIFAT chain has a start sector"));
            }
        } else if current != END {
            return Err(malformed("cfb/difat", "DIFAT chain is longer than declared"));
        }
        if fat_ids.len() != to_usize(self.fat_sectors)? {
            return Err(malformed("cfb/difat", "declared FAT sector count does not match DIFAT"));
        }
        Ok((fat_ids, difat_ids))
    }
}

fn validate_fat(
    fat: &[u32],
    fat_sector_ids: &[u32],
    difat_sector_ids: &[u32],
    sector_count: usize,
    compatibility: CompoundCompatibility,
    recoveries: &mut CompoundRecoveries,
) -> Result<(), ConversionError> {
    if fat.len() < sector_count {
        return Err(malformed("cfb/fat", "FAT does not address every physical sector"));
    }
    for id in fat_sector_ids {
        if fat.get(to_usize(*id)?).copied() != Some(FAT) {
            if compatibility == CompoundCompatibility::Strict {
                return Err(malformed("cfb/fat", "FAT sector is not marked FATSECT"));
            }
            recoveries.insert(CompoundRecovery::FatSectorMarker);
        }
    }
    for id in difat_sector_ids {
        if fat.get(to_usize(*id)?).copied() != Some(DIFAT) {
            return Err(malformed("cfb/difat", "DIFAT sector is not marked DIFSECT"));
        }
    }
    if compatibility == CompoundCompatibility::Strict {
        validate_fat_targets(&fat[..sector_count], sector_count)
    } else {
        if fat[..sector_count].iter().any(|value| fat_target_is_out_of_bounds(*value, sector_count))
        {
            recoveries.insert(CompoundRecovery::UnreachableFatTarget);
        }
        Ok(())
    }
}

fn record_partial_tail(consumed: Option<bool>, recoveries: &mut CompoundRecoveries) {
    if let Some(consumed_all_tail) = consumed {
        if consumed_all_tail {
            recoveries.remove(CompoundRecovery::TrailingFileBytes);
        }
        recoveries.insert(CompoundRecovery::PartialStreamSector);
    }
}

pub(super) fn cfb_initial_memory_plan(
    header: Header,
    sector_count: usize,
) -> Result<u64, ConversionError> {
    let sectors = u64::try_from(sector_count).unwrap_or(u64::MAX);
    let sector_size = u64::try_from(header.sector_size).unwrap_or(u64::MAX);
    let owners = sectors;
    let fat = u64::from(header.fat_sectors)
        .checked_mul(sector_size)
        .ok_or_else(|| limit("max_memory_bytes", "CFB FAT allocation plan overflowed"))?;
    let allocation_ids = u64::from(header.fat_sectors)
        .checked_add(u64::from(header.difat_sectors))
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| limit("max_memory_bytes", "CFB allocation-id plan overflowed"))?;
    // At most one already-owned set of chains and one adversarial overlapping chain coexist
    // before ownership validation rejects it. Each chain item is one u32 sector identifier.
    let chain_scratch = sectors
        .checked_mul(8)
        .ok_or_else(|| limit("max_memory_bytes", "CFB chain inventory plan overflowed"))?;
    owners
        .checked_add(fat)
        .and_then(|bytes| bytes.checked_add(allocation_ids))
        .and_then(|bytes| bytes.checked_add(chain_scratch))
        .ok_or_else(|| limit("max_memory_bytes", "CFB initial memory plan overflowed"))
}

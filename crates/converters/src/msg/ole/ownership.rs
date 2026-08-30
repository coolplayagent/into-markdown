use super::binary::{to_usize, validate_physical};
use super::{ConversionError, DIFAT, END, FAT, FREE, malformed};

pub(super) fn validate_pending_tails(
    targets: &[u32],
    owners: &[bool],
    allocation_table: &[u32],
    part: &str,
) -> Result<(), ConversionError> {
    for target in targets {
        let index = to_usize(*target)?;
        if owners.get(index).copied().unwrap_or(true) {
            return Err(malformed(part, "declared stream tail aliases an owned sector"));
        }
        if allocation_table.get(index).copied() != Some(FREE) {
            return Err(malformed(part, "declared stream tail points to a live orphan chain"));
        }
    }
    Ok(())
}

pub(super) fn validate_fat_targets(
    fat: &[u32],
    sector_count: usize,
) -> Result<(), ConversionError> {
    for value in fat {
        if !matches!(*value, FREE | END | FAT | DIFAT) {
            validate_physical(*value, sector_count, "cfb/fat")?;
        }
    }
    Ok(())
}

pub(super) fn fat_target_is_out_of_bounds(value: u32, sector_count: usize) -> bool {
    !matches!(value, FREE | END | FAT | DIFAT)
        && usize::try_from(value).map_or(true, |value| value >= sector_count)
}

pub(super) fn claim(owners: &mut [bool], id: u32, owner: &str) -> Result<(), ConversionError> {
    let slot =
        owners.get_mut(to_usize(id)?).ok_or_else(|| malformed(owner, "sector is out of bounds"))?;
    if *slot {
        return Err(malformed(owner, "sector overlaps another CFB chain"));
    }
    *slot = true;
    Ok(())
}

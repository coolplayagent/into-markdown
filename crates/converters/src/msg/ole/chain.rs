use super::{DIFAT, END, FAT, FREE};
use crate::msg::budget::malformed;
use into_markdown_core::ConversionError;

pub(super) fn walk_chain(
    start: u32,
    table: &[u32],
    addressable: usize,
    expected: Option<u32>,
    part: &str,
) -> Result<Vec<u32>, ConversionError> {
    walk_chain_with_declared_tail(start, table, addressable, expected, part, false)
        .map(|(chain, _)| chain)
}

pub(super) fn walk_chain_with_declared_tail(
    start: u32,
    table: &[u32],
    addressable: usize,
    expected: Option<u32>,
    part: &str,
    allow_declared_tail: bool,
) -> Result<(Vec<u32>, Option<u32>), ConversionError> {
    let expected = expected.map(to_usize).transpose()?;
    if expected == Some(0) {
        if !matches!(start, END | FREE) {
            return Err(malformed(part, "zero-length chain has a start sector"));
        }
        return Ok((Vec::new(), None));
    }
    if matches!(start, END | FREE | FAT | DIFAT) {
        return Err(malformed(part, "non-empty chain has an invalid start sector"));
    }
    let (length, pending_tail) =
        scan_chain(start, table, addressable, expected, part, allow_declared_tail)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| malformed(part, "sector chain inventory allocation failed"))?;
    let mut current = start;
    for _ in 0..length {
        output.push(current);
        current = *table
            .get(to_usize(current)?)
            .ok_or_else(|| malformed(part, "sector chain exceeds allocation table"))?;
    }
    Ok((output, pending_tail))
}

fn scan_chain(
    start: u32,
    table: &[u32],
    addressable: usize,
    expected: Option<usize>,
    part: &str,
    allow_declared_tail: bool,
) -> Result<(usize, Option<u32>), ConversionError> {
    let mut length = 0_usize;
    let mut current = start;
    loop {
        validate_physical(current, addressable, part)?;
        length = length
            .checked_add(1)
            .ok_or_else(|| malformed(part, "sector chain length overflowed"))?;
        if length > addressable {
            return Err(malformed(part, "sector chain contains a cycle"));
        }
        let next = *table
            .get(to_usize(current)?)
            .ok_or_else(|| malformed(part, "sector chain exceeds allocation table"))?;
        if allow_declared_tail && expected == Some(length) {
            if next == END {
                return Ok((length, None));
            }
            if matches!(next, FREE | FAT | DIFAT) {
                return Err(malformed(part, "declared stream tail enters a reserved marker"));
            }
            validate_physical(next, addressable, part)?;
            let mut prior = start;
            for _ in 0..length {
                if prior == next {
                    return Err(malformed(part, "declared stream tail forms a cycle"));
                }
                prior = *table
                    .get(to_usize(prior)?)
                    .ok_or_else(|| malformed(part, "sector chain exceeds allocation table"))?;
            }
            return Ok((length, Some(next)));
        }
        current = next;
        if current == END {
            break;
        }
        if matches!(current, FREE | FAT | DIFAT) {
            return Err(malformed(part, "sector chain enters a reserved marker"));
        }
    }
    if expected.is_some_and(|count| length != count) {
        return Err(malformed(part, "sector chain length does not match declared stream size"));
    }
    Ok((length, None))
}

fn validate_physical(id: u32, count: usize, part: &str) -> Result<(), ConversionError> {
    if to_usize(id)? >= count {
        return Err(malformed(part, "sector identifier is out of bounds"));
    }
    Ok(())
}

fn to_usize(value: u32) -> Result<usize, ConversionError> {
    usize::try_from(value).map_err(|_| malformed("cfb", "32-bit index cannot be represented"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::ErrorCode;

    #[test]
    fn mini_stream_chain_rejects_a_sector_beyond_declared_length() {
        let error = walk_chain(0, &[1, END], 2, Some(1), "msg/mini").unwrap_err();
        assert_eq!(error.code(), ErrorCode::Malformed);
    }

    #[test]
    fn regular_stream_chain_rejects_a_sector_beyond_declared_length() {
        let error = walk_chain(0, &[1, END], 2, Some(1), "msg/regular").unwrap_err();
        assert_eq!(error.code(), ErrorCode::Malformed);
    }

    #[test]
    fn exact_chain_allows_padding_in_the_final_sector() {
        assert_eq!(walk_chain(0, &[END], 1, Some(1), "msg/stream").unwrap(), vec![0]);
    }

    #[test]
    fn compatibility_can_ignore_only_a_pointer_after_the_declared_stream_end() {
        let (chain, pending) =
            walk_chain_with_declared_tail(0, &[1, FREE], 2, Some(1), "xls/stream", true).unwrap();
        assert_eq!(chain, vec![0]);
        assert_eq!(pending, Some(1));

        assert!(walk_chain_with_declared_tail(0, &[99], 1, Some(1), "xls/stream", true).is_err());
        assert!(walk_chain_with_declared_tail(0, &[0], 1, Some(1), "xls/stream", true).is_err());
        assert!(walk_chain_with_declared_tail(0, &[1, 0], 2, Some(2), "xls/stream", true).is_err());
        assert!(walk_chain_with_declared_tail(0, &[FREE], 1, Some(1), "xls/stream", true).is_err());
        assert!(walk_chain_with_declared_tail(0, &[FAT], 1, Some(1), "xls/stream", true).is_err());
        assert!(walk_chain_with_declared_tail(0, &[0], 2, Some(2), "xls/stream", true).is_err());
    }
}

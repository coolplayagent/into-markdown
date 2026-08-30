use super::{DIFAT, END, FAT, FREE};
use crate::msg::budget::malformed;
use into_markdown_core::ConversionError;
use std::collections::BTreeSet;

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
) -> Result<(Vec<u32>, bool), ConversionError> {
    let expected = expected.map(to_usize).transpose()?;
    if expected == Some(0) {
        if !matches!(start, END | FREE) {
            return Err(malformed(part, "zero-length chain has a start sector"));
        }
        return Ok((Vec::new(), false));
    }
    if matches!(start, END | FREE | FAT | DIFAT) {
        return Err(malformed(part, "non-empty chain has an invalid start sector"));
    }
    let mut output = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = start;
    loop {
        validate_physical(current, addressable, part)?;
        if !seen.insert(current) {
            return Err(malformed(part, "sector chain contains a cycle"));
        }
        output.push(current);
        if output.len() > addressable {
            return Err(malformed(part, "sector chain exceeds addressable sectors"));
        }
        let next = *table
            .get(to_usize(current)?)
            .ok_or_else(|| malformed(part, "sector chain exceeds allocation table"))?;
        if allow_declared_tail && expected == Some(output.len()) {
            return Ok((output, next != END));
        }
        current = next;
        if current == END {
            break;
        }
        if matches!(current, FREE | FAT | DIFAT) {
            return Err(malformed(part, "sector chain enters a reserved marker"));
        }
    }
    if expected.is_some_and(|count| output.len() != count) {
        return Err(malformed(part, "sector chain length does not match declared stream size"));
    }
    Ok((output, false))
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
        let (chain, recovered) =
            walk_chain_with_declared_tail(0, &[99], 1, Some(1), "xls/stream", true).unwrap();
        assert_eq!(chain, vec![0]);
        assert!(recovered);

        assert!(walk_chain_with_declared_tail(0, &[99], 2, Some(2), "xls/stream", true).is_err());
        assert!(walk_chain_with_declared_tail(0, &[0], 2, Some(2), "xls/stream", true).is_err());
    }
}

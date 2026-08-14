use super::{MAX_DEPENDENCIES, Specification, malformed};
use into_markdown_core::ConversionError;
use std::collections::BTreeSet;

const CPU_ARM64: u32 = 0x0100_000c;
const CPU_X86_64: u32 = 0x0100_0007;

pub(super) fn parse(bytes: &[u8], target: &str) -> Result<Specification, ConversionError> {
    match bytes.get(..4) {
        Some([0xca, 0xfe, 0xba, 0xbe]) => parse_fat(bytes, target, Endian::Big, false),
        Some([0xbe, 0xba, 0xfe, 0xca]) => parse_fat(bytes, target, Endian::Little, false),
        Some([0xca, 0xfe, 0xba, 0xbf]) => parse_fat(bytes, target, Endian::Big, true),
        Some([0xbf, 0xba, 0xfe, 0xca]) => parse_fat(bytes, target, Endian::Little, true),
        _ => parse_thin(bytes, target),
    }
}

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

fn parse_fat(
    bytes: &[u8],
    target: &str,
    endian: Endian,
    wide: bool,
) -> Result<Specification, ConversionError> {
    let count = usize::try_from(read32(bytes, 4, endian)?).map_err(|_| malformed())?;
    if count == 0 || count > MAX_DEPENDENCIES {
        return Err(malformed());
    }
    let entry_bytes = if wide { 32_usize } else { 20_usize };
    let header_end = 8_usize
        .checked_add(count.checked_mul(entry_bytes).ok_or_else(malformed)?)
        .ok_or_else(malformed)?;
    if header_end > bytes.len() {
        return Err(malformed());
    }
    let required_cpu = target_cpu(target)?;
    let mut ranges = Vec::new();
    let mut selected = None;
    for index in 0..count {
        let entry = 8 + index * entry_bytes;
        let cpu = read32(bytes, entry, endian)?;
        let (offset, size) = if wide {
            (read64(bytes, entry + 8, endian)?, read64(bytes, entry + 16, endian)?)
        } else {
            (
                u64::from(read32(bytes, entry + 8, endian)?),
                u64::from(read32(bytes, entry + 12, endian)?),
            )
        };
        let start = usize::try_from(offset).map_err(|_| malformed())?;
        let end = start
            .checked_add(usize::try_from(size).map_err(|_| malformed())?)
            .ok_or_else(malformed)?;
        if start < header_end
            || start >= end
            || end > bytes.len()
            || ranges
                .iter()
                .any(|(prior_start, prior_end)| start < *prior_end && *prior_start < end)
        {
            return Err(malformed());
        }
        ranges.push((start, end));
        if cpu == required_cpu && selected.replace((start, end)).is_some() {
            return Err(malformed());
        }
    }
    let (start, end) = selected.ok_or_else(malformed)?;
    parse_thin(&bytes[start..end], target)
}

fn parse_thin(bytes: &[u8], target: &str) -> Result<Specification, ConversionError> {
    let endian = match bytes.get(..4) {
        Some([0xcf, 0xfa, 0xed, 0xfe]) => Endian::Little,
        Some([0xfe, 0xed, 0xfa, 0xcf]) => Endian::Big,
        _ => return Err(malformed()),
    };
    if read32(bytes, 4, endian)? != target_cpu(target)? {
        return Err(malformed());
    }
    let commands = usize::try_from(read32(bytes, 16, endian)?).map_err(|_| malformed())?;
    let command_bytes = usize::try_from(read32(bytes, 20, endian)?).map_err(|_| malformed())?;
    let end = 32_usize.checked_add(command_bytes).ok_or_else(malformed)?;
    if end > bytes.len() || commands > MAX_DEPENDENCIES {
        return Err(malformed());
    }
    let mut needed = BTreeSet::new();
    let mut search = Vec::new();
    let mut cursor = 32_usize;
    for _ in 0..commands {
        let command = read32(bytes, cursor, endian)?;
        let size = usize::try_from(read32(bytes, cursor + 4, endian)?).map_err(|_| malformed())?;
        let next = cursor.checked_add(size).ok_or_else(malformed)?;
        if size < 8 || next > end {
            return Err(malformed());
        }
        if matches!(command, 0x0c | 0x20 | 0x8000_0018 | 0x8000_001f | 0x8000_0023) {
            needed.insert(command_string(bytes, cursor, next, endian)?);
        } else if command == 0x8000_001c {
            search.push(command_string(bytes, cursor, next, endian)?);
        }
        cursor = next;
    }
    if cursor != end {
        return Err(malformed());
    }
    Ok(Specification { needed, search })
}

fn command_string(
    bytes: &[u8],
    command: usize,
    end: usize,
    endian: Endian,
) -> Result<String, ConversionError> {
    let offset = usize::try_from(read32(bytes, command + 8, endian)?).map_err(|_| malformed())?;
    let start = command.checked_add(offset).ok_or_else(malformed)?;
    if offset < 12 || start >= end {
        return Err(malformed());
    }
    let rest = bytes.get(start..end).ok_or_else(malformed)?;
    let nul = rest.iter().position(|byte| *byte == 0).ok_or_else(malformed)?;
    let value = std::str::from_utf8(&rest[..nul]).map_err(|_| malformed())?;
    if value.is_empty() || value.len() > 1_024 || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(malformed());
    }
    Ok(value.to_owned())
}

fn target_cpu(target: &str) -> Result<u32, ConversionError> {
    match target {
        "aarch64-apple-darwin" => Ok(CPU_ARM64),
        // Retained for parser unit coverage even though the authority schema
        // currently publishes only the Apple Silicon runtime target.
        "x86_64-apple-darwin" => Ok(CPU_X86_64),
        _ => Err(malformed()),
    }
}

fn read32(bytes: &[u8], offset: usize, endian: Endian) -> Result<u32, ConversionError> {
    let value =
        bytes.get(offset..offset.checked_add(4).ok_or_else(malformed)?).ok_or_else(malformed)?;
    let value: [u8; 4] = value.try_into().map_err(|_| malformed())?;
    Ok(match endian {
        Endian::Little => u32::from_le_bytes(value),
        Endian::Big => u32::from_be_bytes(value),
    })
}

fn read64(bytes: &[u8], offset: usize, endian: Endian) -> Result<u64, ConversionError> {
    let value =
        bytes.get(offset..offset.checked_add(8).ok_or_else(malformed)?).ok_or_else(malformed)?;
    let value: [u8; 8] = value.try_into().map_err(|_| malformed())?;
    Ok(match endian {
        Endian::Little => u64::from_le_bytes(value),
        Endian::Big => u64::from_be_bytes(value),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thin(command: u32, endian: Endian, cpu: u32) -> Vec<u8> {
        let name = b"/usr/lib/libSystem.B.dylib\0";
        let size = (24 + name.len() + 7) & !7;
        let mut bytes = vec![0_u8; 32 + size];
        let write = |bytes: &mut [u8], offset, value: u32| match endian {
            Endian::Little => bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes()),
            Endian::Big => bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes()),
        };
        bytes[..4].copy_from_slice(match endian {
            Endian::Little => &[0xcf, 0xfa, 0xed, 0xfe],
            Endian::Big => &[0xfe, 0xed, 0xfa, 0xcf],
        });
        write(&mut bytes, 4, cpu);
        write(&mut bytes, 16, 1);
        write(&mut bytes, 20, u32::try_from(size).unwrap());
        write(&mut bytes, 32, command);
        write(&mut bytes, 36, u32::try_from(size).unwrap());
        write(&mut bytes, 40, 24);
        bytes[56..56 + name.len()].copy_from_slice(name);
        bytes
    }

    #[test]
    fn lazy_load_is_bound_for_little_and_big_endian_images() {
        for endian in [Endian::Little, Endian::Big] {
            let parsed =
                parse_thin(&thin(0x20, endian, CPU_ARM64), "aarch64-apple-darwin").unwrap();
            assert!(parsed.needed.contains("/usr/lib/libSystem.B.dylib"));
        }
    }

    #[test]
    fn fat_selects_exact_target_slice_and_rejects_bounds() {
        let slice = thin(0x20, Endian::Little, CPU_ARM64);
        let mut fat = vec![0_u8; 0x100 + slice.len()];
        fat[..4].copy_from_slice(&[0xca, 0xfe, 0xba, 0xbe]);
        fat[4..8].copy_from_slice(&1_u32.to_be_bytes());
        fat[8..12].copy_from_slice(&CPU_ARM64.to_be_bytes());
        fat[16..20].copy_from_slice(&0x100_u32.to_be_bytes());
        fat[20..24].copy_from_slice(&u32::try_from(slice.len()).unwrap().to_be_bytes());
        fat[0x100..].copy_from_slice(&slice);
        assert!(parse(&fat, "aarch64-apple-darwin").is_ok());
        fat[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse(&fat, "aarch64-apple-darwin").is_err());
    }
}

//! Linux smaps_rollup accounting, including swapped and explicit huge pages.
//!
//! Field definitions: https://docs.kernel.org/filesystems/proc.html

pub(super) fn process_gone(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

pub(super) fn proportional_bytes(rollup: &str) -> Result<u64, ()> {
    let mut fields = [None; 4];
    for line in rollup.lines() {
        let Some((name, value)) = line.split_once(':') else { continue };
        let index = match name {
            "Pss" => 0,
            "SwapPss" => 1,
            "Private_Hugetlb" => 2,
            "Shared_Hugetlb" => 3,
            _ => continue,
        };
        let mut tokens = value.split_ascii_whitespace();
        let amount = tokens.next().ok_or(())?.parse::<u64>().map_err(|_| ())?;
        if tokens.next() != Some("kB") || tokens.next().is_some() || fields[index].is_some() {
            return Err(());
        }
        fields[index] = Some(amount);
    }
    // Hugetlb is excluded from PSS. Its shared portion has no proportional
    // kernel counter, so charge that exceptional mapping conservatively.
    fields.into_iter().try_fold(0_u64, |sum, item| {
        sum.checked_add(item.ok_or(())?.checked_mul(1024).ok_or(())?).ok_or(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_proportional_memory_counts_shared_swap_and_huge_pages() {
        let rollup = "Rss: 4096 kB\nPss: 2048 kB\nSwapPss: 64 kB\nPrivate_Hugetlb: 32 kB\nShared_Hugetlb: 16 kB\n";
        assert_eq!(proportional_bytes(rollup), Ok((2048 + 64 + 32 + 16) * 1024));
        // Moving a peak into the past does not charge it to current workers.
        assert_eq!(
            proportional_bytes(&format!("VmHWM: 999999 kB\n{rollup}")),
            proportional_bytes(rollup)
        );
        for invalid in [
            rollup.replace("2048 kB", "2048 MB"),
            rollup.replace("2048 kB", "18446744073709551615 kB"),
            rollup.replace("SwapPss: 64 kB\n", ""),
            format!("{rollup}Pss: 1 kB\n"),
        ] {
            assert!(proportional_bytes(&invalid).is_err());
        }
        assert!(proportional_bytes("").is_err());
    }

    #[test]
    fn reaped_proc_descriptors_are_gone_but_permission_failures_remain_errors() {
        for code in [libc::ESRCH, libc::ENOENT] {
            assert!(process_gone(&std::io::Error::from_raw_os_error(code)));
        }
        assert!(!process_gone(&std::io::Error::from_raw_os_error(libc::EACCES)));
    }
}

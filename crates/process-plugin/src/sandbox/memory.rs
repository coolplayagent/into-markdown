//! Aggregate memory of the process group owned by one sandbox request.

#[cfg(any(target_os = "linux", test))]
mod linux;

#[cfg(target_os = "linux")]
pub(super) fn group_bytes(group: u32) -> Result<u64, ()> {
    let mut total = 0_u64;
    for entry in std::fs::read_dir("/proc").map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(value) => value,
            Err(error) if linux::process_gone(&error) => continue,
            Err(_) => return Err(()),
        };
        if stat_group(&stat)? != group {
            continue;
        }
        let rollup = match std::fs::read_to_string(entry.path().join("smaps_rollup")) {
            Ok(value) => value,
            Err(error) if linux::process_gone(&error) => continue,
            Err(_) => return Err(()),
        };
        // Current proportional memory counts shared pages once across workers.
        // Independent process high-water marks can occur at different times.
        total = total.checked_add(linux::proportional_bytes(&rollup)?).ok_or(())?;
    }
    Ok(total)
}

#[cfg(target_os = "linux")]
fn stat_group(stat: &str) -> Result<u32, ()> {
    // comm can contain spaces and parentheses; fields after its final ')' are
    // state, ppid, pgrp. A malformed kernel observation fails closed.
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(2))
        .ok_or(())?
        .parse()
        .map_err(|_| ())
}

#[cfg(target_os = "macos")]
pub(super) fn group_bytes(group: u32) -> Result<u64, ()> {
    // Bound the inventory independently of machine-wide process count. An
    // overfull group fails closed instead of silently omitting descendants.
    let mut pids = [0_i32; 4096];
    let capacity = i32::try_from(std::mem::size_of_val(&pids)).map_err(|_| ())?;
    // SAFETY: PROC_PGRP_ONLY accepts a process group and writes PID values into
    // the supplied, correctly aligned buffer of exactly `capacity` bytes.
    let bytes = unsafe {
        libc::proc_listpids(
            2, /* PROC_PGRP_ONLY, sys/proc_info.h */
            group,
            pids.as_mut_ptr().cast(),
            capacity,
        )
    };
    if bytes < 0 || bytes >= capacity || bytes % 4 != 0 {
        return Err(());
    }
    let mut total = 0_u64;
    for pid in &pids[..usize::try_from(bytes / 4).map_err(|_| ())?] {
        if *pid <= 0 {
            continue;
        }
        let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v2>::uninit();
        // SAFETY: the requested V2 layout matches the output buffer. A process
        // that exits between enumeration and inspection is accounted as gone.
        let result =
            unsafe { libc::proc_pid_rusage(*pid, libc::RUSAGE_INFO_V2, usage.as_mut_ptr().cast()) };
        if result != 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(());
        }
        // SAFETY: successful proc_pid_rusage initialized every field.
        let usage = unsafe { usage.assume_init() };
        total = total.checked_add(usage.ri_phys_footprint.max(usage.ri_resident_size)).ok_or(())?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn owned_group_accounts_for_the_live_child_and_disappears_after_reaping() {
        let mut child =
            std::process::Command::new("/bin/sleep").arg("10").process_group(0).spawn().unwrap();
        let group = child.id();
        let observed = (0..100).any(|_| {
            if group_bytes(group).unwrap() > 0 {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
            false
        });
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(observed);
        assert_eq!(group_bytes(group).unwrap(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn group_parser_preserves_spaces_and_parentheses_in_process_names() {
        assert_eq!(stat_group("42 (worker ) name) S 10 42 0 0"), Ok(42));
        assert_eq!(stat_group("42 bad stat"), Err(()));
    }
}

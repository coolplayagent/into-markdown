use super::{Policy, install_unix_limits};
use std::os::fd::{AsRawFd as _, FromRawFd as _};

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;
const READ_ACCESS: u64 =
    LANDLOCK_ACCESS_FS_EXECUTE | LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
const WRITE_ACCESS: u64 = LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_REMOVE_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_MAKE_CHAR
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_MAKE_SOCK
    | LANDLOCK_ACCESS_FS_MAKE_FIFO
    | LANDLOCK_ACCESS_FS_MAKE_BLOCK
    | LANDLOCK_ACCESS_FS_MAKE_SYM
    | LANDLOCK_ACCESS_FS_REFER
    | LANDLOCK_ACCESS_FS_TRUNCATE;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
    reserved: u32,
}

pub(super) fn install(policy: &Policy) -> Result<(), ()> {
    install_unix_limits(policy)?;
    // SAFETY: prctl is invoked with the documented constant and zero unused arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(());
    }
    install_landlock(policy)?;
    install_seccomp()
}

fn install_landlock(policy: &Policy) -> Result<(), ()> {
    let handled = READ_ACCESS | WRITE_ACCESS;
    let attr = RulesetAttr { handled_access_fs: handled };
    // SAFETY: pointers and sizes exactly match Linux's landlock ABI.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &raw const attr,
            std::mem::size_of::<RulesetAttr>(),
            0,
        )
    };
    if fd < 0 {
        // Querying the version distinguishes absent/disabled Landlock only for
        // diagnostics; this boundary fails closed in either case.
        // SAFETY: null/zero are required for the ABI version query.
        let _ = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        return Err(());
    }
    // SAFETY: successful create_ruleset returned an owned descriptor.
    let ruleset = unsafe { std::os::fd::OwnedFd::from_raw_fd(i32::try_from(fd).map_err(|_| ())?) };
    add_path(&ruleset, &policy.runtime_root, READ_ACCESS)?;
    for path in &policy.system_read_paths {
        add_path(&ruleset, path, READ_ACCESS)?;
    }
    add_path(&ruleset, &policy.temporary_root, READ_ACCESS | WRITE_ACCESS)?;
    // SAFETY: live ruleset descriptor and zero flags follow the Landlock ABI.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0) } != 0 {
        return Err(());
    }
    Ok(())
}

fn add_path(ruleset: &std::os::fd::OwnedFd, path: &std::path::Path, access: u64) -> Result<(), ()> {
    use std::os::fd::FromRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| ())?;
    // SAFETY: CString is live; O_PATH opens an authority-validated directory without reading it.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(());
    }
    // SAFETY: successful open returned an owned descriptor.
    let parent = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let attr =
        PathBeneathAttr { allowed_access: access, parent_fd: parent.as_raw_fd(), reserved: 0 };
    // SAFETY: live descriptors and exact path-beneath layout.
    if unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            LANDLOCK_RULE_PATH_BENEATH,
            &raw const attr,
            0,
        )
    } != 0
    {
        return Err(());
    }
    Ok(())
}

fn install_seccomp() -> Result<(), ()> {
    const BPF_LD_W_ABS: u16 = 0x20;
    const BPF_JMP_JEQ_K: u16 = 0x15;
    const BPF_JMP_JSET_K: u16 = 0x45;
    const BPF_RET_K: u16 = 0x06;
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    const AUDIT_ARCH: u32 = if cfg!(target_arch = "x86_64") { 0xc000_003e } else { 0xc000_00b7 };
    #[cfg(target_arch = "x86_64")]
    const LEGACY_FORK_SYSCALLS: &[libc::c_long] = &[libc::SYS_fork, libc::SYS_vfork];
    #[cfg(not(target_arch = "x86_64"))]
    const LEGACY_FORK_SYSCALLS: &[libc::c_long] = &[];
    let statement = |code, k| libc::sock_filter { code, jt: 0, jf: 0, k };
    let jump = |code, k, jt, jf| libc::sock_filter { code, jt, jf, k };
    let denied = [
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_ptrace,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_keyctl,
        libc::SYS_open_by_handle_at,
        libc::SYS_kill,
        libc::SYS_tkill,
        libc::SYS_pidfd_send_signal,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ];
    let mut filters = Vec::with_capacity(16 + (denied.len() + LEGACY_FORK_SYSCALLS.len()) * 2);
    filters.push(statement(BPF_LD_W_ABS, 4));
    filters.push(jump(BPF_JMP_JEQ_K, AUDIT_ARCH, 1, 0));
    filters.push(statement(BPF_RET_K, SECCOMP_RET_KILL_PROCESS));
    filters.push(statement(BPF_LD_W_ABS, 0));
    // clone is allowed only for threads in the existing process.
    filters.push(jump(BPF_JMP_JEQ_K, u32::try_from(libc::SYS_clone).map_err(|_| ())?, 0, 4));
    filters.push(statement(BPF_LD_W_ABS, 16));
    filters.push(jump(BPF_JMP_JSET_K, libc::CLONE_THREAD as u32, 1, 0));
    filters.push(statement(BPF_RET_K, SECCOMP_RET_ERRNO | libc::EPERM as u32));
    filters.push(statement(BPF_RET_K, SECCOMP_RET_ALLOW));
    // Returning ENOSYS makes modern libc fall back from clone3 for pthreads.
    filters.push(jump(BPF_JMP_JEQ_K, u32::try_from(libc::SYS_clone3).map_err(|_| ())?, 0, 1));
    filters.push(statement(BPF_RET_K, SECCOMP_RET_ERRNO | libc::ENOSYS as u32));
    // pthreads use tgkill internally; constrain it to this process so a
    // worker thread cannot signal the parent or another process.
    let process = std::process::id();
    filters.push(jump(BPF_JMP_JEQ_K, u32::try_from(libc::SYS_tgkill).map_err(|_| ())?, 0, 4));
    filters.push(statement(BPF_LD_W_ABS, 16));
    filters.push(jump(BPF_JMP_JEQ_K, process, 1, 0));
    filters.push(statement(BPF_RET_K, SECCOMP_RET_ERRNO | libc::EPERM as u32));
    filters.push(statement(BPF_RET_K, SECCOMP_RET_ALLOW));
    for syscall in denied.into_iter().chain(LEGACY_FORK_SYSCALLS.iter().copied()) {
        filters.push(jump(BPF_JMP_JEQ_K, u32::try_from(syscall).map_err(|_| ())?, 0, 1));
        filters.push(statement(BPF_RET_K, SECCOMP_RET_ERRNO | libc::EPERM as u32));
    }
    filters.push(statement(BPF_RET_K, SECCOMP_RET_ALLOW));
    let program = libc::sock_fprog {
        len: u16::try_from(filters.len()).map_err(|_| ())?,
        filter: filters.as_mut_ptr(),
    };
    // SAFETY: the program points to live classic-BPF instructions for the duration of prctl.
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &raw const program) }
        != 0
    {
        return Err(());
    }
    Ok(())
}

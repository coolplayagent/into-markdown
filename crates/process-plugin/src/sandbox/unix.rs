use crate::{PluginError, RuntimePolicy, ValidatedPlugin};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Command;

pub(super) fn prepare(
    command: &mut Command,
    plugin: &ValidatedPlugin,
    policy: &RuntimePolicy,
    directory: &Path,
) -> Result<(), PluginError> {
    let memory = policy.max_memory_bytes;
    let file = policy.max_file_bytes;
    let open_files = policy.max_open_files;
    command.process_group(0);
    #[cfg(target_os = "linux")]
    let landlock =
        linux::Landlock::prepare(&plugin.runtime_root, directory, &policy.read_only_roots)
            .map_err(|_| {
                PluginError::new(
                    crate::PluginErrorCode::SandboxUnavailable,
                    "Landlock setup failed",
                )
            })?;
    #[cfg(target_os = "linux")]
    let mut seccomp = linux::Seccomp::prepare(policy.allow_child_processes).map_err(|_| {
        PluginError::new(crate::PluginErrorCode::SandboxUnavailable, "seccomp setup failed")
    })?;
    #[cfg(target_os = "macos")]
    let seatbelt = macos::Seatbelt::prepare(
        &plugin.runtime_root,
        directory,
        &policy.read_only_roots,
        policy.allow_child_processes,
        policy.macos_compatibility_child,
    )
    .map_err(|_| {
        PluginError::new(crate::PluginErrorCode::SandboxUnavailable, "Seatbelt setup failed")
    })?;
    // SAFETY: on Linux, every allocation, path lookup, FD open, and BPF construction happened
    // above the fork; the closure performs only raw async-signal-safe syscalls on owned storage.
    // macOS sandbox_init is the platform's process activation API and receives a prebuilt profile.
    unsafe {
        command.pre_exec(move || {
            install_limits(memory, file, open_files)?;
            install_no_new_privileges()?;
            #[cfg(target_os = "linux")]
            {
                landlock.restrict()?;
                seccomp.install()?;
            }
            #[cfg(target_os = "macos")]
            seatbelt.install()?;
            Ok(())
        });
    }
    Ok(())
}

fn install_limits(_memory: u64, file: u64, open_files: u32) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    set_limit(libc::RLIMIT_AS, _memory)?;
    #[cfg(target_os = "macos")]
    set_limit(libc::RLIMIT_AS, 2 * 1024 * 1024 * 1024 * 1024)?;
    set_limit(libc::RLIMIT_FSIZE, file)?;
    set_limit(libc::RLIMIT_NOFILE, u64::from(open_files))?;
    set_limit(libc::RLIMIT_CORE, 0)
}

#[cfg(target_os = "linux")]
fn install_no_new_privileges() -> std::io::Result<()> {
    // Prevent privilege gain across the imminent plugin exec and every later exec.
    // SAFETY: prctl takes scalar arguments only for PR_SET_NO_NEW_PRIVS.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_no_new_privileges() -> std::io::Result<()> {
    // macOS has no PR_SET_NO_NEW_PRIVS equivalent. Seatbelt is installed
    // immediately below and the child receives an empty ambient environment.
    Ok(())
}

#[cfg(target_os = "macos")]
type RlimitResource = libc::c_int;
#[cfg(target_os = "linux")]
type RlimitResource = libc::__rlimit_resource_t;

fn set_limit(resource: RlimitResource, value: u64) -> std::io::Result<()> {
    let value = libc::rlim_t::try_from(value)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let limit = libc::rlimit { rlim_cur: value, rlim_max: value };
    // SAFETY: `limit` is initialized and the resource selector is one of the constants above.
    if unsafe { libc::setrlimit(resource, &raw const limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;

    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: u32 = 1;
    const EXECUTE: u64 = 1 << 0;
    const WRITE_FILE: u64 = 1 << 1;
    const READ_FILE: u64 = 1 << 2;
    const READ_DIR: u64 = 1 << 3;
    const REMOVE_DIR: u64 = 1 << 4;
    const REMOVE_FILE: u64 = 1 << 5;
    const MAKE_CHAR: u64 = 1 << 6;
    const MAKE_DIR: u64 = 1 << 7;
    const MAKE_REG: u64 = 1 << 8;
    const MAKE_SOCK: u64 = 1 << 9;
    const MAKE_FIFO: u64 = 1 << 10;
    const MAKE_BLOCK: u64 = 1 << 11;
    const MAKE_SYM: u64 = 1 << 12;
    const REFER: u64 = 1 << 13;
    const TRUNCATE: u64 = 1 << 14;
    const READ: u64 = EXECUTE | READ_FILE | READ_DIR;
    const WRITE: u64 = WRITE_FILE
        | REMOVE_DIR
        | REMOVE_FILE
        | MAKE_CHAR
        | MAKE_DIR
        | MAKE_REG
        | MAKE_SOCK
        | MAKE_FIFO
        | MAKE_BLOCK
        | MAKE_SYM
        | REFER
        | TRUNCATE;

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

    pub(super) struct Landlock {
        ruleset: OwnedFd,
    }

    impl Landlock {
        pub(super) fn prepare(
            runtime: &Path,
            temporary: &Path,
            read_only_roots: &[std::path::PathBuf],
        ) -> std::io::Result<Self> {
            let attr = RulesetAttr { handled_access_fs: READ | WRITE };
            // SAFETY: the kernel receives a correctly sized initialized ruleset structure.
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    &raw const attr,
                    std::mem::size_of::<RulesetAttr>(),
                    0,
                )
            };
            if fd < 0 {
                // SAFETY: null/zero is the documented ABI-version query and has no side effects.
                let _ = unsafe {
                    libc::syscall(
                        libc::SYS_landlock_create_ruleset,
                        std::ptr::null::<RulesetAttr>(),
                        0,
                        CREATE_RULESET_VERSION,
                    )
                };
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: a nonnegative return from create_ruleset is a newly owned FD.
            let ruleset =
                unsafe { OwnedFd::from_raw_fd(i32::try_from(fd).map_err(std::io::Error::other)?) };
            add_path(&ruleset, runtime, READ)?;
            add_path(&ruleset, temporary, READ | WRITE)?;
            for root in read_only_roots {
                add_path(&ruleset, root, READ)?;
            }
            for path in ["/lib", "/lib64", "/usr/lib", "/usr/lib64", "/etc/ld.so.cache"] {
                let path = Path::new(path);
                if path.exists() {
                    let access = if path.is_dir() { READ } else { READ_FILE };
                    add_path(&ruleset, path, access)?;
                }
            }
            Ok(Self { ruleset })
        }

        pub(super) fn restrict(&self) -> std::io::Result<()> {
            // SAFETY: `ruleset` remains an owned valid Landlock FD through this call.
            if unsafe {
                libc::syscall(libc::SYS_landlock_restrict_self, self.ruleset.as_raw_fd(), 0)
            } != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
    }

    fn add_path(ruleset: &OwnedFd, path: &Path, access: u64) -> std::io::Result<()> {
        let value = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        // SAFETY: `value` is NUL-terminated; open returns an independent descriptor.
        let fd = unsafe { libc::open(value.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a nonnegative open result is newly owned.
        let parent = unsafe { OwnedFd::from_raw_fd(fd) };
        let attr =
            PathBeneathAttr { allowed_access: access, parent_fd: parent.as_raw_fd(), reserved: 0 };
        // SAFETY: both FDs and the initialized path-beneath structure remain valid for the call.
        if unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset.as_raw_fd(),
                RULE_PATH_BENEATH,
                &raw const attr,
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) struct Seccomp {
        filters: Vec<libc::sock_filter>,
        process_instruction: usize,
        filter_count: u16,
    }

    impl Seccomp {
        pub(super) fn prepare(allow_child_processes: bool) -> std::io::Result<Self> {
            const LD_W_ABS: u16 = 0x20;
            const JMP_JEQ_K: u16 = 0x15;
            const JMP_JSET_K: u16 = 0x45;
            const RET_K: u16 = 0x06;
            const KILL: u32 = 0x8000_0000;
            const ALLOW: u32 = 0x7fff_0000;
            const ERRNO: u32 = 0x0005_0000;
            let arch = if cfg!(target_arch = "x86_64") { 0xc000_003e } else { 0xc000_00b7 };
            let stmt = |code, k| libc::sock_filter { code, jt: 0, jf: 0, k };
            let jump = |code, k, jt, jf| libc::sock_filter { code, jt, jf, k };
            let denied = [
                libc::SYS_socket,
                libc::SYS_socketpair,
                libc::SYS_connect,
                libc::SYS_bind,
                libc::SYS_listen,
                libc::SYS_accept,
                libc::SYS_accept4,
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
            let legacy: &[libc::c_long] = if allow_child_processes {
                &[]
            } else {
                #[cfg(target_arch = "x86_64")]
                {
                    &[libc::SYS_fork, libc::SYS_vfork]
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    &[]
                }
            };
            let mut filters = Vec::with_capacity(64 + denied.len() * 2);
            filters.push(stmt(LD_W_ABS, 4));
            filters.push(jump(JMP_JEQ_K, arch, 1, 0));
            filters.push(stmt(RET_K, KILL));
            filters.push(stmt(LD_W_ABS, 0));
            if !allow_child_processes {
                filters.push(jump(
                    JMP_JEQ_K,
                    u32::try_from(libc::SYS_clone).map_err(std::io::Error::other)?,
                    0,
                    4,
                ));
                filters.push(stmt(LD_W_ABS, 16));
                filters.push(jump(JMP_JSET_K, libc::CLONE_THREAD as u32, 1, 0));
                filters.push(stmt(RET_K, ERRNO | libc::EPERM as u32));
                filters.push(stmt(RET_K, ALLOW));
            }
            filters.push(jump(
                JMP_JEQ_K,
                u32::try_from(libc::SYS_clone3).map_err(std::io::Error::other)?,
                0,
                1,
            ));
            filters.push(stmt(RET_K, ERRNO | libc::ENOSYS as u32));
            let process_instruction;
            filters.push(jump(
                JMP_JEQ_K,
                u32::try_from(libc::SYS_tgkill).map_err(std::io::Error::other)?,
                0,
                4,
            ));
            filters.push(stmt(LD_W_ABS, 16));
            process_instruction = filters.len();
            filters.push(jump(JMP_JEQ_K, 0, 1, 0));
            filters.push(stmt(RET_K, ERRNO | libc::E2BIG as u32));
            filters.push(stmt(RET_K, ALLOW));
            for (index, syscall) in denied.into_iter().chain(legacy.iter().copied()).enumerate() {
                filters.push(jump(
                    JMP_JEQ_K,
                    u32::try_from(syscall).map_err(std::io::Error::other)?,
                    0,
                    1,
                ));
                filters.push(stmt(
                    RET_K,
                    ERRNO | u32::try_from(100 + index).map_err(std::io::Error::other)?,
                ));
            }
            filters.push(stmt(RET_K, ALLOW));
            let filter_count = u16::try_from(filters.len()).map_err(std::io::Error::other)?;
            Ok(Self { filters, process_instruction, filter_count })
        }

        pub(super) fn install(&mut self) -> std::io::Result<()> {
            // SAFETY: getpid takes no pointers and is async-signal-safe.
            self.filters[self.process_instruction].k = unsafe { libc::getpid() as u32 };
            let program =
                libc::sock_fprog { len: self.filter_count, filter: self.filters.as_mut_ptr() };
            // SAFETY: `program` points to initialized BPF storage that lives through prctl.
            if unsafe {
                libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &raw const program)
            } != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::fmt::Write as _;

    #[link(name = "sandbox")]
    unsafe extern "C" {
        fn sandbox_init(
            profile: *const libc::c_char,
            flags: u64,
            error: *mut *mut libc::c_char,
        ) -> i32;
        fn sandbox_free_error(error: *mut libc::c_char);
    }

    pub(super) struct Seatbelt {
        profile: std::ffi::CString,
    }

    impl Seatbelt {
        pub(super) fn prepare(
            runtime: &Path,
            temporary: &Path,
            read_only_roots: &[std::path::PathBuf],
            allow_child_processes: bool,
            compatibility_child: bool,
        ) -> std::io::Result<Self> {
            let mut ancestors = std::collections::BTreeSet::new();
            for root in std::iter::once(runtime)
                .chain(std::iter::once(temporary))
                .chain(read_only_roots.iter().map(std::path::PathBuf::as_path))
            {
                let mut ancestor = root.parent();
                while let Some(path) = ancestor {
                    if path != Path::new("/") {
                        ancestors.insert(escape(path)?);
                    }
                    ancestor = path.parent();
                }
            }
            let runtime = escape(runtime)?;
            let temporary = escape(temporary)?;
            let mut profile = if compatibility_child {
                String::from(
                    "(version 1)\n(deny default)\n(import \"system.sb\")\n(deny network*)\n(allow process-fork)\n(allow process-info*)\n(allow sysctl-read)\n(allow user-preference-read)\n(allow signal (target self))\n\
                     (allow mach-lookup\n\
                       (global-name \"com.apple.FontObjectsServer\")\n\
                       (global-name \"com.apple.fonts\")\n\
                       (global-name \"com.apple.system.opendirectoryd.libinfo\")\n\
                       (global-name \"com.apple.SystemConfiguration.configd\")\n\
                       (global-name \"com.apple.CoreServices.coreservicesd\")\n\
                       (global-name \"com.apple.DiskArbitration.diskarbitrationd\")\n\
                       (global-name \"com.apple.pasteboard.1\")\n\
                       (global-name \"com.apple.distributed_notifications@Uv3\")\n\
                       (global-name \"com.apple.tccd.system\")\n\
                       (global-name \"com.apple.windowserver.active\")\n\
                       (global-name \"com.apple.coreservices.launchservicesd\")\n\
                       (global-name \"com.apple.lsd.mapdb\")\n\
                       (global-name \"com.apple.lsd.modifydb\")\n\
                       (global-name \"com.apple.dock.server\")\n\
                       (global-name \"com.apple.iohideventsystem\")\n\
                       (global-name \"com.apple.windowmanager.server\")\n\
                       (global-name \"com.apple.CARenderServer\")\n\
                       (global-name \"com.apple.pbs.fetch_services\")\n\
                       (global-name \"com.apple.appkit.restoration_storage\")\n\
                       (global-name \"com.apple.coreservices.appleevents\")\n\
                       (global-name \"com.apple.touchbarserver.mig\")\n\
                       (global-name \"com.apple.window_proxies\"))\n\
                     (allow iokit-open-user-client\n\
                       (iokit-user-client-class \"IOHIDParamUserClient\")\n\
                       (iokit-user-client-class \"IOSurfaceRootUserClient\"))\n",
                )
            } else if allow_child_processes {
                String::from(
                    "(version 1)\n(deny default)\n(import \"dyld-support.sb\")\n(deny network*)\n(allow process-fork)\n(allow process-info*)\n(allow sysctl-read)\n(allow signal (target self))\n",
                )
            } else {
                String::from(
                    "(version 1)\n(deny default)\n(import \"dyld-support.sb\")\n(deny network*)\n(deny process-fork)\n(allow process-info*)\n(allow sysctl-read)\n(allow signal (target self))\n",
                )
            };
            if allow_child_processes {
                // Linux Landlock already grants execute within the private
                // request directory when child processes are declared. Match
                // that contract on macOS so an authenticated provider can
                // stage and execute its package-owned helper below the only
                // writable root; the inherited Seatbelt still denies network
                // and access outside the runtime/request trees.
                writeln!(
                    profile,
                    "(allow process-exec (subpath \"{runtime}\") (subpath \"{temporary}\"))"
                )
                .map_err(std::io::Error::other)?;
            } else {
                writeln!(profile, "(allow process-exec (subpath \"{runtime}\"))")
                    .map_err(std::io::Error::other)?;
            }
            writeln!(
                profile,
                "(allow file-read* (subpath \"{runtime}\") (subpath \"{temporary}\"))"
            )
            .map_err(std::io::Error::other)?;
            writeln!(profile, "(allow file-write* (subpath \"{temporary}\"))")
                .map_err(std::io::Error::other)?;
            if compatibility_child {
                // Compatibility children may derive one per-user, per-profile
                // socket name in /private/tmp. The nested worker applies the
                // exact literal; this outer profile grants only the fixed
                // uid-bound namespace.
                // SAFETY: geteuid has no arguments and no failure condition.
                let uid = unsafe { libc::geteuid() };
                writeln!(
                    profile,
                    "(allow file-read* file-write* \
                     (regex #\"^/private/tmp/OSL_PIPE_{uid}_SingleOfficeIPC_[0-9a-f]+$\"))\n\
                     (allow file-read-metadata file-write-data (literal \"/private/tmp\"))\n\
                     (allow network*\n\
                       (local unix-socket (regex #\"^/(private/)?tmp/OSL_PIPE_{uid}_SingleOfficeIPC_[0-9a-f]+$\"))\n\
                       (remote unix-socket (regex #\"^/(private/)?tmp/OSL_PIPE_{uid}_SingleOfficeIPC_[0-9a-f]+$\")))"
                )
                .map_err(std::io::Error::other)?;
                writeln!(
                    profile,
                    "(allow file-read* (literal \"/private/var/db/.AppleSetupDone\"))\n\
                     (allow file-issue-extension (subpath \"{runtime}\") (subpath \"{temporary}\"))"
                )
                .map_err(std::io::Error::other)?;
            }
            for root in ["/usr/lib", "/System/Library"] {
                writeln!(profile, "(allow file-read* (subpath \"{root}\"))")
                    .map_err(std::io::Error::other)?;
            }
            for root in read_only_roots {
                let root = escape(root)?;
                writeln!(profile, "(allow file-read* (subpath \"{root}\"))")
                    .map_err(std::io::Error::other)?;
            }
            for ancestor in ancestors {
                if compatibility_child {
                    writeln!(profile, "(allow file-read* (literal \"{ancestor}\"))")
                        .map_err(std::io::Error::other)?;
                } else {
                    writeln!(
                        profile,
                        "(allow file-read-metadata file-test-existence (literal \"{ancestor}\"))"
                    )
                    .map_err(std::io::Error::other)?;
                }
            }
            let profile =
                std::ffi::CString::new(profile).map_err(|_| std::io::ErrorKind::InvalidInput)?;
            Ok(Self { profile })
        }

        pub(super) fn install(&self) -> std::io::Result<()> {
            let mut error = std::ptr::null_mut();
            // SAFETY: the prebuilt NUL-terminated profile remains alive; `error` is writable.
            let result = unsafe { sandbox_init(self.profile.as_ptr(), 0, &raw mut error) };
            if !error.is_null() {
                // SAFETY: sandbox_init returned this allocation for the matching free API.
                unsafe { sandbox_free_error(error) };
            }
            if result != 0 {
                return Err(std::io::Error::other("sandbox_init failed"));
            }
            Ok(())
        }
    }

    fn escape(path: &Path) -> std::io::Result<String> {
        let value = path.to_str().ok_or(std::io::ErrorKind::InvalidInput)?;
        if value.bytes().any(|byte| byte == 0 || byte.is_ascii_control()) {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        Ok(value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

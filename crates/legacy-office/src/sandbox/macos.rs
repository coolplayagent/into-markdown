use super::{Policy, install_unix_limits};
use std::ffi::{CStr, CString};
use std::fmt::Write as _;

#[link(name = "sandbox")]
unsafe extern "C" {
    fn sandbox_init(profile: *const libc::c_char, flags: u64, error: *mut *mut libc::c_char)
    -> i32;
    fn sandbox_free_error(error: *mut libc::c_char);
}

pub(super) fn install(policy: &Policy) -> Result<(), ()> {
    install_unix_limits(policy)?;
    let profile = profile(policy)?;
    let profile = CString::new(profile).map_err(|_| ())?;
    let mut error = std::ptr::null_mut();
    // SAFETY: profile is a live NUL-terminated custom profile; error receives
    // an optional sandbox-owned diagnostic which is always released below.
    let result = unsafe { sandbox_init(profile.as_ptr(), 0, &raw mut error) };
    if !error.is_null() {
        // Do not emit the authority paths or native diagnostics over stderr.
        // SAFETY: sandbox_init returned this pointer for sandbox_free_error.
        unsafe {
            let _ = CStr::from_ptr(error);
            sandbox_free_error(error);
        }
    }
    if result != 0 {
        return Err(());
    }
    Ok(())
}

fn profile(policy: &Policy) -> Result<String, ()> {
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(deny network*)\n(deny process-exec)\n\
         (deny process-fork)\n(allow process-info*)\n(allow sysctl-read)\n\
         (allow mach-lookup (global-name \"com.apple.FontObjectsServer\"))\n\
         (allow mach-lookup (global-name \"com.apple.fonts\"))\n\
         (allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\"))\n\
         (allow signal (target self))\n",
    );
    for path in [&policy.runtime_root, &policy.temporary_root] {
        writeln!(profile, "(allow file-read* (subpath \"{}\"))", escape(path)?).map_err(|_| ())?;
    }
    for path in &policy.system_read_paths {
        writeln!(profile, "(allow file-read* (literal \"{}\"))", escape(path)?).map_err(|_| ())?;
    }
    writeln!(profile, "(allow file-write* (subpath \"{}\"))", escape(&policy.temporary_root)?)
        .map_err(|_| ())?;
    Ok(profile)
}

fn escape(path: &std::path::Path) -> Result<String, ()> {
    let value = path.to_str().ok_or(())?;
    if value.bytes().any(|byte| byte == 0 || byte.is_ascii_control()) {
        return Err(());
    }
    Ok(value.replace('\\', "\\\\").replace('"', "\\\""))
}

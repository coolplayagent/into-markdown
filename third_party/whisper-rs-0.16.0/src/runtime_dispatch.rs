use std::ffi::CString;
use std::path::Path;

const FORBIDDEN_LOADER_ENVIRONMENT: &[&str] =
    &["GGML_BACKEND_PATH", "LD_AUDIT", "LD_LIBRARY_PATH", "LD_PRELOAD"];

/// Remove the process working directory from subsequent Windows DLL resolution.
pub fn harden_library_search() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        const LOAD_LIBRARY_SEARCH_APPLICATION_DIR: u32 = 0x0000_0200;
        const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;
        unsafe extern "system" {
            fn SetDefaultDllDirectories(directory_flags: u32) -> i32;
        }
        // SAFETY: the flags are the documented process-wide search policy and carry
        // no pointers. The provider invokes this before loading an optional backend.
        if unsafe {
            SetDefaultDllDirectories(
                LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Ask GGML to CPUID-score backends from exactly one authenticated directory.
pub fn load_cpu_backends_from_path(path: &Path) -> std::io::Result<()> {
    if let Some(name) =
        FORBIDDEN_LOADER_ENVIRONMENT.iter().find(|name| std::env::var_os(name).is_some())
    {
        return Err(std::io::Error::other(format!("untrusted loader environment is set: {name}")));
    }
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;

    #[cfg(unix)]
    let bytes = path.as_os_str().as_bytes();
    #[cfg(windows)]
    let bytes = path
        .to_str()
        .ok_or_else(|| std::io::Error::other("GGML runtime path is not Unicode"))?
        .as_bytes();
    let path =
        CString::new(bytes).map_err(|_| std::io::Error::other("GGML runtime path contains NUL"))?;
    // SAFETY: GGML consumes the NUL-terminated path during this call and keeps
    // independently owned library handles for the selected backend.
    unsafe { whisper_rs_sys::ggml_backend_load_cpu_from_path(path.as_ptr()) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::FORBIDDEN_LOADER_ENVIRONMENT;

    #[test]
    fn loader_environment_contract_covers_ggml_and_elf_overrides() {
        assert_eq!(
            FORBIDDEN_LOADER_ENVIRONMENT,
            &["GGML_BACKEND_PATH", "LD_AUDIT", "LD_LIBRARY_PATH", "LD_PRELOAD"]
        );
    }
}

use super::{Target, VerifiedContainer};
use into_markdown_core::ConversionError;

#[cfg(target_os = "macos")]
use super::{Abi, VerifiedBundle, checked_join, explicit_directory, unavailable, validate_abi};
#[cfg(target_os = "macos")]
use into_markdown_core::ExecutionContext;
#[cfg(target_os = "macos")]
use sha2::{Digest as _, Sha256};
#[cfg(target_os = "macos")]
use std::fs::File;
#[cfg(target_os = "macos")]
use std::io::Read as _;
#[cfg(target_os = "macos")]
use std::path::Path;

#[cfg(target_os = "macos")]
const HASH_BUFFER_BYTES: usize = 64 * 1024;

pub(super) fn verified(target: &Target) -> Result<Option<VerifiedContainer>, ConversionError> {
    target
        .container
        .as_ref()
        .map(|container| {
            Ok(VerifiedContainer {
                format: container.format.clone(),
                image_relative: container.image_path.clone(),
                mount_relative: container.mount_path.clone(),
                kit_sha256: container.kit_sha256.clone(),
            })
        })
        .transpose()
}

#[cfg(target_os = "macos")]
pub(crate) fn validate_mounted(
    bundle: &VerifiedBundle,
    runtime_root: &Path,
    kit_library: &Path,
    install_root: &Path,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let Some(container) = &bundle.container else {
        return Ok(());
    };
    let mount = checked_join(runtime_root, &container.mount_relative)?;
    if container.format == "udif" {
        require_readonly_mount(&mount)?;
    } else if container.format != "zip" {
        return Err(unavailable("containerFormat"));
    }
    validate_abi(
        kit_library,
        &Abi {
            binary_format: "mach-o".into(),
            architecture: "aarch64".into(),
            library_identity: kit_library
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .ok_or_else(|| unavailable("containerAbi"))?
                .into(),
            required_export: "libreofficekit_hook_2".into(),
        },
        context,
    )?;
    if file_sha256(kit_library, context)? != container.kit_sha256 {
        return Err(unavailable("containerHash"));
    }
    explicit_directory(install_root).map_err(|_| unavailable("installRoot"))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn require_readonly_mount(path: &Path) -> Result<(), ConversionError> {
    use std::os::unix::ffi::OsStrExt as _;
    let value = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| unavailable("containerMount"))?;
    let mut status = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: value is a live path and status is initialized on success.
    if unsafe { libc::statfs(value.as_ptr(), status.as_mut_ptr()) } != 0 {
        return Err(unavailable("containerMount"));
    }
    // SAFETY: successful statfs initialized the structure.
    let status = unsafe { status.assume_init() };
    if status.f_flags & u32::try_from(libc::MNT_RDONLY).unwrap_or(1) == 0 {
        return Err(unavailable("containerWritable"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn file_sha256(path: &Path, context: &ExecutionContext) -> Result<String, ConversionError> {
    let mut file = File::open(path).map_err(|_| unavailable("containerIo"))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; HASH_BUFFER_BYTES].into_boxed_slice();
    loop {
        context.checkpoint()?;
        let count = file.read(&mut buffer).map_err(|_| unavailable("containerIo"))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

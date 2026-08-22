use super::{MAX_PATH_BYTES, SystemLibraryAuthority, unavailable};
use into_markdown_core::ConversionError;
use std::fs;
use std::path::{Component, Path, PathBuf};

const MACOS_SYSTEM_LIBRARIES: &[&str] = &[
    "/usr/lib/libSystem.B.dylib",
    "/usr/lib/libc++.1.dylib",
    "/usr/lib/libexslt.0.dylib",
    "/usr/lib/libiconv.2.dylib",
    "/usr/lib/libicucore.A.dylib",
    "/usr/lib/libobjc.A.dylib",
    "/usr/lib/libresolv.9.dylib",
    "/usr/lib/libsandbox.1.dylib",
    "/usr/lib/libxml2.2.dylib",
    "/usr/lib/libxslt.1.dylib",
    "/usr/lib/libz.1.dylib",
    "/System/Library/Frameworks/AVFoundation.framework/Versions/A/AVFoundation",
    "/System/Library/Frameworks/AppKit.framework/Versions/C/AppKit",
    "/System/Library/Frameworks/Carbon.framework/Versions/A/Carbon",
    "/System/Library/Frameworks/Cocoa.framework/Versions/A/Cocoa",
    "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation",
    "/System/Library/Frameworks/CoreGraphics.framework/Versions/A/CoreGraphics",
    "/System/Library/Frameworks/CoreMedia.framework/Versions/A/CoreMedia",
    "/System/Library/Frameworks/CoreServices.framework/Versions/A/CoreServices",
    "/System/Library/Frameworks/CoreText.framework/Versions/A/CoreText",
    "/System/Library/Frameworks/Foundation.framework/Versions/C/Foundation",
    "/System/Library/Frameworks/IOKit.framework/Versions/A/IOKit",
    "/System/Library/Frameworks/ImageIO.framework/Versions/A/ImageIO",
    "/System/Library/Frameworks/Kerberos.framework/Versions/A/Kerberos",
    "/System/Library/Frameworks/Metal.framework/Versions/A/Metal",
    "/System/Library/Frameworks/OpenCL.framework/Versions/A/OpenCL",
    "/System/Library/Frameworks/QuartzCore.framework/Versions/A/QuartzCore",
    "/System/Library/Frameworks/Security.framework/Versions/A/Security",
    "/System/Library/Frameworks/SystemConfiguration.framework/Versions/A/SystemConfiguration",
];

pub(super) fn explicit_directory(path: &Path) -> Result<PathBuf, ConversionError> {
    if !path.is_absolute() {
        return Err(unavailable("unsafePath"));
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| unavailable("runtimeNotPackaged"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || is_reparse(&metadata) {
        return Err(unavailable("unsafePath"));
    }
    let canonical = path.canonicalize().map_err(|_| unavailable("unsafePath"))?;
    if canonical != path {
        return Err(unavailable("unsafePath"));
    }
    Ok(canonical)
}

pub(super) fn explicit_regular_file(
    path: &Path,
    root: Option<&Path>,
) -> Result<PathBuf, ConversionError> {
    if !path.is_absolute() || root.is_some_and(|root| !path.starts_with(root)) {
        return Err(unavailable("unsafePath"));
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| unavailable("runtimeNotPackaged"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || is_reparse(&metadata) {
        return Err(unavailable("unsafePath"));
    }
    let canonical = path.canonicalize().map_err(|_| unavailable("unsafePath"))?;
    if canonical != path || root.is_some_and(|root| !canonical.starts_with(root)) {
        return Err(unavailable("unsafePath"));
    }
    Ok(canonical)
}

pub(super) fn checked_join(root: &Path, relative: &str) -> Result<PathBuf, ConversionError> {
    if !safe_relative(relative) {
        return Err(unavailable("unsafePath"));
    }
    let joined = root.join(relative);
    if !joined.starts_with(root) {
        return Err(unavailable("unsafePath"));
    }
    Ok(joined)
}

pub(super) fn system_library_path(
    library: &SystemLibraryAuthority,
    target: &str,
) -> Result<PathBuf, ConversionError> {
    if library.identity.is_empty()
        || library.identity.len() > MAX_PATH_BYTES
        || library.path.is_empty()
        || library.path.len() > MAX_PATH_BYTES
        || !library.identity.is_ascii()
        || !library.path.is_ascii()
        || library
            .identity
            .bytes()
            .chain(library.path.bytes())
            .any(|byte| byte.is_ascii_control() || byte == 0x7f)
    {
        return Err(unavailable("sandboxAuthority"));
    }
    match target {
        "aarch64-apple-darwin" => macos_system_library(library),
        "aarch64-unknown-linux-gnu" | "x86_64-unknown-linux-gnu" => {
            const SYSTEM_SONAMES: &[&str] = &[
                "ld-linux-aarch64.so.1",
                "ld-linux-x86-64.so.2",
                "libc.so.6",
                "libdl.so.2",
                "libgcc_s.so.1",
                "libm.so.6",
                "libpthread.so.0",
                "librt.so.1",
                "libstdc++.so.6",
            ];
            if !SYSTEM_SONAMES.contains(&library.identity.as_str())
                || Path::new(&library.path).file_name().and_then(|value| value.to_str())
                    != Some(library.identity.as_str())
            {
                return Err(unavailable("sandboxAuthority"));
            }
            explicit_regular_file(Path::new(&library.path), None)
                .map_err(|_| unavailable("sandboxAuthority"))
        }
        "x86_64-pc-windows-msvc" => {
            const SYSTEM_DLLS: &[&str] = &[
                "advapi32.dll",
                "bcrypt.dll",
                "comctl32.dll",
                "comdlg32.dll",
                "crypt32.dll",
                "dwmapi.dll",
                "gdi32.dll",
                "imm32.dll",
                "kernel32.dll",
                "msvcrt.dll",
                "ntdll.dll",
                "ole32.dll",
                "oleaut32.dll",
                "rpcrt4.dll",
                "secur32.dll",
                "setupapi.dll",
                "shell32.dll",
                "shlwapi.dll",
                "user32.dll",
                "ucrtbase.dll",
                "version.dll",
                "winmm.dll",
                "winspool.drv",
                "ws2_32.dll",
            ];
            let identity = library.identity.to_ascii_lowercase();
            let expected = format!(r"C:\Windows\System32\{identity}");
            if !(SYSTEM_DLLS.contains(&identity.as_str())
                || identity.starts_with("api-ms-win-") && identity.ends_with(".dll"))
                || !library.path.eq_ignore_ascii_case(&expected)
            {
                return Err(unavailable("sandboxAuthority"));
            }
            Ok(PathBuf::from(&library.path))
        }
        _ => Err(unavailable("sandboxAuthority")),
    }
}

fn macos_system_library(library: &SystemLibraryAuthority) -> Result<PathBuf, ConversionError> {
    // These identities are provided by the dyld shared cache and do not
    // necessarily have a standalone inode on current macOS releases.
    if library.identity != library.path
        || !MACOS_SYSTEM_LIBRARIES.contains(&library.identity.as_str())
    {
        return Err(unavailable("sandboxAuthority"));
    }
    Ok(PathBuf::from(&library.path))
}

pub(super) fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PATH_BYTES
        && value.is_ascii()
        && !value.bytes().any(|byte| byte.is_ascii_control() || byte == 0x7f)
        && !value.contains(['\\', '\0'])
        && value.split('/').all(|component| !matches!(component, "" | "." | ".."))
        && Path::new(value).components().all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(windows)]
pub(super) fn is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
pub(super) const fn is_reparse(_: &fs::Metadata) -> bool {
    false
}

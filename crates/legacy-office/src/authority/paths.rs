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
        return Err(unavailable("unsafePath:absolute"));
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| unavailable("runtimeNotPackaged"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || is_reparse(&metadata) {
        return Err(unavailable("unsafePath:type"));
    }
    #[cfg(not(windows))]
    let canonical = path.canonicalize().map_err(|_| unavailable("unsafePath"))?;
    #[cfg(windows)]
    let canonical = authenticated_windows_path(path, true)?;
    if canonical != path {
        return Err(unavailable("unsafePath:identity"));
    }
    Ok(canonical)
}

pub(super) fn explicit_regular_file(
    path: &Path,
    root: Option<&Path>,
) -> Result<PathBuf, ConversionError> {
    if !path.is_absolute() || root.is_some_and(|root| !path.starts_with(root)) {
        return Err(unavailable("unsafePath:containment"));
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| unavailable("runtimeNotPackaged"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || is_reparse(&metadata) {
        return Err(unavailable("unsafePath:type"));
    }
    #[cfg(not(windows))]
    let canonical = path.canonicalize().map_err(|_| unavailable("unsafePath"))?;
    #[cfg(windows)]
    let canonical = authenticated_windows_path(path, false)?;
    if canonical != path || root.is_some_and(|root| !canonical.starts_with(root)) {
        return Err(unavailable("unsafePath:identity"));
    }
    Ok(canonical)
}

#[cfg(windows)]
pub(crate) fn authenticated_windows_path(
    path: &Path,
    directory: bool,
) -> Result<PathBuf, ConversionError> {
    use std::mem::size_of;
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NAME_INFO, FileNameInfo, GetFileInformationByHandleEx,
    };

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(unavailable("unsafePath:components"));
    }
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .share_mode(0x1 | 0x2 | 0x4)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    let file = options.open(path).map_err(|_| unavailable("unsafePath:open"))?;
    let metadata = file.metadata().map_err(|_| unavailable("unsafePath:handleMetadata"))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(unavailable("unsafePath:handleType"));
    }
    // FILE_NAME_INFO comes directly from the authenticated handle. Unlike
    // GetFinalPathNameByHandleW, it does not reopen every ancestor and therefore
    // works when the AppContainer intentionally has no authority above the
    // manager-owned snapshot. Comparing the full volume-relative name still
    // detects a parent reparse redirection.
    let mut buffer = vec![0_u64; (64 * 1024) / size_of::<u64>()];
    let buffer_bytes = u32::try_from(buffer.len() * size_of::<u64>()).unwrap_or(u32::MAX);
    // SAFETY: the authenticated handle remains live and the aligned output buffer is writable.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FileNameInfo,
            buffer.as_mut_ptr().cast(),
            buffer_bytes,
        )
    } == 0
    {
        return Err(unavailable("unsafePath:finalName"));
    }
    // SAFETY: the successful call initialized a FILE_NAME_INFO header and the
    // bounded byte count below proves the variable UTF-16 tail is in `buffer`.
    let information = unsafe { &*buffer.as_ptr().cast::<FILE_NAME_INFO>() };
    let name_bytes = usize::try_from(information.FileNameLength).unwrap_or(usize::MAX);
    if name_bytes == 0
        || !name_bytes.is_multiple_of(2)
        || name_bytes > usize::try_from(buffer_bytes).unwrap_or(0) - size_of::<u32>()
    {
        return Err(unavailable("unsafePath:finalNameEncoding"));
    }
    // SAFETY: FILE_NAME_INFO stores `FileNameLength` bytes immediately after
    // its u32 length field; u16 alignment is satisfied by the u64 buffer.
    let name = unsafe {
        std::slice::from_raw_parts(
            std::ptr::addr_of!(information.FileName).cast::<u16>(),
            name_bytes / 2,
        )
    };
    let opened_name =
        String::from_utf16(name).map_err(|_| unavailable("unsafePath:finalNameEncoding"))?;
    let expected_name = windows_volume_relative_path(path)?;
    if !opened_name.eq_ignore_ascii_case(&expected_name) {
        return Err(unavailable("unsafePath:finalNameIdentity"));
    }
    Ok(path.to_path_buf())
}

#[cfg(windows)]
fn windows_volume_relative_path(path: &Path) -> Result<String, ConversionError> {
    let value = path.as_os_str().to_string_lossy();
    let normalized = if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(dos) = value.strip_prefix(r"\\?\") {
        dos.to_owned()
    } else {
        value.into_owned()
    };
    if let Some(unc) = normalized.strip_prefix(r"\\") {
        let mut components = unc.splitn(3, '\\');
        let server = components.next().unwrap_or_default();
        let share = components.next().unwrap_or_default();
        let relative = components.next().unwrap_or_default();
        if server.is_empty() || share.is_empty() || relative.is_empty() {
            return Err(unavailable("unsafePath:finalNameIdentity"));
        }
        return Ok(format!(r"\{relative}"));
    }
    let bytes = normalized.as_bytes();
    if bytes.len() < 3
        || bytes[1] != b':'
        || !bytes[0].is_ascii_alphabetic()
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return Err(unavailable("unsafePath:finalNameIdentity"));
    }
    Ok(normalized[2..].replace('/', r"\"))
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
                "bcryptprimitives.dll",
                "comctl32.dll",
                "comdlg32.dll",
                "crypt32.dll",
                "d2d1.dll",
                "d3d9.dll",
                "dbghelp.dll",
                "dwmapi.dll",
                "fontsub.dll",
                "gdi32.dll",
                "gdiplus.dll",
                "httpapi.dll",
                "imm32.dll",
                "iphlpapi.dll",
                "kernel32.dll",
                "mfplat.dll",
                "mfplay.dll",
                "mfreadwrite.dll",
                "mpr.dll",
                "msvcrt.dll",
                "ncrypt.dll",
                "netapi32.dll",
                "ntdll.dll",
                "ole32.dll",
                "oleaut32.dll",
                "oledlg.dll",
                "propsys.dll",
                "rpcrt4.dll",
                "secur32.dll",
                "setupapi.dll",
                "shell32.dll",
                "shlwapi.dll",
                "user32.dll",
                "ucrtbase.dll",
                "userenv.dll",
                "usp10.dll",
                "version.dll",
                "wer.dll",
                "winhttp.dll",
                "winmm.dll",
                "winspool.drv",
                "ws2_32.dll",
                "wsock32.dll",
            ];
            let identity = library.identity.to_ascii_lowercase();
            let expected = format!(r"C:\Windows\System32\{identity}");
            let api_set_dll = identity.starts_with("api-ms-win-")
                && Path::new(&identity)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("dll"));
            if !(SYSTEM_DLLS.contains(&identity.as_str()) || api_set_dll)
                || !library.path.eq_ignore_ascii_case(&expected)
            {
                return Err(unavailable("sandboxAuthority"));
            }
            Ok(PathBuf::from(&library.path))
        }
        _ => Err(unavailable("sandboxAuthority")),
    }
}

#[cfg(windows)]
pub(crate) fn authenticated_windows_system_path(path: &Path) -> Result<(), ConversionError> {
    let value = path.to_str().ok_or_else(|| unavailable("sandboxAuthority"))?;
    let identity = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| unavailable("sandboxAuthority"))?
        .to_ascii_lowercase();
    let authority = SystemLibraryAuthority { identity: identity.clone(), path: value.to_owned() };
    system_library_path(&authority, "x86_64-pc-windows-msvc")?;

    // API-set DLL names are loader contracts represented by the Windows API
    // schema, not standalone files in System32. Their exact System32 identity
    // is authenticated above; all concrete system DLLs retain handle-based
    // no-reparse authentication.
    if identity.starts_with("api-ms-win-") {
        return Ok(());
    }
    authenticated_windows_path(path, false).map(|_| ())
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::authenticated_windows_path;
    use std::fs;

    #[test]
    fn authenticates_non_ascii_files_and_directories_by_handle() {
        let temporary = tempfile::Builder::new()
            .prefix("into-md-office-path-")
            .tempdir()
            .expect("temporary directory");
        let directory = temporary.path().join("验证 目录");
        fs::create_dir(&directory).expect("create directory");
        let file = directory.join("运行时.bin");
        fs::write(&file, b"authenticated").expect("create file");

        assert_eq!(
            authenticated_windows_path(&directory, true).expect("directory authority"),
            directory
        );
        assert_eq!(authenticated_windows_path(&file, false).expect("file authority"), file);
    }

    #[test]
    fn rejects_parent_components_and_final_reparse_points() {
        use std::os::windows::fs::symlink_file;

        let temporary = tempfile::Builder::new()
            .prefix("into-md-office-path-")
            .tempdir()
            .expect("temporary directory");
        let file = temporary.path().join("runtime.bin");
        fs::write(&file, b"authenticated").expect("create file");
        assert!(
            authenticated_windows_path(&temporary.path().join("child/../runtime.bin"), false)
                .is_err()
        );

        let link = temporary.path().join("runtime-link.bin");
        if symlink_file(&file, &link).is_ok() {
            assert!(authenticated_windows_path(&link, false).is_err());
        }
    }
}

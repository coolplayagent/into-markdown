//! Audited Windows filesystem boundary for model installation transactions.
#![allow(unsafe_code, reason = "narrow Win32 filesystem and ACL boundary")]
#![allow(
    clippy::borrow_as_ptr,
    clippy::cast_possible_wrap,
    clippy::cast_ptr_alignment,
    reason = "Win32 bindings require raw out-pointers and traversal of API-defined aligned buffers"
)]

use crate::ModelManagerError;
use std::ffi::OsStr;
use std::fs::File;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_HANDLE_EOF, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_HANDLE,
    ERROR_MORE_DATA, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
#[cfg(test)]
use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
#[cfg(test)]
use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::{
    ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, EqualSid,
    GetAclInformation, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
    GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    SECURITY_ATTRIBUTES, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER, TokenOwner, TokenUser,
};
#[cfg(test)]
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STREAM_INFO, FileAttributeTagInfo,
    FileDispositionInfo, FileIdInfo, FileStreamInfo, FlushFileBuffers, GetDriveTypeW,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    GetVolumeInformationW, GetVolumePathNameW, MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING,
    READ_CONTROL, SetFileInformationByHandle, VOLUME_NAME_GUID,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn last_error() -> ModelManagerError {
    ModelManagerError::Io(std::io::Error::last_os_error())
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn private() -> Result<Self, ModelManagerError> {
        let user = current_user_sid_string()?;
        let text = format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{user})");
        let sddl = wide(OsStr::new(&text));
        let mut descriptor = null_mut();
        // SAFETY: NUL-terminated input and valid out pointer; LocalFree owns the returned buffer.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(last_error());
        }
        Ok(Self(descriptor))
    }

    fn attributes(&mut self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("structure size fits"),
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns the token handle.
        unsafe { CloseHandle(self.0) };
    }
}

fn current_user_sid() -> Result<(OwnedHandle, Vec<usize>), ModelManagerError> {
    let mut token = null_mut();
    // SAFETY: pseudo-process handle is valid and output pointer is writable.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error());
    }
    let token = OwnedHandle(token);
    let mut needed = 0;
    // SAFETY: null query obtains the required buffer length.
    unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut needed) };
    if needed < u32::try_from(size_of::<TOKEN_USER>()).unwrap() {
        return Err(last_error());
    }
    let words = (needed as usize).div_ceil(size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    // SAFETY: buffer has the exact size returned by the initial query.
    if unsafe {
        GetTokenInformation(token.0, TokenUser, buffer.as_mut_ptr().cast(), needed, &mut needed)
    } == 0
    {
        return Err(last_error());
    }
    Ok((token, buffer))
}

fn current_token_owner_sid(token: HANDLE) -> Result<Vec<usize>, ModelManagerError> {
    let mut needed = 0;
    // SAFETY: null query obtains the required buffer length for the live token.
    unsafe { GetTokenInformation(token, TokenOwner, null_mut(), 0, &mut needed) };
    if needed < u32::try_from(size_of::<TOKEN_OWNER>()).unwrap() {
        return Err(last_error());
    }
    let words = (needed as usize).div_ceil(size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    // SAFETY: aligned buffer has the size returned by the initial query.
    if unsafe {
        GetTokenInformation(token, TokenOwner, buffer.as_mut_ptr().cast(), needed, &mut needed)
    } == 0
    {
        return Err(last_error());
    }
    Ok(buffer)
}

fn current_user_sid_string() -> Result<String, ModelManagerError> {
    let (_token, buffer) = current_user_sid()?;
    // SAFETY: buffer contains TOKEN_USER returned by GetTokenInformation.
    let sid = unsafe { (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    let mut value = null_mut();
    // SAFETY: SID is live and output pointer is writable.
    if unsafe { ConvertSidToStringSidW(sid, &mut value) } == 0 {
        return Err(last_error());
    }
    let mut length = 0;
    // SAFETY: returned value is a NUL-terminated UTF-16 string.
    while unsafe { *value.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: the measured slice lies within the returned allocation.
    let result = String::from_utf16(unsafe { std::slice::from_raw_parts(value, length) })
        .map_err(|_| ModelManagerError::UnsafePath);
    // SAFETY: the conversion API allocates with LocalAlloc.
    unsafe { LocalFree(value.cast()) };
    result
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: descriptor was allocated by the matching Win32 conversion API.
        unsafe { LocalFree(self.0.cast()) };
    }
}

fn open(path: &Path, directory: bool, access: u32) -> Result<File, ModelManagerError> {
    open_with_share(path, directory, access, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
}

fn open_with_share(
    path: &Path,
    directory: bool,
    access: u32,
    share_mode: u32,
) -> Result<File, ModelManagerError> {
    let path_wide = wide(path.as_os_str());
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory { FILE_FLAG_BACKUP_SEMANTICS } else { FILE_ATTRIBUTE_NORMAL };
    // SAFETY: all pointers are valid for the duration of the call.
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            access | READ_CONTROL | if directory { GENERIC_READ } else { 0 },
            share_mode,
            null(),
            OPEN_EXISTING,
            flags,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    // SAFETY: handle is newly owned and valid.
    let file = unsafe { File::from_raw_handle(handle as RawHandle) };
    validate_handle(&file, directory, true)?;
    Ok(file)
}

fn validate_handle(
    file: &File,
    directory: bool,
    require_private_acl: bool,
) -> Result<(), ModelManagerError> {
    // SAFETY: fixed-size output and live handle.
    let mut tag: FILE_ATTRIBUTE_TAG_INFO = unsafe { zeroed() };
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileAttributeTagInfo,
            (&raw mut tag).cast(),
            u32::try_from(size_of::<FILE_ATTRIBUTE_TAG_INFO>()).unwrap(),
        )
    } == 0
    {
        return Err(last_error());
    }
    if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(ModelManagerError::UnsafePath);
    }
    // SAFETY: fixed-size output and live handle.
    let mut info = unsafe { zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(last_error());
    }
    if !directory && info.nNumberOfLinks != 1 {
        return Err(ModelManagerError::UnsafePath);
    }
    if require_private_acl {
        validate_acl(file)?;
    }
    // Named data streams on artifact/control files are rejected. Directory ADS
    // are not transaction namespace children and FileStreamInfo is not supported
    // for non-empty directory handles on all supported Windows filesystems; all
    // transaction-created component names separately reject `:`.
    if directory { Ok(()) } else { validate_no_alternate_streams(file) }
}

fn validate_acl(file: &File) -> Result<(), ModelManagerError> {
    let mut descriptor_raw = null_mut();
    let mut owner: PSID = null_mut();
    // SAFETY: live file handle and valid optional output pointers.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor_raw,
        )
    };
    if status != 0 {
        return Err(ModelManagerError::Io(std::io::Error::from_raw_os_error(status as i32)));
    }
    let descriptor = SecurityDescriptor(descriptor_raw);
    let (token, user_buffer) = current_user_sid()?;
    let owner_buffer = current_token_owner_sid(token.0)?;
    // SAFETY: the aligned token buffers contain TOKEN_USER/TOKEN_OWNER and all SIDs are live.
    let user = unsafe { (*(user_buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    let token_owner = unsafe { (*(owner_buffer.as_ptr().cast::<TOKEN_OWNER>())).Owner };
    // Windows can canonicalize an explicitly supplied owner to the elevated token's exact
    // TokenOwner SID. It is part of this process's authority; unrelated owners remain rejected.
    let owner_matches =
        unsafe { EqualSid(owner, user) } != 0 || unsafe { EqualSid(owner, token_owner) } != 0;
    let expected = SecurityDescriptor::private()?;
    let dacl_matches = dacl_bytes(descriptor.0)? == dacl_bytes(expected.0)?;
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor was returned by GetSecurityInfo and remains live.
    let ok = unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) };
    if ok == 0 {
        return Err(last_error());
    }
    if !owner_matches || !dacl_matches || control & SE_DACL_PROTECTED == 0 {
        return Err(ModelManagerError::DataDirectoryUnsafe);
    }
    Ok(())
}

fn validate_no_alternate_streams(file: &File) -> Result<(), ModelManagerError> {
    // FILE_STREAM_INFO is a variable-length linked list. Use an aligned backing
    // allocation and retry boundedly because the API does not return the required size.
    let mut bytes = 1024_usize;
    let storage = loop {
        if bytes > 1024 * 1024 {
            return Err(ModelManagerError::UnsafePath);
        }
        let words = bytes.div_ceil(size_of::<u64>());
        let mut storage = vec![0_u64; words];
        // SAFETY: aligned storage is writable for the declared byte length and the handle is live.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                FileStreamInfo,
                storage.as_mut_ptr().cast(),
                u32::try_from(storage.len() * size_of::<u64>()).unwrap(),
            )
        };
        if ok != 0 {
            break storage;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(code) if code == ERROR_HANDLE_EOF as i32 => return Ok(()),
            Some(code)
                if code == ERROR_MORE_DATA as i32 || code == ERROR_INSUFFICIENT_BUFFER as i32 =>
            {
                bytes = bytes.checked_mul(2).ok_or(ModelManagerError::UnsafePath)?;
            }
            _ => return Err(ModelManagerError::Io(error)),
        }
    };
    let buffer = unsafe {
        std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), storage.len() * size_of::<u64>())
    };
    let mut offset = 0_usize;
    loop {
        let header_end = offset
            .checked_add(size_of::<FILE_STREAM_INFO>())
            .ok_or(ModelManagerError::UnsafePath)?;
        if header_end > buffer.len() {
            return Err(ModelManagerError::UnsafePath);
        }
        // SAFETY: storage is aligned and offset values are validated before traversal.
        let info = unsafe { &*buffer.as_ptr().add(offset).cast::<FILE_STREAM_INFO>() };
        let name_bytes =
            usize::try_from(info.StreamNameLength).map_err(|_| ModelManagerError::UnsafePath)?;
        if name_bytes % size_of::<u16>() != 0 {
            return Err(ModelManagerError::UnsafePath);
        }
        let name_start = offset
            .checked_add(std::mem::offset_of!(FILE_STREAM_INFO, StreamName))
            .ok_or(ModelManagerError::UnsafePath)?;
        let name_end = name_start.checked_add(name_bytes).ok_or(ModelManagerError::UnsafePath)?;
        if name_end > buffer.len() {
            return Err(ModelManagerError::UnsafePath);
        }
        // SAFETY: UTF-16 stream name extent was checked and storage is suitably aligned.
        let name = unsafe {
            std::slice::from_raw_parts(
                buffer.as_ptr().add(name_start).cast::<u16>(),
                name_bytes / size_of::<u16>(),
            )
        };
        if name != "::$DATA".encode_utf16().collect::<Vec<_>>() {
            return Err(ModelManagerError::UnsafePath);
        }
        if info.NextEntryOffset == 0 {
            return Ok(());
        }
        let next =
            usize::try_from(info.NextEntryOffset).map_err(|_| ModelManagerError::UnsafePath)?;
        if next < size_of::<FILE_STREAM_INFO>() || next % size_of::<u64>() != 0 {
            return Err(ModelManagerError::UnsafePath);
        }
        offset = offset.checked_add(next).ok_or(ModelManagerError::UnsafePath)?;
    }
}

fn dacl_bytes(descriptor: PSECURITY_DESCRIPTOR) -> Result<Vec<u8>, ModelManagerError> {
    let mut present = 0;
    let mut defaulted = 0;
    let mut acl: *mut ACL = null_mut();
    // SAFETY: descriptor is live and all out pointers are writable.
    if unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted) } == 0
    {
        return Err(last_error());
    }
    if present == 0 || acl.is_null() {
        return Err(ModelManagerError::DataDirectoryUnsafe);
    }
    // SAFETY: fixed-size output and ACL returned from the descriptor.
    let mut size: ACL_SIZE_INFORMATION = unsafe { zeroed() };
    if unsafe {
        GetAclInformation(
            acl,
            (&raw mut size).cast(),
            u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).unwrap(),
            AclSizeInformation,
        )
    } == 0
    {
        return Err(last_error());
    }
    // SAFETY: AclBytesInUse is the validated byte length reported by GetAclInformation.
    Ok(unsafe { std::slice::from_raw_parts(acl.cast::<u8>(), size.AclBytesInUse as usize) }
        .to_vec())
}

pub(crate) fn create_private_directory(path: &Path) -> Result<(), ModelManagerError> {
    let mut descriptor = SecurityDescriptor::private()?;
    let attributes = descriptor.attributes();
    let path_wide = wide(path.as_os_str());
    // SAFETY: pointers remain valid through the call.
    if unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) } == 0 {
        return Err(last_error());
    }
    validate_private_directory(path)
}

pub(crate) fn ensure_private_root(path: &Path) -> Result<(), ModelManagerError> {
    match create_private_directory(path) {
        Ok(()) => {}
        Err(ModelManagerError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_private_directory(path)?;
        }
        Err(error) => return Err(error),
    }
    validate_volume(path)
}

pub(crate) fn create_private_file(path: &Path) -> Result<File, ModelManagerError> {
    create_private_file_with_share(path, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
}

fn create_private_file_with_share(path: &Path, share_mode: u32) -> Result<File, ModelManagerError> {
    let mut descriptor = SecurityDescriptor::private()?;
    let attributes = descriptor.attributes();
    let path_wide = wide(path.as_os_str());
    // SAFETY: pointers remain valid through the call.
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            share_mode,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    // SAFETY: handle is newly owned and valid.
    let file = unsafe { File::from_raw_handle(handle as RawHandle) };
    validate_handle(&file, false, true)?;
    Ok(file)
}

pub(crate) fn validate_private_directory(path: &Path) -> Result<(), ModelManagerError> {
    open(path, true, FILE_READ_ATTRIBUTES).map(drop)
}

pub(crate) fn validate_private_file(path: &Path) -> Result<(), ModelManagerError> {
    open(path, false, FILE_READ_ATTRIBUTES).map(drop)
}

pub(crate) fn create_lock_file(path: &Path) -> Result<File, ModelManagerError> {
    create_private_file_with_share(path, FILE_SHARE_READ | FILE_SHARE_WRITE)
}

pub(crate) fn open_lock_file(path: &Path) -> Result<File, ModelManagerError> {
    open_with_share(path, false, GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE)
}

pub(crate) fn open_private_file_read(path: &Path) -> Result<File, ModelManagerError> {
    open(path, false, GENERIC_READ)
}

pub(crate) fn open_authenticated_snapshot_file_read(
    path: &Path,
) -> Result<File, ModelManagerError> {
    let path_wide = wide(path.as_os_str());
    // The process host authenticated the complete private snapshot and installed
    // its exact current-user plus AppContainer read-only DACL before launch. The
    // worker therefore validates object type, reparse state, link count, and ADS,
    // but must not require the transaction manager's current-user-only DACL.
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_READ | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    let file = unsafe { File::from_raw_handle(handle as RawHandle) };
    validate_handle(&file, false, false)?;
    Ok(file)
}

fn open_for_delete(path: &Path, directory: bool) -> Result<File, ModelManagerError> {
    let path_wide = wide(path.as_os_str());
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory { FILE_FLAG_BACKUP_SEMANTICS } else { FILE_ATTRIBUTE_NORMAL };
    // Omit FILE_SHARE_DELETE so the validated object cannot be renamed or
    // replaced between validation and handle-bound disposition.
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            DELETE | FILE_READ_ATTRIBUTES | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            flags,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    let file = unsafe { File::from_raw_handle(handle as RawHandle) };
    validate_handle(&file, directory, true)?;
    Ok(file)
}

fn mark_delete(file: &File) -> Result<(), ModelManagerError> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfo,
            (&raw const disposition).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).unwrap(),
        )
    } == 0
    {
        return Err(last_error());
    }
    Ok(())
}

pub(crate) fn delete_private_file(path: &Path) -> Result<(), ModelManagerError> {
    let file = open_for_delete(path, false)?;
    mark_delete(&file)
}

// A model bundle currently contains two runtime artifacts plus its state file.
// Keep ample headroom for format evolution while ensuring cleanup cannot be
// turned into an unbounded scan of an attacker-controlled directory.
pub(crate) const MAX_PRIVATE_DIRECTORY_FILES: usize = 64;

pub(crate) fn delete_flat_private_directory(path: &Path) -> Result<(), ModelManagerError> {
    // Retaining this no-share-delete handle binds enumeration and final removal
    // to the same validated directory object.
    let directory = open_for_delete(path, true)?;
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Err(ModelManagerError::UnsafePath);
        }
        if files.len() == MAX_PRIVATE_DIRECTORY_FILES {
            return Err(ModelManagerError::DataDirectoryUnsafe);
        }
        validate_private_file(&entry.path())?;
        files.push(entry.path());
    }
    // Deletion starts only after the bounded preflight has validated every
    // observed child, so an over-limit or non-file directory fails closed.
    for file in files {
        delete_private_file(&file)?;
    }
    mark_delete(&directory)
}

pub(crate) fn root_identity(path: &Path) -> Result<(String, String), ModelManagerError> {
    let file = open(path, true, FILE_READ_ATTRIBUTES)?;
    identity_from_handle(&file)
}

fn identity_from_handle(file: &File) -> Result<(String, String), ModelManagerError> {
    // SAFETY: fixed-size output and live handle.
    let mut id: FILE_ID_INFO = unsafe { zeroed() };
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&raw mut id).cast(),
            u32::try_from(size_of::<FILE_ID_INFO>()).unwrap(),
        )
    } == 0
    {
        return Err(last_error());
    }
    let mut buffer = vec![0_u16; 512];
    let length = loop {
        // SAFETY: writable UTF-16 buffer and live handle.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle() as HANDLE,
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).unwrap(),
                VOLUME_NAME_GUID,
            )
        };
        if length == 0 {
            return Err(last_error());
        }
        if (length as usize) < buffer.len() {
            break length as usize;
        }
        let required = (length as usize).checked_add(1).ok_or(ModelManagerError::UnsafePath)?;
        if required > 32_768 {
            return Err(ModelManagerError::UnsafePath);
        }
        buffer.resize(required, 0);
    };
    buffer.truncate(length);
    let canonical = String::from_utf16(&buffer).map_err(|_| ModelManagerError::UnsafePath)?;
    let fsid = format!("windows-volume-file:{:016x}:{}", id.VolumeSerialNumber, hex_id(&id));
    Ok((canonical, fsid))
}

pub(crate) struct RootGuard {
    file: File,
    identity: (String, String),
}

impl RootGuard {
    pub(crate) fn acquire(path: &Path) -> Result<Self, ModelManagerError> {
        let path_wide = wide(path.as_os_str());
        // Deliberately omit FILE_SHARE_DELETE: while this guard is live Windows
        // prevents rename/delete/recreation of the transaction root.
        // SAFETY: NUL-terminated path and valid arguments.
        let handle = unsafe {
            CreateFileW(
                path_wide.as_ptr(),
                FILE_READ_ATTRIBUTES | READ_CONTROL | GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        // SAFETY: handle is newly owned and valid.
        let file = unsafe { File::from_raw_handle(handle as RawHandle) };
        validate_handle(&file, true, true)?;
        let identity = identity_from_handle(&file)?;
        Ok(Self { file, identity })
    }

    pub(crate) fn validate(&self, path: &Path) -> Result<(), ModelManagerError> {
        let current = root_identity(path)?;
        if current != self.identity {
            return Err(ModelManagerError::DataDirectoryUnsafe);
        }
        // Keep the handle observably live across the comparison.
        let retained = identity_from_handle(&self.file)?;
        if retained != self.identity {
            return Err(ModelManagerError::DataDirectoryUnsafe);
        }
        Ok(())
    }
}

pub(crate) fn file_identity(file: &File) -> Result<String, ModelManagerError> {
    // SAFETY: fixed-size output and live retained artifact handle.
    let mut id: FILE_ID_INFO = unsafe { zeroed() };
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&raw mut id).cast(),
            u32::try_from(size_of::<FILE_ID_INFO>()).unwrap(),
        )
    } == 0
    {
        return Err(last_error());
    }
    Ok(format!("windows-volume-file:{:016x}:{}", id.VolumeSerialNumber, hex_id(&id)))
}

fn hex_id(id: &FILE_ID_INFO) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(id.FileId.Identifier.len() * 2);
    for byte in id.FileId.Identifier {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value
}

fn validate_volume(path: &Path) -> Result<(), ModelManagerError> {
    let path_wide = wide(path.as_os_str());
    let mut root = vec![0_u16; 1024];
    // SAFETY: valid input and writable output buffer.
    if unsafe {
        GetVolumePathNameW(
            path_wide.as_ptr(),
            root.as_mut_ptr(),
            u32::try_from(root.len()).unwrap(),
        )
    } == 0
    {
        return Err(last_error());
    }
    // SAFETY: root is NUL-terminated by GetVolumePathNameW.
    if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
        return Err(ModelManagerError::ComponentUnavailable);
    }
    let mut fs_name = [0_u16; 32];
    // SAFETY: all output buffers are valid and root is NUL-terminated.
    if unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            null_mut(),
            fs_name.as_mut_ptr(),
            u32::try_from(fs_name.len()).unwrap(),
        )
    } == 0
    {
        return Err(last_error());
    }
    let length = fs_name.iter().position(|unit| *unit == 0).unwrap_or(fs_name.len());
    let name = String::from_utf16_lossy(&fs_name[..length]);
    if name != "NTFS" && name != "ReFS" {
        return Err(ModelManagerError::ComponentUnavailable);
    }
    Ok(())
}

pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> Result<(), ModelManagerError> {
    let from = wide(from.as_os_str());
    let to = wide(to.as_os_str());
    // SAFETY: NUL-terminated path inputs remain valid through the call. Absence of
    // MOVEFILE_REPLACE_EXISTING and MOVEFILE_COPY_ALLOWED enforces same-volume no-replace.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(last_error());
    }
    Ok(())
}

pub(crate) fn flush_file(file: &File) -> Result<(), ModelManagerError> {
    // SAFETY: live file handle.
    if unsafe { FlushFileBuffers(file.as_raw_handle() as HANDLE) } == 0 {
        return Err(last_error());
    }
    Ok(())
}

pub(crate) fn flush_directory(path: &Path) -> Result<(), ModelManagerError> {
    // Some supported Windows versions/filesystems reject GENERIC_WRITE or
    // FlushFileBuffers for directory handles. Namespace durability is supplied
    // by every MoveFileExW(..., MOVEFILE_WRITE_THROUGH) commit point; attempt the
    // directory flush where the local filesystem exposes it.
    match open(path, true, GENERIC_READ | GENERIC_WRITE)
        .and_then(|directory| flush_file(&directory))
    {
        Ok(()) => Ok(()),
        Err(ModelManagerError::Io(ref error))
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(ERROR_INVALID_HANDLE as i32) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
pub(crate) fn harden_test_directory(path: &Path) -> Result<(), ModelManagerError> {
    let directory = match open(path, true, FILE_READ_ATTRIBUTES) {
        Ok(_) => return Ok(()),
        Err(ModelManagerError::DataDirectoryUnsafe) => {
            let path_wide = wide(path.as_os_str());
            // SAFETY: NUL-terminated path and valid flags; this handle is immediately owned.
            let handle = unsafe {
                CreateFileW(
                    path_wide.as_ptr(),
                    READ_CONTROL | WRITE_DAC,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                    null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(last_error());
            }
            // SAFETY: handle is newly owned and valid.
            unsafe { File::from_raw_handle(handle as RawHandle) }
        }
        Err(error) => return Err(error),
    };
    let descriptor = SecurityDescriptor::private()?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = null_mut();
    // SAFETY: descriptor is live and output pointers are writable.
    if unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted) }
        == 0
        || present == 0
        || dacl.is_null()
    {
        return Err(last_error());
    }
    // SAFETY: live directory handle and DACL owned by the live descriptor.
    let status = unsafe {
        SetSecurityInfo(
            directory.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null_mut(),
        )
    };
    if status != 0 {
        return Err(ModelManagerError::Io(std::io::Error::from_raw_os_error(status as i32)));
    }
    validate_private_directory(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn private_root_file_directory_and_no_replace_are_usable() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("models");
        create_private_directory(&root).unwrap();
        validate_volume(&root).unwrap();
        validate_private_directory(&root).unwrap();
        root_identity(&root).unwrap();
        let source = root.join("source");
        let mut file = create_private_file(&source).unwrap();
        file.write_all(b"model").unwrap();
        file.sync_all().unwrap();
        drop(file);
        validate_private_file(&source).unwrap();
        let destination = root.join("destination");
        rename_no_replace(&source, &destination).unwrap();
        let another = root.join("another");
        drop(create_private_file(&another).unwrap());
        assert!(rename_no_replace(&another, &destination).is_err());
        let directory = root.join("staging");
        create_private_directory(&directory).unwrap();
        drop(create_private_file(&directory.join("artifact")).unwrap());
        validate_private_directory(&directory).unwrap();
        flush_directory(&root).unwrap();
        delete_flat_private_directory(&directory).unwrap();
        assert!(!directory.exists());
        delete_private_file(&another).unwrap();
        assert!(!another.exists());
    }

    #[test]
    fn flat_cleanup_fails_closed_before_deleting_an_over_limit_directory() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("models");
        create_private_directory(&directory).unwrap();
        for index in 0..=MAX_PRIVATE_DIRECTORY_FILES {
            drop(create_private_file(&directory.join(format!("artifact-{index}"))).unwrap());
        }

        assert!(matches!(
            delete_flat_private_directory(&directory),
            Err(ModelManagerError::DataDirectoryUnsafe)
        ));
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), MAX_PRIVATE_DIRECTORY_FILES + 1);
    }

    #[test]
    fn existing_inherited_acl_root_is_rejected_then_test_hardening_is_stable() {
        let parent = tempfile::tempdir().unwrap();
        assert!(matches!(
            ensure_private_root(parent.path()),
            Err(ModelManagerError::DataDirectoryUnsafe)
        ));
        harden_test_directory(parent.path()).unwrap();
        ensure_private_root(parent.path()).unwrap();
        harden_test_directory(parent.path()).unwrap();
    }

    #[test]
    fn authenticated_snapshot_file_accepts_host_authorized_non_private_acl() {
        let parent = tempfile::tempdir().unwrap();
        let artifact = parent.path().join("model.onnx");
        std::fs::write(&artifact, b"model").unwrap();

        assert!(matches!(
            validate_private_file(&artifact),
            Err(ModelManagerError::DataDirectoryUnsafe)
        ));
        let file = open_authenticated_snapshot_file_read(&artifact).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 5);
    }

    #[test]
    fn file_stream_hardlink_identity_and_root_lifetime_are_enforced() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("models");
        create_private_directory(&root).unwrap();

        let first = root.join("first");
        let first_file = create_private_file(&first).unwrap();
        let first_identity = file_identity(&first_file).unwrap();
        drop(first_file);
        let second = root.join("second");
        let second_file = create_private_file(&second).unwrap();
        assert_ne!(first_identity, file_identity(&second_file).unwrap());
        drop(second_file);

        std::fs::write(format!("{}:untrusted", first.display()), b"stream").unwrap();
        assert!(matches!(validate_private_file(&first), Err(ModelManagerError::UnsafePath)));
        std::fs::remove_file(format!("{}:untrusted", first.display())).unwrap();
        let link = root.join("linked");
        std::fs::hard_link(&first, &link).unwrap();
        assert!(matches!(validate_private_file(&first), Err(ModelManagerError::UnsafePath)));
        std::fs::remove_file(link).unwrap();

        let guard = RootGuard::acquire(&root).unwrap();
        let moved = parent.path().join("moved-models");
        assert!(std::fs::rename(&root, &moved).is_err());
        guard.validate(&root).unwrap();
        drop(guard);
        std::fs::rename(&root, &moved).unwrap();
    }
}

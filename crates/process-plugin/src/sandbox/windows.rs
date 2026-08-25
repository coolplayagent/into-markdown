use crate::{PluginError, PluginErrorCode, RuntimePolicy, ValidatedPlugin};
use sha2::Digest as _;
use std::fs::File;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Component, Path};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree,
    SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
    DeriveAppContainerSidFromAppContainerName, GetAppContainerFolderPath,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, EqualSid, FreeSid, GetAce,
    GetSecurityDescriptorControl, GetTokenInformation, InitializeSecurityDescriptor,
    IsWellKnownSid, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
    SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
    SetSecurityDescriptorOwner, TOKEN_APPCONTAINER_INFORMATION, TOKEN_QUERY, TOKEN_USER,
    TokenAppContainerSid, TokenIsAppContainer, TokenUser, WinBuiltinAdministratorsSid,
    WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_ALL_ACCESS, FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW,
    MOVEFILE_WRITE_THROUGH, MoveFileExW, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    GetExitCodeProcess, InitializeProcThreadAttributeList, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

const INFINITE: u32 = u32::MAX;
const ERROR_ACCESS_DENIED_VALUE: u32 = 5;
const ERROR_SHARING_VIOLATION_VALUE: u32 = 32;
const ERROR_LOCK_VIOLATION_VALUE: u32 = 33;
const MOVE_RETRY_ATTEMPTS: u32 = 8;
const HRESULT_ALREADY_EXISTS: i32 = 0x8007_00B7_u32.cast_signed();
const HRESULT_FILE_NOT_FOUND: i32 = 0x8007_0002_u32.cast_signed();
const ACCESS_ALLOWED_ACE_TYPE_VALUE: u32 = 0;
const APP_READ_EXECUTE: u32 = 0x0012_00A9;

struct StoragePath(*mut u16);

impl Drop for StoragePath {
    fn drop(&mut self) {
        // SAFETY: GetAppContainerFolderPath returns CoTaskMemAlloc-owned storage.
        unsafe { windows_sys::Win32::System::Com::CoTaskMemFree(self.0.cast()) };
    }
}
const INHERITED_ACE_FLAG: u8 = 0x10;

pub(crate) fn provision(
    scope_plugin_identity: &str,
) -> Result<crate::WindowsSandboxAuthority, PluginError> {
    let suffix = format!("{:x}", sha2::Sha256::digest(scope_plugin_identity.as_bytes()));
    let profile_name = format!("into-markdown.plugin.{}", &suffix[..24]);
    let name = wide_text(&profile_name)?;
    let display = wide_text("into-markdown plugin sandbox")?;
    let description = wide_text("Zero-capability process-v1 plugin identity")?;
    let mut created = std::ptr::null_mut();
    // SAFETY: all UTF-16 strings are terminated and remain live, there are no capability entries,
    // and `created` is a writable out pointer owned by this call.
    let result = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            display.as_ptr(),
            description.as_ptr(),
            std::ptr::null(),
            0,
            &raw mut created,
        )
    };
    let sid = if result >= 0 {
        let created = AppContainerSid(created);
        sid_text(created.as_ptr())?
    } else if result == HRESULT_ALREADY_EXISTS {
        let derived = AppContainerSid::derive(&profile_name)?;
        sid_text(derived.as_ptr())?
    } else {
        return Err(unavailable("AppContainer profile provisioning failed"));
    };
    let derived = AppContainerSid::derive_verified(&profile_name, &sid)?;
    let storage_root = storage_path(derived.as_ptr())?;
    Ok(crate::WindowsSandboxAuthority { profile_name, sid, storage_root })
}

pub(crate) fn remove_profile(scope_plugin_identity: &str) -> Result<(), PluginError> {
    let suffix = format!("{:x}", sha2::Sha256::digest(scope_plugin_identity.as_bytes()));
    let profile_name = format!("into-markdown.plugin.{}", &suffix[..24]);
    let name = wide_text(&profile_name)?;
    // SAFETY: `name` is a live, terminated UTF-16 profile name.
    let result = unsafe { DeleteAppContainerProfile(name.as_ptr()) };
    if result < 0 && result != HRESULT_FILE_NOT_FOUND {
        return Err(unavailable("AppContainer profile cleanup failed"));
    }
    Ok(())
}

pub(crate) fn authorize_path(
    authority: &crate::WindowsSandboxAuthority,
    path: &Path,
) -> Result<(), PluginError> {
    let app = AppContainerSid::derive_verified(&authority.profile_name, &authority.sid)?;
    grant_runtime_path(path, app.as_ptr(), true)?;
    verify_runtime_tree(path, app.as_ptr(), true)
}

pub(crate) fn authorize_request_source(
    authority: &crate::WindowsSandboxAuthority,
    path: &Path,
) -> Result<(), PluginError> {
    let current = CurrentUser::open()?;
    let app = AppContainerSid::derive_verified(&authority.profile_name, &authority.sid)?;
    let mut access = [
        EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: current.sid().cast(),
            },
        },
        EXPLICIT_ACCESS_W {
            grfAccessPermissions: APP_READ_EXECUTE,
            grfAccessMode: SET_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: app.as_ptr().cast(),
            },
        },
    ];
    let mut acl = std::ptr::null_mut();
    // SAFETY: both token/profile-backed SIDs and the writable ACL output remain live.
    if unsafe { SetEntriesInAclW(2, access.as_mut_ptr(), std::ptr::null(), &raw mut acl) } != 0
        || acl.is_null()
    {
        return Err(unavailable("request source DACL construction failed"));
    }
    let mut wide = wide_path(path)?;
    // SAFETY: the path and exact two-entry ACL remain live for this synchronous installation.
    let installed = unsafe {
        SetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    // SAFETY: SetEntriesInAclW returned LocalAlloc-owned storage.
    unsafe { LocalFree(acl.cast()) };
    if installed != 0 {
        return Err(unavailable("request source DACL installation failed"));
    }
    verify_runtime_tree(path, app.as_ptr(), true)
}

pub(crate) fn verify_authorized_path(
    authority: &crate::WindowsSandboxAuthority,
    path: &Path,
) -> Result<(), PluginError> {
    let app = AppContainerSid::derive_verified(&authority.profile_name, &authority.sid)?;
    verify_runtime_tree(path, app.as_ptr(), true)
}

fn verify_runtime_tree(path: &Path, app_sid: PSID, root: bool) -> Result<(), PluginError> {
    use std::os::windows::fs::MetadataExt as _;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| unavailable("runtime snapshot metadata unavailable"))?;
    if metadata.file_attributes() & 0x400 != 0 {
        return Err(unavailable("runtime snapshot reparse point rejected"));
    }
    let current = CurrentUser::open()?;
    verify_acl(path, &[current.sid(), app_sid], root)?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)
            .map_err(|_| unavailable("runtime snapshot inventory unavailable"))?
        {
            let entry = entry.map_err(|_| unavailable("runtime snapshot entry unavailable"))?;
            verify_runtime_tree(&entry.path(), app_sid, false)?;
        }
    }
    Ok(())
}

fn grant_runtime_path(path: &Path, app_sid: PSID, directory: bool) -> Result<(), PluginError> {
    let mut old_acl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let mut wide = wide_path(path)?;
    // SAFETY: output pointers and the terminated path are valid.
    if unsafe {
        GetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut old_acl,
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    } != 0
        || old_acl.is_null()
        || descriptor.is_null()
    {
        return Err(unavailable("runtime snapshot DACL unavailable"));
    }
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: APP_READ_EXECUTE,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: if directory { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { 0 },
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: app_sid.cast(),
        },
    };
    let mut acl = std::ptr::null_mut();
    // SAFETY: old ACL, app SID, access entry, and output pointer remain live.
    let built = unsafe { SetEntriesInAclW(1, &raw const access, old_acl, &raw mut acl) };
    let installed = if built == 0 && !acl.is_null() {
        // SAFETY: the path and new ACL remain live.
        unsafe {
            SetNamedSecurityInfoW(
                wide.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl,
                std::ptr::null(),
            )
        }
    } else {
        1
    };
    if !acl.is_null() {
        // SAFETY: SetEntriesInAclW returned LocalAlloc-owned ACL storage.
        unsafe { LocalFree(acl.cast()) };
    }
    // SAFETY: GetNamedSecurityInfoW returned LocalAlloc-owned descriptor storage.
    unsafe { LocalFree(descriptor) };
    if installed != 0 {
        return Err(unavailable("runtime snapshot DACL installation failed"));
    }
    Ok(())
}

pub(crate) fn create_private_directory(path: &Path) -> Result<(), PluginError> {
    let wide = wide_path(path)?;
    let current_user = CurrentUser::open()?;
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: current_user.sid().cast(),
        },
    };
    let mut acl = std::ptr::null_mut();
    // SAFETY: access and token-backed SID remain live and acl is writable.
    if unsafe { SetEntriesInAclW(1, &raw const access, std::ptr::null(), &raw mut acl) } != 0
        || acl.is_null()
    {
        return Err(unavailable("private directory DACL construction failed"));
    }
    let mut descriptor = windows_sys::Win32::Security::SECURITY_DESCRIPTOR::default();
    // SAFETY: descriptor is writable and acl is a valid absolute ACL.
    let initialized = unsafe {
        InitializeSecurityDescriptor((&raw mut descriptor).cast(), 1) != 0
            && SetSecurityDescriptorOwner((&raw mut descriptor).cast(), current_user.sid(), 0) != 0
            && SetSecurityDescriptorDacl((&raw mut descriptor).cast(), 1, acl, 0) != 0
            && SetSecurityDescriptorControl(
                (&raw mut descriptor).cast(),
                SE_DACL_PROTECTED,
                SE_DACL_PROTECTED,
            ) != 0
    };
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: (&raw mut descriptor).cast(),
        bInheritHandle: 0,
    };
    // SAFETY: wide path, security descriptor, DACL, and attributes all remain live.
    let created =
        initialized && unsafe { CreateDirectoryW(wide.as_ptr(), &raw const attributes) } != 0;
    // SAFETY: SetEntriesInAclW returned LocalAlloc-owned storage.
    unsafe { LocalFree(acl.cast()) };
    if !created {
        return Err(unavailable("private directory creation failed"));
    }
    if let Err(error) = verify_private_path(path) {
        let _ = std::fs::remove_dir(path);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn rename_sibling_no_replace(
    directory: &File,
    source: &std::ffi::OsStr,
    destination: &std::ffi::OsStr,
) -> Result<(), PluginError> {
    // The caller retains and revalidates this authenticated directory handle
    // around publication. Absolute names are derived only from that handle.
    for name in [source, destination] {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(unavailable("plugin transaction name rejected"));
        }
    }
    let handle = directory.as_raw_handle().cast();
    let mut buffer = vec![0_u16; 32_768];
    // SAFETY: the pinned directory handle is live and the output buffer is writable.
    let written = unsafe {
        GetFinalPathNameByHandleW(
            handle,
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).unwrap_or(u32::MAX),
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if written == 0 || usize::try_from(written).unwrap_or(usize::MAX) >= buffer.len() {
        return Err(unavailable("plugin transaction directory identity unavailable"));
    }
    buffer.truncate(written as usize);
    let parent = std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer));
    let source = wide_path(&parent.join(source))?;
    let destination = wide_path(&parent.join(destination))?;
    // REPLACE_EXISTING is deliberately absent, so a raced destination is never overwritten.
    if !move_file_with_transient_retry(&source, &destination, MOVEFILE_WRITE_THROUGH) {
        return Err(unavailable("plugin transaction rename failed"));
    }
    Ok(())
}

pub(crate) fn replace_sibling(
    directory: &File,
    source: &std::ffi::OsStr,
    destination: &std::ffi::OsStr,
) -> Result<(), PluginError> {
    for name in [source, destination] {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(unavailable("plugin transaction name rejected"));
        }
    }
    let parent = final_directory_path(directory)?;
    let source = wide_path(&parent.join(source))?;
    let destination = wide_path(&parent.join(destination))?;
    // The pinned directory leases the namespace for the write-through replace.
    if !move_file_with_transient_retry(&source, &destination, MOVEFILE_WRITE_THROUGH | 0x1) {
        return Err(unavailable("plugin transaction replacement failed"));
    }
    Ok(())
}

fn final_directory_path(directory: &File) -> Result<std::path::PathBuf, PluginError> {
    let mut buffer = vec![0_u16; 32_768];
    // SAFETY: the pinned directory handle is live and the output buffer is writable.
    let written = unsafe {
        GetFinalPathNameByHandleW(
            directory.as_raw_handle().cast(),
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).unwrap_or(u32::MAX),
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if written == 0 || usize::try_from(written).unwrap_or(usize::MAX) >= buffer.len() {
        return Err(unavailable("plugin transaction directory identity unavailable"));
    }
    buffer.truncate(written as usize);
    Ok(std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer)))
}

pub(crate) fn move_between_no_replace(
    source_directory: &File,
    source: &std::ffi::OsStr,
    destination_directory: &File,
    destination: &std::ffi::OsStr,
) -> Result<(), PluginError> {
    for name in [source, destination] {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(unavailable("plugin transaction name rejected"));
        }
    }
    let source_path = final_directory_path(source_directory)?.join(source);
    let destination_path = final_directory_path(destination_directory)?.join(destination);
    let source = wide_path(&source_path)?;
    let destination = wide_path(&destination_path)?;
    // REPLACE_EXISTING is deliberately absent.
    if !move_file_with_transient_retry(&source, &destination, MOVEFILE_WRITE_THROUGH) {
        return Err(unavailable(format!(
            "plugin transaction cross-directory rename failed ({} -> {}): {}",
            source_path.display(),
            destination_path.display(),
            std::io::Error::last_os_error(),
        )));
    }
    Ok(())
}

fn move_file_with_transient_retry(source: &[u16], destination: &[u16], flags: u32) -> bool {
    for attempt in 0..MOVE_RETRY_ATTEMPTS {
        // SAFETY: both slices contain live NUL-terminated absolute paths for the call.
        if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } != 0 {
            return true;
        }
        // Antivirus/indexer handles can briefly deny a write-through rename even when every
        // product-owned handle permits deletion. Persistent locks and ACL failures still fail.
        let error = unsafe { GetLastError() };
        if !matches!(
            error,
            ERROR_ACCESS_DENIED_VALUE | ERROR_SHARING_VIOLATION_VALUE | ERROR_LOCK_VIOLATION_VALUE
        ) || attempt + 1 == MOVE_RETRY_ATTEMPTS
        {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1_u64 << attempt));
    }
    false
}

pub(crate) fn verify_private_path(path: &Path) -> Result<(), PluginError> {
    let current_user = CurrentUser::open()?;
    verify_acl_with_masks(path, &[current_user.sid()], &[FILE_ALL_ACCESS], true, true)
}

pub(crate) fn verify_private_child(path: &Path) -> Result<(), PluginError> {
    let current_user = CurrentUser::open()?;
    verify_acl_with_masks(path, &[current_user.sid()], &[FILE_ALL_ACCESS], false, true)
}

pub(crate) fn verify_trusted_parent(path: &Path) -> Result<(), PluginError> {
    const UNTRUSTED_MUTATION: u32 = 0x0000_0002
        | 0x0000_0004
        | 0x0000_0010
        | 0x0000_0040
        | 0x0000_0100
        | 0x0001_0000
        | 0x0004_0000
        | 0x0008_0000
        | 0x1000_0000
        | 0x4000_0000;
    let current_user = CurrentUser::open()?;
    let user = current_user.sid();
    let mut owner = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let mut path = wide_path(path)?;
    // SAFETY: all output pointers are writable and the path is terminated.
    let result = unsafe {
        GetNamedSecurityInfoW(
            path.as_mut_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if result != 0 || descriptor.is_null() || dacl.is_null() {
        if !descriptor.is_null() {
            // SAFETY: GetNamedSecurityInfoW returned LocalAlloc-owned storage.
            unsafe { LocalFree(descriptor) };
        }
        return Err(unavailable("trusted parent DACL unavailable"));
    }
    // SAFETY: owner and token SID remain live.
    let mut valid = !owner.is_null() && unsafe { EqualSid(owner, user) } != 0;
    // SAFETY: dacl points inside the live descriptor.
    let count = unsafe { (*dacl).AceCount };
    for index in 0..u32::from(count) {
        let mut raw = std::ptr::null_mut();
        // SAFETY: index is bounded by AceCount and raw is writable.
        if unsafe { GetAce(dacl, index, &raw mut raw) } == 0 || raw.is_null() {
            valid = false;
            break;
        }
        // Only the plain allow ACE is parsed as an authority grant. Plain and
        // object deny/audit ACEs do not grant authority; callback, object-allow,
        // and every unknown ACE layout fail closed rather than being skipped.
        let header = unsafe { &*raw.cast::<windows_sys::Win32::Security::ACE_HEADER>() };
        match u32::from(header.AceType) {
            ACCESS_ALLOWED_ACE_TYPE_VALUE => {}
            1 | 2 | 6 | 7 => continue,
            _ => {
                valid = false;
                break;
            }
        }
        // SAFETY: an allowed ACE contains ACCESS_ALLOWED_ACE and a trailing SID.
        let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = (&raw const ace.SidStart).cast_mut().cast();
        let authorized = unsafe {
            EqualSid(sid, user) != 0
                || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
        };
        if !authorized && ace.Mask & UNTRUSTED_MUTATION != 0 {
            valid = false;
            break;
        }
    }
    // SAFETY: descriptor is the LocalAlloc block returned above.
    unsafe { LocalFree(descriptor) };
    if !valid {
        return Err(unavailable("trusted parent grants unauthorized mutation"));
    }
    Ok(())
}

fn verify_acl(path: &Path, allowed: &[PSID], require_protected: bool) -> Result<(), PluginError> {
    let masks = allowed
        .iter()
        .enumerate()
        .map(|(index, _)| if index == 0 { FILE_ALL_ACCESS } else { APP_READ_EXECUTE })
        .collect::<Vec<_>>();
    verify_acl_with_masks(path, allowed, &masks, require_protected, false)
}

fn verify_acl_with_masks(
    path: &Path,
    allowed: &[PSID],
    masks: &[u32],
    require_protected: bool,
    allow_os_administrators: bool,
) -> Result<(), PluginError> {
    if allowed.is_empty() || allowed.len() != masks.len() {
        return Err(unavailable("plugin DACL authority is invalid"));
    }
    let user = allowed[0];
    let is_directory = std::fs::metadata(path)
        .map_err(|_| unavailable("plugin ACL metadata unavailable"))?
        .is_dir();
    let mut owner = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let mut path = wide_path(path)?;
    // SAFETY: all output pointers are writable and the terminated path remains live.
    let result = unsafe {
        GetNamedSecurityInfoW(
            path.as_mut_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if result != 0 || descriptor.is_null() || dacl.is_null() {
        if !descriptor.is_null() {
            // SAFETY: GetNamedSecurityInfoW allocated this descriptor with LocalAlloc.
            unsafe { LocalFree(descriptor) };
        }
        return Err(unavailable("plugin DACL unavailable"));
    }
    // SAFETY: owner and current token SID are valid while descriptor/token remain owned.
    let mut valid = !owner.is_null() && unsafe { EqualSid(owner, user) } != 0;
    let mut rejection = (!valid).then(|| "owner is not the current user".to_owned());
    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: descriptor is live and both outputs are writable.
    if unsafe { GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision) } == 0
        || require_protected && control & SE_DACL_PROTECTED == 0
    {
        valid = false;
        rejection.get_or_insert_with(|| "DACL is unavailable or not protected".to_owned());
    }
    // SAFETY: dacl points inside the live descriptor.
    let count = unsafe { (*dacl).AceCount };
    if usize::from(count) < allowed.len()
        || usize::from(count) > allowed.len() + usize::from(allow_os_administrators) * 2
    {
        valid = false;
        rejection.get_or_insert_with(|| format!("unexpected allowed ACE count {count}"));
    }
    let mut seen = vec![false; allowed.len()];
    let mut seen_system = false;
    let mut seen_administrators = false;
    for index in 0..u32::from(count) {
        let mut raw = std::ptr::null_mut();
        // SAFETY: index is bounded by AceCount and raw is writable.
        if unsafe { GetAce(dacl, index, &raw mut raw) } == 0 || raw.is_null() {
            valid = false;
            rejection.get_or_insert_with(|| format!("ACE {index} is unreadable"));
            break;
        }
        // SAFETY: GetAce returned an ACE inside the live ACL.
        let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
        if u32::from(ace.Header.AceType) != ACCESS_ALLOWED_ACE_TYPE_VALUE {
            valid = false;
            rejection.get_or_insert_with(|| format!("ACE {index} is not access-allowed"));
            break;
        }
        let sid = (&raw const ace.SidStart).cast_mut().cast();
        let identity = allowed.iter().position(|allowed| unsafe { EqualSid(sid, *allowed) } != 0);
        let (identity_name, expected_mask) = if let Some(identity) = identity {
            if seen[identity] {
                valid = false;
                rejection.get_or_insert_with(|| format!("ACE {index} duplicates an allowed SID"));
                break;
            }
            seen[identity] = true;
            ("user", masks[identity])
        } else if allow_os_administrators && unsafe { IsWellKnownSid(sid, WinLocalSystemSid) } != 0
        {
            if seen_system {
                valid = false;
                rejection.get_or_insert_with(|| format!("ACE {index} duplicates LocalSystem"));
                break;
            }
            seen_system = true;
            ("LocalSystem", FILE_ALL_ACCESS)
        } else if allow_os_administrators
            && unsafe { IsWellKnownSid(sid, WinBuiltinAdministratorsSid) } != 0
        {
            if seen_administrators {
                valid = false;
                rejection.get_or_insert_with(|| {
                    format!("ACE {index} duplicates Builtin Administrators")
                });
                break;
            }
            seen_administrators = true;
            ("Builtin Administrators", FILE_ALL_ACCESS)
        } else {
            valid = false;
            rejection.get_or_insert_with(|| format!("ACE {index} has an unauthorized SID"));
            break;
        };
        let expected_inheritance = if is_directory { 3 } else { 0 };
        if ace.Mask != expected_mask
            || ace.Header.AceFlags & !INHERITED_ACE_FLAG != expected_inheritance
        {
            valid = false;
            rejection.get_or_insert_with(|| {
                format!(
                    "ACE {index} for {identity_name} has mask 0x{:08x} and flags 0x{:02x}",
                    ace.Mask, ace.Header.AceFlags
                )
            });
            break;
        }
    }
    if !seen.into_iter().all(|value| value) {
        valid = false;
        rejection.get_or_insert_with(|| "an expected allowed SID is absent".to_owned());
    }
    // SAFETY: descriptor is the LocalAlloc block returned above.
    unsafe { LocalFree(descriptor) };
    if !valid {
        return Err(unavailable(format!(
            "plugin DACL rejected: {}",
            rejection.as_deref().unwrap_or("unknown ACL mismatch")
        )));
    }
    Ok(())
}

struct CurrentUser {
    _token: OwnedHandle,
    storage: Vec<usize>,
}

impl CurrentUser {
    fn open() -> Result<Self, PluginError> {
        let mut token = std::ptr::null_mut();
        // SAFETY: current-process pseudo-handle is valid and token is writable.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
            return Err(unavailable("current user token unavailable"));
        }
        // SAFETY: OpenProcessToken returned a newly owned handle.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut needed = 0_u32;
        // SAFETY: null-buffer sizing call with a valid token.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &raw mut needed,
            )
        };
        if needed == 0 {
            return Err(unavailable("current user identity unavailable"));
        }
        let word = size_of::<usize>();
        let words = usize::try_from(needed)
            .ok()
            .and_then(|bytes| bytes.checked_add(word - 1))
            .map(|bytes| bytes / word)
            .ok_or_else(|| unavailable("current user identity size overflow"))?;
        let mut storage = vec![0_usize; words];
        // SAFETY: aligned storage has at least `needed` writable bytes and token is valid.
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                storage.as_mut_ptr().cast(),
                needed,
                &raw mut needed,
            )
        } == 0
        {
            return Err(unavailable("current user identity unavailable"));
        }
        Ok(Self { _token: token, storage })
    }

    fn sid(&self) -> PSID {
        // SAFETY: storage is aligned, initialized by GetTokenInformation(TokenUser), and live.
        unsafe { (*self.storage.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

pub(crate) struct Child {
    process: OwnedHandle,
    job: OwnedHandle,
    stdin: Option<File>,
    stdout: Option<File>,
    stderr: Option<File>,
}

impl Child {
    pub(crate) fn take_stdin(&mut self) -> Option<File> {
        self.stdin.take()
    }
    pub(crate) fn take_stdout(&mut self) -> Option<File> {
        self.stdout.take()
    }
    pub(crate) fn take_stderr(&mut self) -> Option<File> {
        self.stderr.take()
    }

    pub(crate) fn try_wait(&mut self) -> Result<Option<bool>, ()> {
        // SAFETY: `process` owns a live or signalled kernel process handle for this entire call.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0_u32;
                // SAFETY: the signalled process handle is valid and `code` is writable.
                if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) } == 0 {
                    Err(())
                } else {
                    Ok(Some(code == 0))
                }
            }
            _ => Err(()),
        }
    }

    pub(crate) fn terminate(&mut self) {
        // SAFETY: both owned handles remain valid; Job close-kill and explicit termination cover
        // the full process tree, and waiting keeps teardown synchronous.
        unsafe {
            let _ = TerminateJobObject(self.job.as_raw_handle(), 1);
            let _ = TerminateProcess(self.process.as_raw_handle(), 1);
            let _ = WaitForSingleObject(self.process.as_raw_handle(), INFINITE);
        }
    }
}

pub(super) fn working_directory(policy: &RuntimePolicy) -> Result<tempfile::TempDir, PluginError> {
    validate_storage(policy)?;
    let directory = tempfile::Builder::new()
        .prefix("into-md-plugin-")
        .tempdir_in(&policy.windows.storage_root)
        .map_err(|_| unavailable("AppContainer working directory unavailable"))?;
    protect_working_directory(policy, directory.path())?;
    validate_working_directory(policy, directory.path())?;
    Ok(directory)
}

fn protect_working_directory(policy: &RuntimePolicy, path: &Path) -> Result<(), PluginError> {
    let current = CurrentUser::open()?;
    let app = AppContainerSid::derive_verified(&policy.windows.profile_name, &policy.windows.sid)?;
    let mut access = [
        EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: current.sid().cast(),
            },
        },
        EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: app.as_ptr().cast(),
            },
        },
    ];
    let mut acl = std::ptr::null_mut();
    // SAFETY: both token/profile-backed SIDs and the writable ACL output remain live.
    if unsafe { SetEntriesInAclW(2, access.as_mut_ptr(), std::ptr::null(), &raw mut acl) } != 0
        || acl.is_null()
    {
        return Err(unavailable("AppContainer working DACL construction failed"));
    }
    let mut wide = wide_path(path)?;
    // SAFETY: the exact two-entry ACL and terminated path remain live for the call.
    let installed = unsafe {
        SetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    // SAFETY: SetEntriesInAclW returned LocalAlloc-owned storage.
    unsafe { LocalFree(acl.cast()) };
    if installed != 0 {
        return Err(unavailable("AppContainer working DACL installation failed"));
    }
    Ok(())
}

pub(super) fn spawn(
    _command: std::process::Command,
    plugin: &ValidatedPlugin,
    policy: &RuntimePolicy,
    directory: &Path,
) -> Result<super::SandboxChild, PluginError> {
    let _ = &plugin.runtime_root;
    validate_storage(policy)?;
    let sid = AppContainerSid::derive_verified(&policy.windows.profile_name, &policy.windows.sid)?;
    let stdin = Pipe::new(false)?;
    let stdout = Pipe::new(true)?;
    let stderr = Pipe::new(true)?;
    let inherited = [stdin.child_raw(), stdout.child_raw(), stderr.child_raw()];
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: sid.as_ptr(),
        Capabilities: std::ptr::null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let mut attributes = AttributeList::new(2)?;
    attributes.update(
        usize::try_from(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES)
            .map_err(|_| unavailable("attribute conversion failed"))?,
        (&raw const capabilities).cast(),
        size_of::<SECURITY_CAPABILITIES>(),
    )?;
    attributes.update(
        usize::try_from(PROC_THREAD_ATTRIBUTE_HANDLE_LIST)
            .map_err(|_| unavailable("attribute conversion failed"))?,
        inherited.as_ptr().cast(),
        size_of::<[HANDLE; 3]>(),
    )?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>())
        .map_err(|_| unavailable("startup conversion failed"))?;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited[0];
    startup.StartupInfo.hStdOutput = inherited[1];
    startup.StartupInfo.hStdError = inherited[2];
    startup.lpAttributeList = attributes.as_mut_ptr();
    let application = wide_process_path(&plugin.executable)?;
    let current_directory = wide_process_path(directory)?;
    let environment = environment_block(policy, directory)?;
    // Establish every fallible Job limit before creating a suspended process.
    let job = create_job(policy.max_memory_bytes, policy.allow_child_processes)?;
    let mut information = PROCESS_INFORMATION::default();
    // SAFETY: all UTF-16 buffers, startup attributes, capability/SID storage, inherited handles,
    // and output pointers remain alive for the duration of CreateProcessW.
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            current_directory.as_ptr(),
            &raw const startup.StartupInfo,
            &raw mut information,
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        return Err(PluginError::new(
            PluginErrorCode::Launch,
            format!("AppContainer process launch failed (os={:?})", error.raw_os_error()),
        ));
    }
    // SAFETY: successful CreateProcessW returns two new, non-null handles whose ownership passes
    // to the caller exactly once.
    let process = unsafe { OwnedHandle::from_raw_handle(information.hProcess) };
    // SAFETY: as above, this is the separately owned primary-thread handle.
    let thread = unsafe { OwnedHandle::from_raw_handle(information.hThread) };
    // SAFETY: the owned Job and suspended process handles are valid and not shared mutably.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) } == 0 {
        terminate_suspended(&job, &process);
        return Err(unavailable("AppContainer job assignment failed"));
    }
    match process_token_matches(process.as_raw_handle(), &policy.windows.sid) {
        Ok(true) => {}
        Ok(false) => {
            terminate_suspended(&job, &process);
            return Err(unavailable("AppContainer process identity mismatch"));
        }
        Err(error) => {
            terminate_suspended(&job, &process);
            return Err(error);
        }
    }
    // SAFETY: `thread` is the still-suspended primary thread returned by CreateProcessW.
    if unsafe { ResumeThread(thread.as_raw_handle()) } != 1 {
        terminate_suspended(&job, &process);
        return Err(unavailable("AppContainer identity/job installation failed"));
    }
    drop(thread);
    drop(attributes);
    Ok(super::SandboxChild::Windows(Child {
        process,
        job,
        stdin: Some(stdin.into_parent()),
        stdout: Some(stdout.into_parent()),
        stderr: Some(stderr.into_parent()),
    }))
}

fn terminate_suspended(job: &OwnedHandle, process: &OwnedHandle) {
    // SAFETY: both handles are owned by the caller and valid. TerminateProcess also covers the
    // rare AssignProcessToJobObject failure where the process never became a Job member.
    unsafe {
        let _ = TerminateJobObject(job.as_raw_handle(), 1);
        let _ = TerminateProcess(process.as_raw_handle(), 1);
        let _ = WaitForSingleObject(process.as_raw_handle(), INFINITE);
    }
}

fn validate_storage(policy: &RuntimePolicy) -> Result<(), PluginError> {
    if policy.windows.profile_name.is_empty() || !policy.windows.sid.starts_with("S-1-15-2-") {
        return Err(unavailable("AppContainer authority missing"));
    }
    let sid = AppContainerSid::derive_verified(&policy.windows.profile_name, &policy.windows.sid)?;
    let actual = storage_path(sid.as_ptr())?;
    let expected = policy
        .windows
        .storage_root
        .canonicalize()
        .map_err(|_| unavailable("AppContainer storage unavailable"))?;
    if actual != expected || !expected.is_dir() {
        return Err(unavailable("AppContainer storage identity mismatch"));
    }
    Ok(())
}

fn environment_block(policy: &RuntimePolicy, directory: &Path) -> Result<Vec<u16>, PluginError> {
    let mut pairs = policy
        .environment
        .iter()
        .map(|(key, value)| format!("{}={}", key.to_string_lossy(), value.to_string_lossy()))
        .collect::<Vec<_>>();
    pairs.push("INTO_MARKDOWN_PLUGIN_PROTOCOL=process-v1".into());
    let windows = windows_directory()?;
    let drive = Path::new(&windows)
        .components()
        .next()
        .and_then(|component| match component {
            std::path::Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy()),
            _ => None,
        })
        .ok_or_else(|| unavailable("Windows system drive unavailable"))?;
    // Every request receives an authenticated writable directory below the
    // AppContainer AC authority. It is already the process current directory
    // and is removed only after the complete Job has stopped.
    let private = process_path_text(directory)?;
    pairs.extend([
        format!("SystemRoot={windows}"),
        format!("windir={windows}"),
        format!("SystemDrive={drive}"),
        format!("INTO_MARKDOWN_PRIVATE_TEMP={private}"),
        format!("USERPROFILE={private}"),
        format!("LOCALAPPDATA={private}"),
        format!("APPDATA={private}"),
        format!("TEMP={private}"),
        format!("TMP={private}"),
    ]);
    pairs.sort_by_key(|value| value.to_uppercase());
    let mut block = Vec::new();
    for pair in pairs {
        block.extend(pair.encode_utf16());
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn windows_directory() -> Result<String, PluginError> {
    let mut buffer = vec![0_u16; 32_768];
    // SAFETY: the vector exposes its full writable capacity and the supplied length matches it.
    let length = unsafe {
        GetWindowsDirectoryW(
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len())
                .map_err(|_| unavailable("Windows directory buffer overflow"))?,
        )
    };
    let length =
        usize::try_from(length).map_err(|_| unavailable("Windows directory length overflow"))?;
    if length == 0 || length >= buffer.len() {
        return Err(unavailable("Windows directory unavailable"));
    }
    String::from_utf16(&buffer[..length]).map_err(|_| unavailable("Windows directory is invalid"))
}

fn create_job(memory: u64, allow_child_processes: bool) -> Result<OwnedHandle, PluginError> {
    // SAFETY: null security/name pointers request a private unnamed Job.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return Err(unavailable("job creation failed"));
    }
    // SAFETY: the successful call returned a newly owned non-null Job handle.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = if allow_child_processes { 16 } else { 1 };
    let memory =
        usize::try_from(memory).map_err(|_| unavailable("job memory conversion failed"))?;
    limits.ProcessMemoryLimit = memory;
    limits.JobMemoryLimit = memory;
    // SAFETY: `limits` has the documented structure/size and `job` is valid.
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .map_err(|_| unavailable("job size conversion failed"))?,
        )
    } == 0
    {
        return Err(unavailable("job limit installation failed"));
    }
    Ok(job)
}

struct Pipe {
    parent: OwnedHandle,
    child: OwnedHandle,
}
impl Pipe {
    fn new(parent_reads: bool) -> Result<Self, PluginError> {
        let mut security = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
                .map_err(|_| unavailable("pipe size conversion failed"))?,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let mut read = std::ptr::null_mut();
        let mut write = std::ptr::null_mut();
        // SAFETY: all output pointers and the initialized security descriptor are valid.
        if unsafe { CreatePipe(&raw mut read, &raw mut write, &raw mut security, 0) } == 0 {
            return Err(unavailable("pipe creation failed"));
        }
        // SAFETY: successful CreatePipe returned two newly owned handles.
        let read = unsafe { OwnedHandle::from_raw_handle(read) };
        // SAFETY: this is the distinct write handle returned by the same call.
        let write = unsafe { OwnedHandle::from_raw_handle(write) };
        let (parent, child) = if parent_reads { (read, write) } else { (write, read) };
        // SAFETY: `parent` owns a valid pipe handle; the call only clears its inherit bit.
        if unsafe { SetHandleInformation(parent.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(unavailable("pipe inheritance failed"));
        }
        Ok(Self { parent, child })
    }
    fn child_raw(&self) -> HANDLE {
        self.child.as_raw_handle()
    }
    fn into_parent(self) -> File {
        File::from(self.parent)
    }
}

struct AttributeList {
    bytes: Vec<usize>,
    pointer: *mut core::ffi::c_void,
}
impl AttributeList {
    fn new(count: u32) -> Result<Self, PluginError> {
        let mut bytes = 0_usize;
        // SAFETY: null is the documented sizing probe; `bytes` is writable.
        unsafe {
            let _ =
                InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &raw mut bytes);
        }
        if bytes == 0 || bytes > 1024 * 1024 {
            return Err(unavailable("attribute sizing failed"));
        }
        let mut storage = vec![0_usize; bytes.div_ceil(size_of::<usize>())];
        let pointer = storage.as_mut_ptr().cast();
        // SAFETY: pointer storage is aligned and at least the probed byte length.
        if unsafe { InitializeProcThreadAttributeList(pointer, count, 0, &raw mut bytes) } == 0 {
            return Err(unavailable("attribute initialization failed"));
        }
        Ok(Self { bytes: storage, pointer })
    }
    fn update(
        &mut self,
        attribute: usize,
        value: *const core::ffi::c_void,
        bytes: usize,
    ) -> Result<(), PluginError> {
        // SAFETY: the list is initialized and the caller keeps the typed value alive through
        // process creation; `bytes` is the exact value size.
        if unsafe {
            UpdateProcThreadAttribute(
                self.pointer,
                0,
                attribute,
                value,
                bytes,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(unavailable("attribute update failed"));
        }
        Ok(())
    }
    fn as_mut_ptr(&mut self) -> *mut core::ffi::c_void {
        self.pointer
    }
}
impl Drop for AttributeList {
    fn drop(&mut self) {
        let _ = self.bytes.len();
        // SAFETY: `pointer` was initialized once and remains backed by `bytes` until this Drop.
        unsafe {
            DeleteProcThreadAttributeList(self.pointer);
        }
    }
}

struct AppContainerSid(PSID);
impl AppContainerSid {
    fn derive(name: &str) -> Result<Self, PluginError> {
        let name = wide_text(name)?;
        let mut sid = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated and `sid` is a writable result pointer.
        if unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &raw mut sid) } < 0
            || sid.is_null()
        {
            return Err(unavailable("AppContainer SID derivation failed"));
        }
        Ok(Self(sid))
    }

    fn derive_verified(name: &str, expected: &str) -> Result<Self, PluginError> {
        let sid = Self::derive(name)?;
        if sid_text(sid.0)?.eq_ignore_ascii_case(expected) {
            Ok(sid)
        } else {
            Err(unavailable("AppContainer SID mismatch"))
        }
    }
    fn as_ptr(&self) -> PSID {
        self.0
    }
}
impl Drop for AppContainerSid {
    fn drop(&mut self) {
        // SAFETY: the SID was allocated by DeriveAppContainerSidFromAppContainerName and is freed
        // exactly once by this owner.
        unsafe {
            FreeSid(self.0);
        }
    }
}

fn process_token_matches(process: HANDLE, expected: &str) -> Result<bool, PluginError> {
    let mut token = std::ptr::null_mut();
    // SAFETY: `process` is a valid process handle and `token` is writable.
    if unsafe { OpenProcessToken(process, 0x0008, &raw mut token) } == 0 {
        return Err(unavailable("process token unavailable"));
    }
    let result = token_matches(token, expected);
    // SAFETY: OpenProcessToken returned a newly owned token handle.
    unsafe {
        CloseHandle(token);
    }
    result
}

fn token_matches(token: HANDLE, expected: &str) -> Result<bool, PluginError> {
    let mut is_app = 0_u32;
    let mut returned = 0_u32;
    // SAFETY: token is valid; the fixed-size output and returned-length pointers are writable.
    if unsafe {
        GetTokenInformation(
            token,
            TokenIsAppContainer,
            (&raw mut is_app).cast(),
            4,
            &raw mut returned,
        )
    } == 0
        || is_app != 1
    {
        return Ok(false);
    }
    let mut needed = 0_u32;
    // SAFETY: null-buffer sizing is the documented first GetTokenInformation call.
    unsafe {
        let _ = GetTokenInformation(
            token,
            TokenAppContainerSid,
            std::ptr::null_mut(),
            0,
            &raw mut needed,
        );
    }
    let information_bytes = u32::try_from(size_of::<TOKEN_APPCONTAINER_INFORMATION>())
        .map_err(|_| unavailable("token SID structure size overflow"))?;
    if needed < information_bytes || needed > 64 * 1024 {
        return Err(unavailable("token SID size invalid"));
    }
    let mut buffer = vec![0_usize; (needed as usize).div_ceil(size_of::<usize>())];
    // SAFETY: the aligned buffer is at least `needed` bytes and all output pointers are valid.
    if unsafe {
        GetTokenInformation(
            token,
            TokenAppContainerSid,
            buffer.as_mut_ptr().cast(),
            needed,
            &raw mut returned,
        )
    } == 0
        || returned != needed
    {
        return Err(unavailable("token SID unavailable"));
    }
    // SAFETY: the successful call initialized at least one complete, aligned information record.
    let information = unsafe { &*buffer.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>() };
    if information.TokenAppContainer.is_null() {
        return Ok(false);
    }
    Ok(sid_text(information.TokenAppContainer)?.eq_ignore_ascii_case(expected))
}

fn storage_path(sid: PSID) -> Result<std::path::PathBuf, PluginError> {
    let text = sid_text(sid)?;
    let text = wide_text(&text)?;
    let mut raw = std::ptr::null_mut();
    // SAFETY: `text` is NUL-terminated and `raw` is a writable result pointer.
    if unsafe { GetAppContainerFolderPath(text.as_ptr(), &raw mut raw) } < 0 || raw.is_null() {
        return Err(unavailable("AppContainer storage unavailable"));
    }
    let raw = StoragePath(raw);
    wide_string(raw.0)
        .map(std::path::PathBuf::from)?
        .canonicalize()
        .map_err(|_| unavailable("AppContainer storage canonicalization failed"))
}

fn validate_working_directory(policy: &RuntimePolicy, directory: &Path) -> Result<(), PluginError> {
    use std::os::windows::fs::MetadataExt as _;

    let root = policy
        .windows
        .storage_root
        .canonicalize()
        .map_err(|_| unavailable("AppContainer storage unavailable"))?;
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|_| unavailable("AppContainer working directory unavailable"))?;
    if !metadata.is_dir() || metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(unavailable("AppContainer working directory identity mismatch"));
    }
    let canonical = directory
        .canonicalize()
        .map_err(|_| unavailable("AppContainer working directory canonicalization failed"))?;
    if canonical.parent() != Some(root.as_path()) {
        return Err(unavailable("AppContainer working directory identity mismatch"));
    }
    let current = CurrentUser::open()?;
    let app = AppContainerSid::derive_verified(&policy.windows.profile_name, &policy.windows.sid)?;
    verify_acl_with_masks(
        &canonical,
        &[current.sid(), app.as_ptr()],
        &[FILE_ALL_ACCESS, FILE_ALL_ACCESS],
        true,
        false,
    )
}

fn sid_text(sid: PSID) -> Result<String, PluginError> {
    let mut raw = std::ptr::null_mut();
    // SAFETY: caller supplies a valid SID and `raw` is writable.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut raw) } == 0 || raw.is_null() {
        return Err(unavailable("SID rendering failed"));
    }
    let result = wide_string(raw);
    // SAFETY: ConvertSidToStringSidW returned LocalAlloc-owned storage.
    unsafe {
        LocalFree(raw.cast());
    }
    result
}

fn wide_path(path: &Path) -> Result<Vec<u16>, PluginError> {
    let absolute =
        std::path::absolute(path).map_err(|_| unavailable("absolute Windows path unavailable"))?;
    let encoded = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
    let slash = u16::from(b'\\');
    let verbatim = [slash, slash, u16::from(b'?'), slash];
    let unc = [slash, slash];
    let mut value = if encoded.starts_with(&verbatim) {
        encoded
    } else if encoded.starts_with(&unc) {
        "\\\\?\\UNC\\".encode_utf16().chain(encoded.into_iter().skip(2)).collect()
    } else {
        "\\\\?\\".encode_utf16().chain(encoded).collect()
    };
    if value.contains(&0) {
        return Err(unavailable("wide string contains NUL"));
    }
    value.push(0);
    Ok(value)
}

fn wide_process_path(path: &Path) -> Result<Vec<u16>, PluginError> {
    let absolute =
        std::path::absolute(path).map_err(|_| unavailable("absolute Windows path unavailable"))?;
    let encoded = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
    let slash = u16::from(b'\\');
    let verbatim = [slash, slash, u16::from(b'?'), slash];
    let verbatim_unc = [
        slash,
        slash,
        u16::from(b'?'),
        slash,
        u16::from(b'U'),
        u16::from(b'N'),
        u16::from(b'C'),
        slash,
    ];
    let legacy = if encoded.starts_with(&verbatim_unc) {
        [slash, slash].into_iter().chain(encoded.into_iter().skip(verbatim_unc.len())).collect()
    } else if encoded.starts_with(&verbatim) {
        encoded.into_iter().skip(verbatim.len()).collect()
    } else {
        encoded
    };
    let mut legacy = legacy;
    legacy.push(0);
    Ok(legacy)
}

fn process_path_text(path: &Path) -> Result<String, PluginError> {
    let mut wide = wide_process_path(path)?;
    let terminated = wide.pop();
    if terminated != Some(0) {
        return Err(unavailable("process environment path is not terminated"));
    }
    String::from_utf16(&wide).map_err(|_| unavailable("process environment path is invalid"))
}
fn wide_text(value: &str) -> Result<Vec<u16>, PluginError> {
    wide_os(std::ffi::OsStr::new(value))
}
fn wide_os(value: &std::ffi::OsStr) -> Result<Vec<u16>, PluginError> {
    let mut value = value.encode_wide().collect::<Vec<_>>();
    if value.contains(&0) {
        return Err(unavailable("wide string contains NUL"));
    }
    value.push(0);
    Ok(value)
}
fn wide_string(value: *const u16) -> Result<String, PluginError> {
    if value.is_null() {
        return Err(unavailable("wide string is null"));
    }
    let mut length = 0_usize;
    // SAFETY: every caller passes an OS-owned NUL-terminated string; the defensive 32 KiB limit
    // bounds reads even if the operating-system contract is violated.
    while length <= 32 * 1024 && unsafe { *value.add(length) } != 0 {
        length += 1;
    }
    if length > 32 * 1024 {
        return Err(unavailable("wide string exceeds limit"));
    }
    // SAFETY: the scan above established `length` initialized u16 values before the terminator.
    String::from_utf16(unsafe { std::slice::from_raw_parts(value, length) })
        .map_err(|_| unavailable("wide string is invalid"))
}

fn unavailable(detail: impl Into<String>) -> PluginError {
    PluginError::new(PluginErrorCode::SandboxUnavailable, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_api_paths_use_verbatim_drive_and_unc_prefixes() {
        let drive = wide_path(Path::new(r"C:\isolated\runtime")).unwrap();
        let drive = String::from_utf16(&drive[..drive.len() - 1]).unwrap();
        assert_eq!(drive, r"\\?\C:\isolated\runtime");
        let unc = wide_path(Path::new(r"\\server\share\runtime")).unwrap();
        let unc = String::from_utf16(&unc[..unc.len() - 1]).unwrap();
        assert_eq!(unc, r"\\?\UNC\server\share\runtime");
        let verbatim = wide_path(Path::new(r"\\?\C:\already\verbatim")).unwrap();
        let verbatim = String::from_utf16(&verbatim[..verbatim.len() - 1]).unwrap();
        assert_eq!(verbatim, r"\\?\C:\already\verbatim");

        let process = wide_process_path(Path::new(r"\\?\C:\process\working")).unwrap();
        let process = String::from_utf16(&process[..process.len() - 1]).unwrap();
        assert_eq!(process, r"C:\process\working");
        let process_unc = wide_process_path(Path::new(r"\\?\UNC\server\share\working")).unwrap();
        let process_unc = String::from_utf16(&process_unc[..process_unc.len() - 1]).unwrap();
        assert_eq!(process_unc, r"\\server\share\working");
    }
}

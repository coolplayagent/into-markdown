//! Protected Windows directories for private stores and workspace output journals.

use super::{
    AclSource, CurrentUser, PluginError, sid_text, token_information, unavailable,
    verify_object_acl, verify_private_child, verify_private_path, wide_path,
};
use std::path::Path;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce, PSECURITY_DESCRIPTOR,
    PSID, TOKEN_GROUPS, TokenRestrictedSids,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_ALL_ACCESS, FILE_DELETE_CHILD};

use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, SET_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    InitializeSecurityDescriptor, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
    SetSecurityDescriptorOwner,
};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

// dsh gives a non-unique, workspace-specific SID this inheritable mask.
// DELETE_CHILD is denied separately on directories. Omit it from both grants,
// so deleting a child still requires DELETE on that child.
const WORKSPACE_WRITE: u32 = 0x0011_0156;
const JOURNAL_WRITE: u32 = WORKSPACE_WRITE & !FILE_DELETE_CHILD;
const OWNER_ACCESS: u32 = FILE_ALL_ACCESS & !FILE_DELETE_CHILD;

/// Atomically create one directory with a current-user-only protected DACL.
///
/// # Errors
///
/// Returns an error when creation or the exact owner/DACL verification fails.
pub fn create_windows_plugin_store_directory(path: &Path) -> Result<(), PluginError> {
    let user = CurrentUser::open()?;
    create_directory_with_access(path, &user, &[directory_access(user.sid(), FILE_ALL_ACCESS)])?;
    if let Err(error) = verify_private_path(path) {
        let _ = std::fs::remove_dir(path);
        return Err(error);
    }
    Ok(())
}

/// Verify a plugin-store path remains owned and writable only by the current user.
///
/// # Errors
///
/// Returns an error when the path identity, owner, or DACL is not exact.
pub fn verify_windows_plugin_store_path(path: &Path) -> Result<(), PluginError> {
    verify_private_path(path)
}

/// Verify a child inherited only the private plugin-store DACL.
///
/// # Errors
///
/// Returns an error when the child is a link or has unexpected ACL authority.
pub fn verify_windows_plugin_store_child(path: &Path) -> Result<(), PluginError> {
    verify_private_child(path)
}

fn directory_access(sid: PSID, mask: u32) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: SET_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: sid.cast(),
        },
    }
}

fn create_directory_with_access(
    path: &Path,
    current_user: &CurrentUser,
    entries: &[EXPLICIT_ACCESS_W],
) -> Result<(), PluginError> {
    let wide = wide_path(path)?;
    let count =
        u32::try_from(entries.len()).map_err(|_| unavailable("too many directory grants"))?;
    let mut acl = std::ptr::null_mut();
    // SAFETY: the entries and their SID storage remain live, and acl is writable.
    if unsafe { SetEntriesInAclW(count, entries.as_ptr(), std::ptr::null(), &raw mut acl) } != 0
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
    Ok(())
}

struct ParentAcl {
    descriptor: PSECURITY_DESCRIPTOR,
    capabilities: Vec<PSID>,
}

impl Drop for ParentAcl {
    fn drop(&mut self) {
        // SAFETY: GetNamedSecurityInfoW allocated the descriptor and its DACL.
        if !self.descriptor.is_null() {
            unsafe { LocalFree(self.descriptor) };
        }
    }
}

impl ParentAcl {
    fn for_token(root: &Path, user: &CurrentUser) -> Result<Self, PluginError> {
        let buffer = token_information(&user.token, TokenRestrictedSids, "restricted SIDs")?;
        let groups = buffer.as_ptr().cast::<TOKEN_GROUPS>();
        // SAFETY: the system filled the aligned buffer with a counted TOKEN_GROUPS array.
        let count = unsafe { (*groups).GroupCount } as usize;
        if count == 0 {
            return Ok(Self { descriptor: std::ptr::null_mut(), capabilities: Vec::new() });
        }
        // SAFETY: buffer owns the array and all of its SIDs through the retain call.
        let restricted = unsafe {
            std::slice::from_raw_parts(
                (&raw const (*groups).Groups)
                    .cast::<windows_sys::Win32::Security::SID_AND_ATTRIBUTES>(),
                count,
            )
        };
        let mut parent = Self::read(root)?;
        parent
            .capabilities
            .retain(|&sid| restricted.iter().any(|group| unsafe { EqualSid(group.Sid, sid) } != 0));
        if parent.capabilities.is_empty() {
            for group in restricted {
                if workspace_sid(&sid_text(group.Sid)?) {
                    return Err(unavailable(
                        "output parent lacks an inheritable write grant for this restricted workspace",
                    ));
                }
            }
        }
        Ok(parent)
    }

    fn read(root: &Path) -> Result<Self, PluginError> {
        let root = wide_path(root)?;
        let mut dacl = std::ptr::null_mut();
        let mut result = Self { descriptor: std::ptr::null_mut(), capabilities: Vec::new() };
        // SAFETY: the path is terminated and both output pointers are writable.
        let status = unsafe {
            GetNamedSecurityInfoW(
                root.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut dacl,
                std::ptr::null_mut(),
                &raw mut result.descriptor,
            )
        };
        if status != 0 || dacl.is_null() {
            return Err(unavailable("output parent DACL unavailable"));
        }
        result.capabilities = Self::workspace_grants(dacl)?;
        Ok(result)
    }

    fn workspace_grants(dacl: *const ACL) -> Result<Vec<PSID>, PluginError> {
        let mut capabilities = Vec::new();
        // SAFETY: the caller holds the descriptor containing this OS-provided ACL.
        for index in 0..u32::from(unsafe { (*dacl).AceCount }) {
            let mut raw = std::ptr::null_mut();
            if unsafe { GetAce(dacl, index, &raw mut raw) } == 0 || raw.is_null() {
                return Err(unavailable("output parent ACE unavailable"));
            }
            // Only ordinary allow and deny ACEs have the layout used below.
            let header = unsafe { &*raw.cast::<windows_sys::Win32::Security::ACE_HEADER>() };
            if !matches!(header.AceType, 0 | 1) {
                return Ok(Vec::new());
            }
            let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
            if header.AceType == 1 {
                if ace.Mask & !FILE_DELETE_CHILD != 0 {
                    return Ok(Vec::new());
                }
                continue;
            }
            // OBJECT_INHERIT | CONTAINER_INHERIT, optionally already inherited.
            if header.AceFlags & !0x10 != 3 || !matches!(ace.Mask, WORKSPACE_WRITE | JOURNAL_WRITE)
            {
                continue;
            }
            let sid = (&raw const ace.SidStart).cast_mut().cast();
            if workspace_sid(&sid_text(sid)?) {
                capabilities.push(sid);
            }
        }
        Ok(capabilities)
    }
}

fn workspace_sid(sid: &str) -> bool {
    let Some((a, b)) = sid.strip_prefix("S-1-4-").and_then(|tail| tail.split_once('-')) else {
        return false;
    };
    // The per-session temporary directory uses the same two-component
    // identifier with a third sub-authority of 1.
    let b = b.strip_suffix("-1").unwrap_or(b);
    [a, b].iter().all(|part| part.parse::<u32>().is_ok_and(|n| (1..(1 << 30)).contains(&n)))
}

/// Create a private output directory using only a write capability already
/// present in both the current restricted token and the output parent's ACL.
///
/// # Errors
/// Returns an error when Windows refuses creation or the resulting ACL is unexpected.
pub fn create_windows_output_directory(path: &Path) -> Result<(), PluginError> {
    let user = CurrentUser::open()?;
    let parent = path.parent().ok_or_else(|| unavailable("output parent missing"))?;
    let grants = ParentAcl::for_token(parent, &user)?;
    let Some(&sid) = grants.capabilities.first() else {
        return create_windows_plugin_store_directory(path);
    };
    let entries =
        [directory_access(user.sid(), OWNER_ACCESS), directory_access(sid, JOURNAL_WRITE)];
    create_directory_with_access(path, &user, &entries)?;
    if let Err(error) = verify_output_acl(AclSource::Path(path), &user, sid, true) {
        let _ = std::fs::remove_dir(path);
        return Err(error);
    }
    Ok(())
}

/// Authenticate an output journal directory or regular member against its
/// original private ACL or the narrowly scoped grant on its output parent.
///
/// # Errors
/// Returns an error for extra authorities, unexpected access or an invalid owner.
pub fn verify_windows_output_member(
    root: &Path,
    handle: std::os::windows::io::BorrowedHandle<'_>,
    directory: bool,
) -> Result<(), PluginError> {
    let user = CurrentUser::open()?;
    let source = AclSource::Handle(handle, directory);
    let original = verify_object_acl(source, &[user.sid()], &[FILE_ALL_ACCESS], directory, true);
    if original.is_ok() {
        return Ok(());
    }
    // A more privileged process must not replay a journal writable by a sandbox.
    let parent = ParentAcl::for_token(root, &user)?;
    for &sid in &parent.capabilities {
        if verify_output_acl(source, &user, sid, directory).is_ok() {
            return Ok(());
        }
    }
    original
}

fn verify_output_acl(
    source: AclSource<'_>,
    user: &CurrentUser,
    sid: PSID,
    directory: bool,
) -> Result<(), PluginError> {
    verify_object_acl(source, &[user.sid(), sid], &[OWNER_ACCESS, JOURNAL_WRITE], directory, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;

    #[test]
    fn journal_grants_are_bound_to_the_parent_and_cannot_be_broadened() {
        let temporary = tempfile::tempdir().unwrap();
        let user = CurrentUser::open().unwrap();
        let text = super::super::wide_text("S-1-4-101-202").unwrap();
        let mut sid = std::ptr::null_mut();
        // SAFETY: the test SID is terminated and the output pointer is writable.
        assert_ne!(unsafe { ConvertStringSidToSidW(text.as_ptr(), &raw mut sid) }, 0);
        let workspace = temporary.path().join("workspace");
        create_directory_with_access(
            &workspace,
            &user,
            &[
                directory_access(user.sid(), FILE_ALL_ACCESS),
                directory_access(sid, WORKSPACE_WRITE),
            ],
        )
        .unwrap();
        let directory = workspace.join("journal");
        create_directory_with_access(
            &directory,
            &user,
            &[directory_access(user.sid(), OWNER_ACCESS), directory_access(sid, JOURNAL_WRITE)],
        )
        .unwrap();
        let member = directory.join("member");
        std::fs::write(&member, b"test").unwrap();
        verify_output_acl(AclSource::Path(&directory), &user, sid, true).unwrap();

        verify_output_acl(AclSource::Path(&member), &user, sid, false).unwrap();
        use std::os::windows::io::AsHandle as _;
        let file = std::fs::File::open(&member).unwrap();
        assert!(verify_windows_output_member(&workspace, file.as_handle(), false).is_err());
        let other = temporary.path().join("other");
        create_windows_plugin_store_directory(&other).unwrap();
        assert!(verify_windows_output_member(&other, file.as_handle(), false).is_err());
        assert!(verify_private_path(&directory).is_err());
        let broad = workspace.join("broad");
        create_directory_with_access(
            &broad,
            &user,
            &[directory_access(user.sid(), OWNER_ACCESS), directory_access(sid, FILE_ALL_ACCESS)],
        )
        .unwrap();
        assert!(verify_output_acl(AclSource::Path(&broad), &user, sid, true).is_err());
        // SAFETY: ConvertStringSidToSidW returned a LocalAlloc-owned SID.
        unsafe { LocalFree(sid) };
    }

    #[test]
    fn workspace_sid_excludes_accounts_groups_and_unbounded_identifiers() {
        assert!(workspace_sid("S-1-4-1-1073741823"));
        assert!(workspace_sid("S-1-4-1-1073741823-1"));
        for other in
            ["S-1-1-0", "S-1-5-32-544", "S-1-4-1", "S-1-4-1-2-3", "S-1-4-0-1", "S-1-4-1-1073741824"]
        {
            assert!(!workspace_sid(other));
        }
    }
}

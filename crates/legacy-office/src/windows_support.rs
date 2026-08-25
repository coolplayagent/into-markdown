use crate::authority::AppContainerAuthority;
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Component, PathBuf};
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Isolation::{
    DeriveAppContainerSidFromAppContainerName, GetAppContainerFolderPath,
};
use windows_sys::Win32::Security::{
    FreeSid, GetTokenInformation, PSID, TOKEN_APPCONTAINER_INFORMATION, TokenAppContainerSid,
    TokenIsAppContainer,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub(crate) struct AppContainerSid(PSID);

impl AppContainerSid {
    pub(crate) fn derive(authority: &AppContainerAuthority) -> Result<Self, ()> {
        let name = wide(&authority.profile_name)?;
        let mut sid = std::ptr::null_mut();
        // SAFETY: name is terminated and sid receives a userenv allocation.
        if unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &raw mut sid) } < 0
            || sid.is_null()
        {
            return Err(());
        }
        let sid = Self(sid);
        if sid.text()?.eq_ignore_ascii_case(&authority.sid) { Ok(sid) } else { Err(()) }
    }

    pub(crate) const fn as_ptr(&self) -> PSID {
        self.0
    }

    fn text(&self) -> Result<String, ()> {
        sid_text(self.0)
    }
}

impl Drop for AppContainerSid {
    fn drop(&mut self) {
        // SAFETY: this is the unique allocation returned by userenv.
        unsafe { FreeSid(self.0) };
    }
}

pub(crate) fn current_token_matches(expected: &str) -> Result<bool, ()> {
    let mut token = std::ptr::null_mut();
    // TOKEN_QUERY = 0x0008.
    if unsafe { OpenProcessToken(GetCurrentProcess(), 0x0008, &raw mut token) } == 0 {
        return Err(());
    }
    let result = token_matches(token, expected);
    // SAFETY: token is an owned raw handle returned by OpenProcessToken.
    unsafe { CloseHandle(token) };
    result
}

pub(crate) fn process_token_matches(
    process: *mut core::ffi::c_void,
    expected: &str,
) -> Result<bool, ()> {
    let mut token = std::ptr::null_mut();
    // TOKEN_QUERY = 0x0008. The process is still suspended by the caller.
    if unsafe { OpenProcessToken(process, 0x0008, &raw mut token) } == 0 {
        return Err(());
    }
    let result = token_matches(token, expected);
    // SAFETY: token is an owned raw handle returned by OpenProcessToken.
    unsafe { CloseHandle(token) };
    result
}

fn token_matches(token: *mut core::ffi::c_void, expected: &str) -> Result<bool, ()> {
    let dword_bytes = u32::try_from(std::mem::size_of::<u32>()).map_err(|_| ())?;
    let mut is_app_container = 0_u32;
    let mut returned = 0_u32;
    // SAFETY: the output has TokenIsAppContainer's documented DWORD layout.
    if unsafe {
        GetTokenInformation(
            token,
            TokenIsAppContainer,
            (&raw mut is_app_container).cast(),
            dword_bytes,
            &raw mut returned,
        )
    } == 0
        || returned != dword_bytes
        || is_app_container != 1
    {
        return Ok(false);
    }
    let mut needed = 0_u32;
    // SAFETY: a null first call obtains the exact variable buffer length.
    let _ = unsafe {
        GetTokenInformation(token, TokenAppContainerSid, std::ptr::null_mut(), 0, &raw mut needed)
    };
    let information_bytes =
        u32::try_from(std::mem::size_of::<TOKEN_APPCONTAINER_INFORMATION>()).map_err(|_| ())?;
    if needed < information_bytes || needed > 64 * 1024 {
        return Err(());
    }
    let words = usize::try_from(needed).map_err(|_| ())?.div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    // SAFETY: buffer is aligned and at least `needed` bytes long.
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
        return Err(());
    }
    // SAFETY: the successful API call initialized this leading structure.
    let information = unsafe { &*buffer.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>() };
    if information.TokenAppContainer.is_null() {
        return Ok(false);
    }
    Ok(sid_text(information.TokenAppContainer)?.eq_ignore_ascii_case(expected))
}

pub(crate) fn storage_path(expected_sid: &str) -> Result<PathBuf, ()> {
    let sid = wide(expected_sid)?;
    let mut raw = std::ptr::null_mut();
    // SAFETY: sid is terminated and raw receives a LocalAlloc path.
    if unsafe { GetAppContainerFolderPath(sid.as_ptr(), &raw mut raw) } < 0 || raw.is_null() {
        return Err(());
    }
    let result = wide_string(raw).map(PathBuf::from).and_then(|path| {
        if path.is_absolute()
            && !path
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            Ok(path)
        } else {
            Err(())
        }
    });
    // SAFETY: raw is the unique LocalAlloc result from userenv.
    unsafe { LocalFree(raw.cast()) };
    result
}

fn sid_text(sid: PSID) -> Result<String, ()> {
    let mut raw = std::ptr::null_mut();
    // SAFETY: caller supplies a live SID; raw receives a LocalAlloc string.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut raw) } == 0 || raw.is_null() {
        return Err(());
    }
    let result = wide_string(raw);
    // SAFETY: raw is the unique LocalAlloc result from advapi32.
    unsafe { LocalFree(raw.cast()) };
    result
}

fn wide(value: &str) -> Result<Vec<u16>, ()> {
    if value.encode_utf16().any(|unit| unit == 0) {
        return Err(());
    }
    Ok(std::ffi::OsStr::new(value).encode_wide().chain(Some(0)).collect())
}

fn wide_string(value: *const u16) -> Result<String, ()> {
    let mut length = 0_usize;
    // SAFETY: callers provide platform-owned terminated strings. This hard
    // ceiling prevents unbounded scanning on a malformed platform result.
    while length <= 32 * 1024 && unsafe { *value.add(length) } != 0 {
        length += 1;
    }
    if length > 32 * 1024 {
        return Err(());
    }
    // SAFETY: the bounded scan established this initialized range.
    String::from_utf16(unsafe { std::slice::from_raw_parts(value, length) }).map_err(|_| ())
}

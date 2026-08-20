use crate::{PluginError, PluginErrorCode, RuntimePolicy, ValidatedPlugin};
use std::fs::File;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree,
    SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Isolation::{
    DeriveAppContainerSidFromAppContainerName, GetAppContainerFolderPath,
};
use windows_sys::Win32::Security::{
    FreeSid, GetTokenInformation, PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES,
    TOKEN_APPCONTAINER_INFORMATION, TokenAppContainerSid, TokenIsAppContainer,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
    InitializeProcThreadAttributeList, OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};

const INFINITE: u32 = u32::MAX;

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
    tempfile::Builder::new()
        .prefix("into-md-plugin-")
        .tempdir_in(&policy.windows.storage_root)
        .map_err(|_| unavailable("AppContainer working directory unavailable"))
}

pub(super) fn spawn(
    _command: std::process::Command,
    plugin: &ValidatedPlugin,
    policy: &RuntimePolicy,
    directory: &Path,
) -> Result<super::SandboxChild, PluginError> {
    let _ = &plugin.runtime_root;
    validate_storage(policy)?;
    let sid = AppContainerSid::derive(&policy.windows.profile_name, &policy.windows.sid)?;
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
    let application = wide_path(&plugin.executable)?;
    let current_directory = wide_path(directory)?;
    let environment = environment_block(policy, directory)?;
    // Establish every fallible Job limit before creating a suspended process.
    let job = create_job(policy.max_memory_bytes)?;
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
    let sid = AppContainerSid::derive(&policy.windows.profile_name, &policy.windows.sid)?;
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
    let private = directory.to_string_lossy();
    pairs.extend([
        format!("SystemRoot={windows}"),
        format!("windir={windows}"),
        format!("SystemDrive={drive}"),
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

fn create_job(memory: u64) -> Result<OwnedHandle, PluginError> {
    // SAFETY: null security/name pointers request a private unnamed Job.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return Err(unavailable("job creation failed"));
    }
    // SAFETY: the successful call returned a newly owned non-null Job handle.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    limits.ProcessMemoryLimit =
        usize::try_from(memory).map_err(|_| unavailable("job memory conversion failed"))?;
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
    fn derive(name: &str, expected: &str) -> Result<Self, PluginError> {
        let name = wide_text(name)?;
        let mut sid = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated and `sid` is a writable result pointer.
        if unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &raw mut sid) } < 0
            || sid.is_null()
        {
            return Err(unavailable("AppContainer SID derivation failed"));
        }
        let sid = Self(sid);
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
    let result = wide_string(raw)
        .map(std::path::PathBuf::from)?
        .canonicalize()
        .map_err(|_| unavailable("AppContainer storage canonicalization failed"));
    // SAFETY: the API allocated this string with LocalAlloc and ownership is released once.
    unsafe {
        LocalFree(raw.cast());
    }
    result
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
    wide_os(path.as_os_str())
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

fn unavailable(detail: &'static str) -> PluginError {
    PluginError::new(PluginErrorCode::SandboxUnavailable, detail)
}

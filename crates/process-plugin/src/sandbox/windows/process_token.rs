use super::{CurrentUser, token_information, unavailable};
use crate::PluginError;
use std::os::windows::io::AsRawHandle as _;
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, IsTokenRestricted, TOKEN_MANDATORY_LABEL,
    TokenIntegrityLevel,
};

/// Detect restricting SIDs or an integrity level below medium on the current Windows process.
///
/// # Errors
///
/// Returns an error if the process token or its mandatory integrity label cannot be queried.
pub fn windows_process_is_restricted_or_low_integrity() -> Result<bool, PluginError> {
    const SECURITY_MANDATORY_MEDIUM_RID: u32 = 0x2000;
    let user = CurrentUser::open()?;
    // SAFETY: the owned current-process token remains open with TOKEN_QUERY.
    if unsafe { IsTokenRestricted(user.token.as_raw_handle()) } != 0 {
        return Ok(true);
    }
    let label = token_information(&user.token, TokenIntegrityLevel, "process integrity level")?;
    // SAFETY: GetTokenInformation initialized a TOKEN_MANDATORY_LABEL and its SID in
    // this aligned buffer. The buffer remains live while accessing the SID.
    let sid = unsafe { (*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
    // SAFETY: Windows supplied the mandatory label SID above.
    let count = unsafe { *GetSidSubAuthorityCount(sid) };
    if count == 0 {
        return Err(unavailable("process integrity level has no RID"));
    }
    // SAFETY: the last subauthority exists and holds the integrity RID.
    let rid = unsafe { *GetSidSubAuthority(sid, u32::from(count - 1)) };
    Ok(rid < SECURITY_MANDATORY_MEDIUM_RID)
}

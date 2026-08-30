use super::{
    CliError, Deserialize, Digest, ExitClass, File, OsStr, OsString, Path, Read, Serialize, Sha256,
    Write, fs, hex_bytes, io, validate_single_name,
};

fn recovery_error(detail: impl Into<String>) -> CliError {
    CliError::new(ExitClass::Io, "transactionRecoveryFailed", detail)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigExpectedAuthority {
    pub identity: Option<(u64, u64)>,
    pub sha256: Option<String>,
}

mod atomic_replace;
mod platform;
mod read_recover;

pub(crate) use atomic_replace::*;
pub(crate) use platform::*;
use read_recover::{
    WindowsConfigJournal, WindowsConfigPhase, config_rename_no_replace,
    config_test_mutate_before_publish, recover_windows_config_transaction,
    remove_config_with_authority, windows_config_test_crash, write_windows_config_journal,
};

#[cfg(all(test, windows))]
pub(in crate::transaction) use atomic_replace::windows_config_tests::config_replace_crash_child;

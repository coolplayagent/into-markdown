//! Narrow host adapter; platform packages may replace only this seam.

use std::collections::BTreeMap;

/// Platform identity and indispensable process environment.
pub trait PlatformAdapter {
    /// Stable OS name.
    fn platform(&self) -> &'static str;
    /// Stable machine architecture.
    fn architecture(&self) -> &'static str;
    /// Rust target triple bound by the archive manifest.
    fn target(&self) -> &'static str;
    /// Minimal environment required to create a process on this platform.
    fn process_environment(&self) -> BTreeMap<String, String>;
}

/// Adapter for the current host.
pub struct HostPlatform;

impl PlatformAdapter for HostPlatform {
    fn platform(&self) -> &'static str {
        std::env::consts::OS
    }

    fn architecture(&self) -> &'static str {
        std::env::consts::ARCH
    }

    fn target(&self) -> &'static str {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => "aarch64-apple-darwin",
            ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
            ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
            ("windows", "x86_64") => "x86_64-pc-windows-msvc",
            _ => "unsupported",
        }
    }

    fn process_environment(&self) -> BTreeMap<String, String> {
        #[cfg(windows)]
        {
            let mut values = BTreeMap::new();
            for name in ["SystemRoot", "WINDIR"] {
                if let Ok(value) = std::env::var(name) {
                    values.insert(name.to_owned(), value);
                }
            }
            values
        }
        #[cfg(not(windows))]
        {
            BTreeMap::new()
        }
    }
}

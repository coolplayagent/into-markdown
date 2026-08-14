use into_markdown_core::ConversionError;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(super) struct MountedContainer {
    mount: PathBuf,
}

impl MountedContainer {
    pub(super) fn attach(image: &Path, mount: &Path) -> Result<Self, ConversionError> {
        let status = Command::new("/usr/bin/hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-noautoopen", "-readonly", "-mountpoint"])
            .arg(mount)
            .arg(image)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| unavailable("containerAttach"))?;
        if !status.success() {
            return Err(unavailable("containerAttach"));
        }
        Ok(Self { mount: mount.to_owned() })
    }

    fn detach(&self) {
        for attempt in 0..10 {
            let status = Command::new("/usr/bin/hdiutil")
                .args(["detach", "-quiet"])
                .arg(&self.mount)
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if status.is_ok_and(|value| value.success()) {
                return;
            }
            if attempt != 9 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        let _ = Command::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet", "-force"])
            .arg(&self.mount)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for MountedContainer {
    fn drop(&mut self) {
        self.detach();
    }
}

fn unavailable(detail: &str) -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: "legacy-office-worker".into(),
        detail: detail.into(),
    }
}

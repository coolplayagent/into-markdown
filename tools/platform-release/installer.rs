//! Native transaction helper for the Linux and Windows Core installers.
//!
//! This binary intentionally uses only the Rust standard library.  It is built by the
//! platform release assembler and is not part of the macOS release graph.

use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode};

const LOCK: &str = ".install-lock";
const TRANSACTION: &str = ".install-transaction";
const CURRENT: &str = "current.txt";

fn main() -> ExitCode {
    match run() {
        Ok(message) => {
            println!("{}", message.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("into-md installer: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<PathBuf, String> {
    let mut arguments = env::args_os();
    let executable = env::current_exe().map_err(io_error("resolve installer executable"))?;
    let _ = arguments.next();
    let operation = arguments.next();
    if executable.file_stem() == Some(OsStr::new("into-md"))
        && operation.as_deref() != Some(OsStr::new("install"))
        && operation.as_deref() != Some(OsStr::new("uninstall"))
    {
        return launch(executable, operation, arguments.collect());
    }
    match operation.as_deref().and_then(OsStr::to_str) {
        Some("install") => {
            let distribution = next_path(&mut arguments, "distribution")?;
            let prefix = next_path(&mut arguments, "prefix")?;
            let command_directory = next_path(&mut arguments, "command directory")?;
            let archive_id = arguments
                .next()
                .and_then(|value| value.into_string().ok())
                .ok_or_else(|| "installUsage: archive manifest hash is required".to_owned())?;
            no_more(arguments)?;
            install(&executable, &distribution, &prefix, &command_directory, &archive_id)
        }
        Some("uninstall") => {
            let prefix = next_path(&mut arguments, "prefix")?;
            let command_directory = next_path(&mut arguments, "command directory")?;
            no_more(arguments)?;
            uninstall(&prefix, &command_directory)
        }
        _ => Err("installUsage: expected install or uninstall operation".to_owned()),
    }
}

fn next_path(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
    label: &str,
) -> Result<PathBuf, String> {
    arguments.next().map(PathBuf::from).ok_or_else(|| format!("installUsage: {label} is required"))
}

fn no_more(mut arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    if arguments.next().is_some() {
        Err("installUsage: unexpected trailing argument".to_owned())
    } else {
        Ok(())
    }
}

fn install(
    executable: &Path,
    distribution: &Path,
    prefix: &Path,
    command_directory: &Path,
    archive_id: &str,
) -> Result<PathBuf, String> {
    validate_hash(archive_id)?;
    let distribution = canonical_safe_directory(distribution, "distribution")?;
    let prefix = absolute_clean(prefix, "prefix")?;
    let command_directory = absolute_clean(command_directory, "command directory")?;
    ensure_safe_parent(&prefix)?;
    ensure_safe_parent(&command_directory)?;
    create_private_directory(&prefix)?;
    create_private_directory(&prefix.join("versions"))?;
    create_private_directory(&command_directory)?;
    verify_directory(&prefix, true)?;
    verify_directory(&prefix.join("versions"), true)?;
    verify_directory(&command_directory, false)?;

    let _lock = InstallLock::acquire(&prefix)?;
    recover(&prefix, &command_directory)?;
    let versions = prefix.join("versions");
    let destination = versions.join(archive_id);
    let mut installed_new = false;
    if destination.exists() {
        verify_directory(&destination, true)?;
        run_archive_check(&destination)?;
    } else {
        let temporary = versions.join(format!(".install-{archive_id}-{}", std::process::id()));
        if temporary.exists() {
            remove_tree(&temporary)?;
        }
        fs::create_dir(&temporary).map_err(io_error("create install staging directory"))?;
        write_transaction(&prefix, &format!("install\n{}\n{}\n", archive_id, temporary.display()))?;
        if let Err(error) = copy_tree(&distribution, &temporary)
            .and_then(|()| run_archive_check(&temporary))
            .and_then(|()| make_immutable(&temporary))
            .and_then(|()| {
                fs::rename(&temporary, &destination).map_err(io_error("publish installed version"))
            })
        {
            if let Err(cleanup) = remove_tree_eventually(&temporary) {
                return Err(format!(
                    "installRollbackFailed: installation failed ({error}); staging cleanup will be retried on the next invocation ({cleanup})"
                ));
            }
            clear_transaction(&prefix)?;
            return Err(error);
        }
        sync_directory(&versions)?;
        installed_new = true;
    }

    let command_result = {
        #[cfg(unix)]
        {
            install_unix_command(&prefix, &command_directory, archive_id)
        }
        #[cfg(windows)]
        {
            install_windows_command(executable, &prefix, &command_directory, archive_id)
        }
    };
    if let Err(error) = command_result {
        if installed_new {
            if let Err(cleanup) = remove_tree_eventually(&destination) {
                return Err(format!(
                    "installRollbackFailed: command publication failed ({error}); unpublished version cleanup failed ({cleanup})"
                ));
            }
        }
        let _ = clear_transaction(&prefix);
        return Err(error);
    }
    clear_transaction(&prefix)?;
    Ok(destination)
}

fn uninstall(prefix: &Path, command_directory: &Path) -> Result<PathBuf, String> {
    let prefix = absolute_clean(prefix, "prefix")?;
    let command_directory = absolute_clean(command_directory, "command directory")?;
    if !prefix.exists() {
        return Ok(prefix);
    }
    verify_directory(&prefix, true)?;
    let _lock = InstallLock::acquire(&prefix)?;
    recover(&prefix, &command_directory)?;
    #[cfg(unix)]
    validate_unix_command(&prefix, &command_directory)?;
    #[cfg(windows)]
    validate_windows_command(&prefix, &command_directory)?;
    let versions = prefix.join("versions");
    let mut removed_versions = None;
    if versions.exists() {
        verify_directory(&versions, true)?;
        let removed = prefix.join(format!(".removed-versions-{}", std::process::id()));
        write_transaction(
            &prefix,
            &format!(
                "uninstall-prepared\n{}\n{}\n",
                removed.display(),
                command_directory.display()
            ),
        )?;
        fs::rename(&versions, &removed).map_err(|error| {
            format!("installBusy: installed files are in use; the existing installation was preserved ({error})")
        })?;
        sync_directory(&prefix)?;
        removed_versions = Some(removed);
    }
    let remove_command = {
        #[cfg(unix)]
        {
            remove_unix_command(&prefix, &command_directory)
        }
        #[cfg(windows)]
        {
            remove_windows_command(&prefix, &command_directory)
        }
    };
    if let Err(error) = remove_command {
        if let Some(removed) = removed_versions.as_ref() {
            if let Err(restore_error) = fs::rename(removed, &versions) {
                return Err(format!(
                    "installRollbackFailed: command removal failed ({error}); installed files could not be restored ({restore_error})"
                ));
            }
            let _ = sync_directory(&prefix);
        }
        let _ = clear_transaction(&prefix);
        return Err(error);
    }
    if let Some(removed) = removed_versions.as_ref() {
        write_transaction(
            &prefix,
            &format!(
                "uninstall-committed\n{}\n{}\n",
                removed.display(),
                command_directory.display()
            ),
        )?;
    }
    if let Some(removed) = removed_versions.as_ref() {
        remove_tree(removed)?;
    }
    let current = prefix.join(CURRENT);
    if current.exists() {
        reject_special(&current, "current authority")?;
        fs::remove_file(&current).map_err(io_error("remove current authority"))?;
    }
    #[cfg(unix)]
    {
        let current_link = prefix.join("current");
        if fs::symlink_metadata(&current_link).is_ok() {
            fs::remove_file(current_link).map_err(io_error("remove current link"))?;
        }
    }
    clear_transaction(&prefix)?;
    Ok(prefix)
}

fn launch(
    executable: PathBuf,
    first: Option<std::ffi::OsString>,
    mut rest: Vec<std::ffi::OsString>,
) -> Result<PathBuf, String> {
    let command_directory = executable
        .parent()
        .ok_or_else(|| "launchAuthorityInvalid: launcher has no parent".to_owned())?;
    let prefix_file = command_directory.join("into-md.prefix");
    reject_special(&prefix_file, "launcher prefix authority")?;
    let prefix = PathBuf::from(read_small(&prefix_file)?);
    let prefix = absolute_clean(&prefix, "launcher prefix")?;
    let current = prefix.join(CURRENT);
    reject_special(&current, "current authority")?;
    let archive_id = read_small(&current)?;
    validate_hash(&archive_id)?;
    let target = prefix.join("versions").join(&archive_id).join("bin").join(if cfg!(windows) {
        "into-md.exe"
    } else {
        "into-md"
    });
    reject_special(&target, "installed command")?;
    if let Some(value) = first {
        rest.insert(0, value);
    }
    let status = Command::new(&target)
        .args(&rest)
        .status()
        .map_err(|error| format!("launchUnavailable: cannot start installed command ({error})"))?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(unix)]
fn install_unix_command(
    prefix: &Path,
    command_directory: &Path,
    archive_id: &str,
) -> Result<(), String> {
    use std::os::unix::fs::symlink;
    let current = prefix.join("current");
    if let Ok(metadata) = fs::symlink_metadata(&current) {
        if !metadata.file_type().is_symlink() {
            return Err("installPathUnsafe: current path is not installer-owned".to_owned());
        }
    }
    let command = command_directory.join("into-md");
    let command_exists = if let Ok(metadata) = fs::symlink_metadata(&command) {
        if !metadata.file_type().is_symlink()
            || fs::read_link(&command).ok().as_deref() != Some(&prefix.join("current/bin/into-md"))
        {
            return Err("installPathUnsafe: command path is not installer-owned".to_owned());
        }
        true
    } else {
        false
    };
    if !command_exists {
        let next_command =
            command_directory.join(format!(".into-md-{archive_id}-{}", std::process::id()));
        let _ = fs::remove_file(&next_command);
        symlink(prefix.join("current/bin/into-md"), &next_command)
            .map_err(io_error("create command link"))?;
        fs::rename(&next_command, &command).map_err(io_error("publish command link"))?;
        sync_directory(command_directory)?;
    }
    write_atomic(&prefix.join(CURRENT), archive_id.as_bytes())?;
    let next = prefix.join(format!(".current-{archive_id}-{}", std::process::id()));
    let _ = fs::remove_file(&next);
    symlink(format!("versions/{archive_id}"), &next).map_err(io_error("create current link"))?;
    fs::rename(&next, &current).map_err(io_error("publish current link"))?;
    sync_directory(prefix)?;
    Ok(())
}

#[cfg(unix)]
fn remove_unix_command(prefix: &Path, command_directory: &Path) -> Result<(), String> {
    validate_unix_command(prefix, command_directory)?;
    let command = command_directory.join("into-md");
    if fs::symlink_metadata(&command).is_ok() {
        fs::remove_file(command).map_err(io_error("remove command link"))?;
        sync_directory(command_directory)?;
    }
    Ok(())
}

#[cfg(unix)]
fn validate_unix_command(prefix: &Path, command_directory: &Path) -> Result<(), String> {
    let command = command_directory.join("into-md");
    if let Ok(metadata) = fs::symlink_metadata(&command) {
        if !metadata.file_type().is_symlink()
            || fs::read_link(&command).ok().as_deref() != Some(&prefix.join("current/bin/into-md"))
        {
            return Err("installPathUnsafe: refusing to remove a foreign command".to_owned());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn install_windows_command(
    executable: &Path,
    prefix: &Path,
    command_directory: &Path,
    archive_id: &str,
) -> Result<(), String> {
    let launcher = command_directory.join("into-md.exe");
    let prefix_authority = command_directory.join("into-md.prefix");
    if prefix_authority.exists() {
        reject_special(&prefix_authority, "launcher authority")?;
        if PathBuf::from(read_small(&prefix_authority)?) != prefix {
            return Err("installPathUnsafe: command path is not installer-owned".to_owned());
        }
        if launcher.exists() {
            reject_special(&launcher, "command launcher")?;
        }
    } else if launcher.exists() {
        return Err("installPathUnsafe: command path is not installer-owned".to_owned());
    } else {
        write_atomic(&prefix_authority, prefix.as_os_str().to_string_lossy().as_bytes())?;
    }
    let next_launcher = command_directory.join(format!(".into-md-launcher-{}", std::process::id()));
    let _ = fs::remove_file(&next_launcher);
    copy_regular(executable, &next_launcher)?;
    replace_file(&next_launcher, &launcher).map_err(|error| {
        let _ = fs::remove_file(&next_launcher);
        format!("installBusy: command launcher is in use; the existing installation was preserved ({error})")
    })?;
    write_atomic(&prefix.join(CURRENT), archive_id.as_bytes())?;
    Ok(())
}

#[cfg(windows)]
fn remove_windows_command(prefix: &Path, command_directory: &Path) -> Result<(), String> {
    validate_windows_command(prefix, command_directory)?;
    let launcher = command_directory.join("into-md.exe");
    let prefix_authority = command_directory.join("into-md.prefix");
    if launcher.exists() || prefix_authority.exists() {
        fs::remove_file(&prefix_authority).map_err(io_error("remove launcher authority"))?;
        if let Err(error) = fs::remove_file(&launcher) {
            if let Err(restore_error) =
                write_atomic(&prefix_authority, prefix.as_os_str().to_string_lossy().as_bytes())
            {
                return Err(format!(
                    "installRollbackFailed: command launcher is in use ({error}); its authority could not be restored ({restore_error})"
                ));
            }
            return Err(format!(
                "installBusy: command launcher is in use; installation was preserved ({error})"
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_command(prefix: &Path, command_directory: &Path) -> Result<(), String> {
    let launcher = command_directory.join("into-md.exe");
    let prefix_authority = command_directory.join("into-md.prefix");
    if launcher.exists() || prefix_authority.exists() {
        if !launcher.is_file()
            || !prefix_authority.is_file()
            || is_special(
                &fs::symlink_metadata(&launcher).map_err(io_error("inspect command launcher"))?,
            )
            || is_special(
                &fs::symlink_metadata(&prefix_authority)
                    .map_err(io_error("inspect launcher authority"))?,
            )
            || PathBuf::from(read_small(&prefix_authority)?) != prefix
        {
            return Err("installPathUnsafe: refusing to remove a foreign command".to_owned());
        }
    }
    Ok(())
}

fn recover(prefix: &Path, command_directory: &Path) -> Result<(), String> {
    let transaction = prefix.join(TRANSACTION);
    if !transaction.exists() {
        return Ok(());
    }
    reject_special(&transaction, "install transaction")?;
    let text = read_small(&transaction)?;
    let mut lines = text.lines();
    match lines.next() {
        Some("install") => {
            let _archive_id = lines
                .next()
                .ok_or_else(|| "installRecoveryFailed: incomplete install journal".to_owned())?;
            let temporary =
                PathBuf::from(lines.next().ok_or_else(|| {
                    "installRecoveryFailed: incomplete install journal".to_owned()
                })?);
            if temporary.parent() != Some(prefix.join("versions").as_path()) {
                return Err(
                    "installRecoveryFailed: staging path escaped versions directory".to_owned()
                );
            }
            if temporary.exists() {
                remove_tree(&temporary)?;
            }
        }
        Some(stage @ ("uninstall-prepared" | "uninstall-committed")) => {
            let removed =
                PathBuf::from(lines.next().ok_or_else(|| {
                    "installRecoveryFailed: incomplete uninstall journal".to_owned()
                })?);
            if removed.parent() != Some(prefix)
                || !removed
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".removed-versions-"))
            {
                return Err("installRecoveryFailed: removal path escaped install prefix".to_owned());
            }
            let journal_command =
                PathBuf::from(lines.next().ok_or_else(|| {
                    "installRecoveryFailed: incomplete uninstall journal".to_owned()
                })?);
            if &journal_command != command_directory {
                return Err("installRecoveryFailed: uninstall command directory changed".to_owned());
            }
            if removed.exists() {
                if stage == "uninstall-prepared"
                    && command_is_present(command_directory)
                    && !prefix.join("versions").exists()
                {
                    fs::rename(&removed, prefix.join("versions"))
                        .map_err(io_error("restore interrupted uninstall"))?;
                    sync_directory(prefix)?;
                } else {
                    remove_tree(&removed)?;
                }
            }
        }
        _ => return Err("installRecoveryFailed: unknown install journal operation".to_owned()),
    }
    clear_transaction(prefix)
}

fn command_is_present(command_directory: &Path) -> bool {
    command_directory.join(if cfg!(windows) { "into-md.exe" } else { "into-md" }).exists()
        || fs::symlink_metadata(command_directory.join("into-md")).is_ok()
}

fn write_transaction(prefix: &Path, value: &str) -> Result<(), String> {
    write_atomic(&prefix.join(TRANSACTION), value.as_bytes())
}

fn clear_transaction(prefix: &Path) -> Result<(), String> {
    let path = prefix.join(TRANSACTION);
    if path.exists() {
        fs::remove_file(path).map_err(io_error("clear install transaction"))?;
        sync_directory(prefix)?;
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent =
        path.parent().ok_or_else(|| "installPathUnsafe: authority has no parent".to_owned())?;
    let next = parent.join(format!(
        ".{}.next-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let _ = fs::remove_file(&next);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&next)
        .map_err(io_error("create authority temporary"))?;
    file.write_all(bytes).map_err(io_error("write authority temporary"))?;
    file.write_all(b"\n").map_err(io_error("write authority terminator"))?;
    file.sync_all().map_err(io_error("sync authority temporary"))?;
    drop(file);
    replace_file(&next, path)?;
    sync_directory(parent)
}

#[cfg(unix)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), String> {
    fs::rename(source, destination).map_err(io_error("publish authority"))
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    const MOVEFILE_REPLACE_EXISTING: u32 = 1;
    const MOVEFILE_WRITE_THROUGH: u32 = 8;
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both pointers reference live, NUL-terminated UTF-16 buffers for the duration of the call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(format!("installAtomicReplaceFailed: {}", std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    for entry in fs::read_dir(source).map_err(io_error("read distribution directory"))? {
        let entry = entry.map_err(io_error("read distribution entry"))?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let metadata =
            fs::symlink_metadata(&from).map_err(io_error("inspect distribution entry"))?;
        if is_special(&metadata) {
            return Err(format!(
                "installIntegrityFailed: distribution contains a link or reparse point: {}",
                from.display()
            ));
        }
        if metadata.is_dir() {
            fs::create_dir(&to).map_err(io_error("create installed directory"))?;
            copy_tree(&from, &to)?;
        } else if metadata.is_file() {
            copy_regular(&from, &to)?;
        } else {
            return Err(
                "installIntegrityFailed: distribution contains an unsupported file type".to_owned()
            );
        }
    }
    Ok(())
}

fn copy_regular(source: &Path, destination: &Path) -> Result<(), String> {
    reject_special(source, "source file")?;
    let mut input = File::open(source).map_err(io_error("open source file"))?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(io_error("create installed file"))?;
    std::io::copy(&mut input, &mut output).map_err(io_error("copy installed file"))?;
    output.sync_all().map_err(io_error("sync installed file"))?;
    fs::set_permissions(
        destination,
        fs::metadata(source).map_err(io_error("inspect source permissions"))?.permissions(),
    )
    .map_err(io_error("copy installed permissions"))
}

fn make_immutable(root: &Path) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(io_error("read installed tree"))? {
        let path = entry.map_err(io_error("read installed entry"))?.path();
        let metadata = fs::symlink_metadata(&path).map_err(io_error("inspect installed entry"))?;
        if metadata.is_dir() {
            make_immutable(&path)?;
        } else {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(true);
            fs::set_permissions(path, permissions).map_err(io_error("protect installed file"))?;
        }
    }
    Ok(())
}

fn remove_tree(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path).map_err(io_error("inspect removal root"))?;
    if !metadata.is_dir() || is_special(&metadata) {
        return Err(
            "installPathUnsafe: removal root is not an installer-owned directory".to_owned()
        );
    }
    make_mutable(path)?;
    fs::remove_dir_all(path).map_err(io_error("remove installer-owned tree"))
}

fn remove_tree_eventually(path: &Path) -> Result<(), String> {
    let mut last = None;
    for attempt in 0..10 {
        match remove_tree(path) {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
        #[cfg(windows)]
        std::thread::sleep(std::time::Duration::from_millis(25 * (attempt + 1)));
        #[cfg(not(windows))]
        break;
    }
    Err(last.unwrap_or_else(|| "installIoFailed: staging cleanup failed".to_owned()))
}

fn make_mutable(root: &Path) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(io_error("read removal tree"))? {
        let path = entry.map_err(io_error("read removal entry"))?.path();
        let metadata = fs::symlink_metadata(&path).map_err(io_error("inspect removal entry"))?;
        if is_special(&metadata) {
            return Err("installPathUnsafe: installer-owned tree contains a link or reparse point"
                .to_owned());
        }
        if metadata.is_dir() {
            make_mutable(&path)?;
        } else {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)
                .map_err(io_error("make installed file removable"))?;
        }
    }
    Ok(())
}

fn run_archive_check(root: &Path) -> Result<(), String> {
    let checker =
        root.join("bin").join(if cfg!(windows) { "archive-check.exe" } else { "archive-check" });
    reject_special(&checker, "archive checker")?;
    let status =
        Command::new(&checker).arg(root).status().map_err(io_error("start archive checker"))?;
    if status.success() {
        Ok(())
    } else {
        Err("installIntegrityFailed: archive checker rejected installed content".to_owned())
    }
}

fn absolute_clean(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path.components().any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(format!("installPathUnsafe: {label} must be an absolute normalized path"));
    }
    Ok(path.to_path_buf())
}

fn canonical_safe_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = absolute_clean(path, label)?;
    verify_directory(&path, false)?;
    fs::canonicalize(path).map_err(io_error("canonicalize directory"))
}

fn ensure_safe_parent(path: &Path) -> Result<(), String> {
    let mut candidate =
        path.parent().ok_or_else(|| "installPathUnsafe: path has no parent".to_owned())?;
    while !candidate.exists() {
        candidate = candidate
            .parent()
            .ok_or_else(|| "installPathUnsafe: no existing path ancestor".to_owned())?;
    }
    verify_directory(candidate, false)
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    if path.exists() {
        return verify_directory(path, false);
    }
    fs::create_dir_all(path).map_err(io_error("create installer directory"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(io_error("protect installer directory"))?;
    }
    Ok(())
}

fn verify_directory(path: &Path, require_owner: bool) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(io_error("inspect directory"))?;
    if !metadata.is_dir() || is_special(&metadata) {
        return Err(format!("installPathUnsafe: unsafe directory: {}", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if require_owner && metadata.uid() != unsafe { geteuid() } {
            return Err(format!(
                "installPathUnsafe: directory is not owned by the current user: {}",
                path.display()
            ));
        }
        if metadata.mode() & 0o022 != 0 && !(metadata.uid() == 0 && metadata.mode() & 0o1000 != 0) {
            return Err(format!(
                "installPathUnsafe: directory is writable by another account: {}",
                path.display()
            ));
        }
    }
    #[cfg(windows)]
    let _ = require_owner;
    Ok(())
}

#[cfg(unix)]
unsafe extern "C" {
    fn geteuid() -> u32;
}

fn reject_special(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(io_error("inspect path"))?;
    if is_special(&metadata) || !metadata.is_file() {
        Err(format!("installPathUnsafe: {label} is not a regular file"))
    } else {
        Ok(())
    }
}

fn is_special(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn read_small(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(io_error("open authority"))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(io_error("read authority"))?;
    if bytes.len() > 4096 {
        return Err("installAuthorityInvalid: authority file is too large".to_owned());
    }
    String::from_utf8(bytes)
        .map(|value| value.trim().to_owned())
        .map_err(|_| "installAuthorityInvalid: authority is not UTF-8".to_owned())
}

fn validate_hash(value: &str) -> Result<(), String> {
    if value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err("installAuthorityInvalid: archive manifest hash is invalid".to_owned())
    }
}

fn sync_directory(path: &Path) -> Result<(), String> {
    sync_directory_platform(path)
}

#[cfg(unix)]
fn sync_directory_platform(path: &Path) -> Result<(), String> {
    File::open(path).and_then(|file| file.sync_all()).map_err(io_error("sync installer directory"))
}

#[cfg(windows)]
fn sync_directory_platform(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_ALL: u32 = 0x0000_0007;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const INVALID_HANDLE_VALUE: isize = -1;
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const std::ffi::c_void,
            creation: u32,
            flags: u32,
            template: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
    }
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `name` is a live NUL-terminated path and the other pointer arguments are null.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle as isize == INVALID_HANDLE_VALUE {
        return Err(format!(
            "installIoFailed: open installer directory for sync ({})",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `CreateFileW` returned an owned kernel handle.
    let file = unsafe { File::from_raw_handle(handle) };
    drop(file);
    // Authority files are flushed before MOVEFILE_WRITE_THROUGH publication.
    // Windows does not support FlushFileBuffers on an opened directory handle.
    Ok(())
}

fn io_error(action: &'static str) -> impl FnOnce(std::io::Error) -> String {
    move |error| format!("installIoFailed: {action} ({error})")
}

struct InstallLock {
    path: PathBuf,
}

impl InstallLock {
    fn acquire(prefix: &Path) -> Result<Self, String> {
        let path = prefix.join(LOCK);
        fs::create_dir(&path).map_err(|error| {
            format!("installBusy: another install or uninstall is active ({error})")
        })?;
        Ok(Self { path })
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_hash_is_canonical_lowercase_sha256() {
        assert!(validate_hash(&"a".repeat(64)).is_ok());
        assert!(validate_hash(&"A".repeat(64)).is_err());
        assert!(validate_hash(&"0".repeat(63)).is_err());
        assert!(validate_hash(&format!("{}g", "0".repeat(63))).is_err());
    }

    #[test]
    fn paths_must_be_absolute_and_lexically_normal() {
        assert!(absolute_clean(Path::new("relative"), "fixture").is_err());
        let root = std::env::current_dir().unwrap();
        assert!(absolute_clean(&root, "fixture").is_ok());
        assert!(absolute_clean(&root.join("child").join(".."), "fixture").is_err());
    }

    #[test]
    fn interrupted_install_recovery_never_follows_staging_links() {
        let root =
            std::env::temp_dir().join(format!("into-md-installer-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("versions")).unwrap();
        fs::write(
            root.join(TRANSACTION),
            format!("install\n{}\n{}\n", "0".repeat(64), root.join("outside").display()),
        )
        .unwrap();
        let error = recover(&root, &root.join("command")).unwrap_err();
        assert!(error.contains("escaped versions directory"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prepared_uninstall_is_rolled_back_during_recovery() {
        let root =
            std::env::temp_dir().join(format!("into-md-uninstall-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let removed = root.join(".removed-versions-fixture");
        fs::create_dir(&removed).unwrap();
        let command = root.join("command");
        fs::create_dir(&command).unwrap();
        #[cfg(windows)]
        fs::write(command.join("into-md.exe"), b"launcher").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("target", command.join("into-md")).unwrap();
        fs::write(
            root.join(TRANSACTION),
            format!("uninstall-prepared\n{}\n{}\n", removed.display(), command.display()),
        )
        .unwrap();
        recover(&root, &command).unwrap();
        assert!(root.join("versions").is_dir());
        assert!(!root.join(TRANSACTION).exists());
        fs::remove_dir_all(root).unwrap();
    }
}

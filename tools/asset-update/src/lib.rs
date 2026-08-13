//! Descriptor-relative updater for the checked-in Web console assets.

use std::fmt;
use std::path::Path;

/// A stable failure from the checked-asset updater.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateError(String);

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for UpdateError {}

/// Atomically replaces `web/console/dist` with the supplied generated tree.
///
/// macOS and Linux use descriptor-relative operations rooted in a continuously
/// open, no-follow `web/console` directory. Other platforms fail before any
/// workspace mutation because this tool has no weaker pathname fallback.
///
/// # Errors
///
/// Returns [`UpdateError`] if an input is unsafe, an entry changes during the
/// transaction, the platform lacks the required primitives, or an I/O operation
/// fails.
pub fn update_assets(workspace: &Path, generated: &Path) -> Result<(), UpdateError> {
    platform::update_assets(workspace, generated)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::UpdateError;
    use std::path::Path;

    pub(super) fn update_assets(_workspace: &Path, _generated: &Path) -> Result<(), UpdateError> {
        Err(UpdateError(
            "assetUpdateUnavailable: descriptor-relative asset update is unsupported on this platform"
                .to_owned(),
        ))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod platform {
    use super::UpdateError;
    use rustix::fd::{AsFd, OwnedFd};
    use rustix::fs::{
        AtFlags, Dir, FileType, Mode, OFlags, RenameFlags, fchmod, fstat, fsync, mkdirat, open,
        openat, renameat_with, statat, unlinkat,
    };
    use rustix::io::{Errno, read, write};
    use std::ffi::{CStr, CString};
    use std::fs;
    use std::path::{Component, Path};

    const DIRECTORY_FLAGS: OFlags =
        OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
    const FILE_FLAGS: OFlags = OFlags::RDONLY.union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Identity {
        device: i128,
        inode: i128,
        kind: FileType,
    }

    impl Identity {
        fn from_stat(stat: &rustix::fs::Stat) -> Self {
            Self {
                device: stat.st_dev.into(),
                inode: stat.st_ino.into(),
                kind: FileType::from_raw_mode(stat.st_mode),
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum HookStage {
        Temp,
        Parent,
        Destination,
        Backup,
    }

    #[cfg_attr(not(test), allow(dead_code))]
    struct HookContext<'a> {
        parent_path: &'a Path,
        temporary_name: &'a CStr,
    }

    pub(super) fn update_assets(workspace: &Path, generated: &Path) -> Result<(), UpdateError> {
        update_assets_with_hook(workspace, generated, &mut |_stage, _context| Ok(()))
    }

    #[allow(clippy::too_many_lines)]
    fn update_assets_with_hook(
        workspace: &Path,
        generated: &Path,
        hook: &mut impl FnMut(HookStage, &HookContext<'_>) -> Result<(), UpdateError>,
    ) -> Result<(), UpdateError> {
        if !workspace.is_absolute() {
            return Err(UpdateError("workspace must be an absolute path".to_owned()));
        }
        let workspace = workspace.to_path_buf();
        let parent_path = workspace.join("web/console");
        let generated =
            fs::canonicalize(generated).map_err(|error| failure("generated assets", error))?;
        let parent = open_absolute_directory(&parent_path, "workspace console directory")?;
        let parent_identity = identity(&parent)?;
        let source = open_absolute_directory(&generated, "generated asset directory")?;
        validate_tree(&source, "generated assets")?;

        let destination = open_optional_directory(&parent, c("dist"), "destination")?;
        if let Some(fd) = &destination {
            validate_tree(fd, "destination")?;
        }
        require_absent(&parent, c(".dist-backup"), "backup")?;

        let temporary_name = create_temporary(&parent)?;
        let temporary = open_directory_at(&parent, &temporary_name, "temporary directory")?;
        let temporary_identity = identity(&temporary)?;
        let context = HookContext { parent_path: &parent_path, temporary_name: &temporary_name };

        let operation = (|| {
            copy_tree(&source, &temporary)?;
            fchmod(&temporary, Mode::from_raw_mode(0o755))
                .map_err(|error| failure("chmod temporary directory", error))?;
            fsync(&temporary).map_err(|error| failure("sync temporary directory", error))?;

            require_named_identity(
                &parent,
                &temporary_name,
                temporary_identity,
                "temporary directory",
            )?;
            hook(HookStage::Temp, &context)?;
            require_named_identity(
                &parent,
                &temporary_name,
                temporary_identity,
                "temporary directory",
            )?;

            verify_parent_binding(&parent_path, parent_identity)?;
            hook(HookStage::Parent, &context)?;
            verify_parent_binding(&parent_path, parent_identity)?;

            let destination_identity = destination.as_ref().map(identity).transpose()?;
            require_optional_identity(&parent, c("dist"), destination_identity, "destination")?;
            hook(HookStage::Destination, &context)?;
            require_optional_identity(&parent, c("dist"), destination_identity, "destination")?;

            require_absent(&parent, c(".dist-backup"), "backup")?;
            hook(HookStage::Backup, &context)?;
            require_absent(&parent, c(".dist-backup"), "backup")?;

            if let (Some(old), Some(old_identity)) = (&destination, destination_identity) {
                exchange(&parent, &temporary_name, c("dist"))?;
                if require_named_identity(
                    &parent,
                    c("dist"),
                    temporary_identity,
                    "published destination",
                )
                .is_err()
                    || require_named_identity(
                        &parent,
                        &temporary_name,
                        old_identity,
                        "exchanged destination",
                    )
                    .is_err()
                {
                    let _ = exchange(&parent, c("dist"), &temporary_name);
                    return Err(UpdateError(
                        "asset entries changed during atomic exchange".to_owned(),
                    ));
                }
                if let Err(error) = rename_no_replace(
                    &parent,
                    &temporary_name,
                    &parent,
                    c(".dist-backup"),
                    "move previous destination to backup",
                ) {
                    let _ = exchange(&parent, c("dist"), &temporary_name);
                    return Err(error);
                }
                require_named_identity(&parent, c(".dist-backup"), old_identity, "backup")?;
                remove_named_tree(&parent, c(".dist-backup"), old, old_identity, "backup")?;
            } else {
                rename_no_replace(
                    &parent,
                    &temporary_name,
                    &parent,
                    c("dist"),
                    "publish destination",
                )?;
                require_named_identity(
                    &parent,
                    c("dist"),
                    temporary_identity,
                    "published destination",
                )?;
            }
            fsync(&parent).map_err(|error| failure("sync workspace console directory", error))?;
            Ok(())
        })();

        if operation.is_err()
            && named_identity(&parent, &temporary_name).ok().flatten() == Some(temporary_identity)
        {
            let _ = remove_named_tree(
                &parent,
                &temporary_name,
                &temporary,
                temporary_identity,
                "temporary directory",
            );
        }
        operation
    }

    fn c(value: &'static str) -> &'static CStr {
        CStr::from_bytes_with_nul(match value {
            "dist" => b"dist\0",
            ".dist-backup" => b".dist-backup\0",
            _ => b".\0",
        })
        .expect("static C string")
    }

    fn failure(context: &str, error: impl fmt::Display) -> UpdateError {
        UpdateError(format!("{context}: {error}"))
    }

    use std::fmt;

    fn identity(fd: &impl AsFd) -> Result<Identity, UpdateError> {
        fstat(fd)
            .map(|stat| Identity::from_stat(&stat))
            .map_err(|error| failure("inspect open entry", error))
    }

    fn open_absolute_directory(path: &Path, label: &str) -> Result<OwnedFd, UpdateError> {
        if !path.is_absolute() {
            return Err(UpdateError(format!("{label} must be absolute")));
        }
        let mut current = open("/", DIRECTORY_FLAGS, Mode::empty())
            .map_err(|error| failure(&format!("open {label} root"), error))?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    current = openat(&current, name, DIRECTORY_FLAGS, Mode::empty())
                        .map_err(|error| failure(&format!("open {label} component"), error))?;
                }
                _ => return Err(UpdateError(format!("{label} contains an unsafe component"))),
            }
        }
        Ok(current)
    }

    fn open_directory_at(
        parent: &impl AsFd,
        name: &CStr,
        label: &str,
    ) -> Result<OwnedFd, UpdateError> {
        openat(parent, name, DIRECTORY_FLAGS, Mode::empty())
            .map_err(|error| failure(&format!("open {label}"), error))
    }

    fn open_optional_directory(
        parent: &impl AsFd,
        name: &CStr,
        label: &str,
    ) -> Result<Option<OwnedFd>, UpdateError> {
        match openat(parent, name, DIRECTORY_FLAGS, Mode::empty()) {
            Ok(fd) => Ok(Some(fd)),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(failure(&format!("open {label} without following links"), error)),
        }
    }

    fn verify_parent_binding(path: &Path, expected: Identity) -> Result<(), UpdateError> {
        let current = open_absolute_directory(path, "workspace console directory")?;
        if identity(&current)? != expected {
            return Err(UpdateError(
                "workspace console directory changed during update".to_owned(),
            ));
        }
        Ok(())
    }

    fn named_identity(parent: &impl AsFd, name: &CStr) -> Result<Option<Identity>, UpdateError> {
        match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(Identity::from_stat(&stat))),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(failure("inspect directory entry", error)),
        }
    }

    fn require_named_identity(
        parent: &impl AsFd,
        name: &CStr,
        expected: Identity,
        label: &str,
    ) -> Result<(), UpdateError> {
        if named_identity(parent, name)? != Some(expected) {
            return Err(UpdateError(format!("{label} changed during update")));
        }
        Ok(())
    }

    fn require_optional_identity(
        parent: &impl AsFd,
        name: &CStr,
        expected: Option<Identity>,
        label: &str,
    ) -> Result<(), UpdateError> {
        if named_identity(parent, name)? != expected {
            return Err(UpdateError(format!("{label} changed during update")));
        }
        Ok(())
    }

    fn require_absent(parent: &impl AsFd, name: &CStr, label: &str) -> Result<(), UpdateError> {
        if named_identity(parent, name)?.is_some() {
            return Err(UpdateError(format!("{label} already exists")));
        }
        Ok(())
    }

    fn create_temporary(parent: &impl AsFd) -> Result<CString, UpdateError> {
        for attempt in 0_u32..1024 {
            let name = CString::new(format!(".dist-update-{}-{attempt}", std::process::id()))
                .map_err(|error| failure("temporary directory name", error))?;
            match mkdirat(parent, &name, Mode::from_raw_mode(0o700)) {
                Ok(()) => return Ok(name),
                Err(Errno::EXIST) => {}
                Err(error) => return Err(failure("create temporary directory", error)),
            }
        }
        Err(UpdateError("cannot allocate a unique temporary directory".to_owned()))
    }

    fn directory_names(fd: &impl AsFd) -> Result<Vec<CString>, UpdateError> {
        let mut directory = Dir::read_from(fd).map_err(|error| failure("read directory", error))?;
        let mut names = Vec::new();
        for entry in &mut directory {
            let entry = entry.map_err(|error| failure("read directory entry", error))?;
            if entry.file_name().to_bytes() != b"." && entry.file_name().to_bytes() != b".." {
                names.push(entry.file_name().to_owned());
            }
        }
        names.sort_by(|left, right| left.to_bytes().cmp(right.to_bytes()));
        Ok(names)
    }

    fn validate_tree(fd: &impl AsFd, label: &str) -> Result<(), UpdateError> {
        for name in directory_names(fd)? {
            let stat = statat(fd, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| failure(&format!("inspect {label} entry"), error))?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    let child = open_directory_at(fd, &name, label)?;
                    require_named_identity(fd, &name, identity(&child)?, label)?;
                    validate_tree(&child, label)?;
                }
                FileType::RegularFile => {
                    let child = openat(fd, &name, FILE_FLAGS, Mode::empty())
                        .map_err(|error| failure(&format!("open {label} file"), error))?;
                    require_named_identity(fd, &name, identity(&child)?, label)?;
                }
                _ => return Err(UpdateError(format!("{label} contains a non-regular entry"))),
            }
        }
        Ok(())
    }

    fn copy_tree(source: &impl AsFd, destination: &impl AsFd) -> Result<(), UpdateError> {
        for name in directory_names(source)? {
            let stat = statat(source, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| failure("inspect generated entry", error))?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    let source_child = open_directory_at(source, &name, "generated directory")?;
                    require_named_identity(
                        source,
                        &name,
                        identity(&source_child)?,
                        "generated directory",
                    )?;
                    mkdirat(destination, &name, Mode::from_raw_mode(0o700))
                        .map_err(|error| failure("create generated directory", error))?;
                    let destination_child =
                        open_directory_at(destination, &name, "temporary directory")?;
                    copy_tree(&source_child, &destination_child)?;
                    fchmod(&destination_child, Mode::from_raw_mode(0o755))
                        .map_err(|error| failure("chmod generated directory", error))?;
                    fsync(&destination_child)
                        .map_err(|error| failure("sync generated directory", error))?;
                }
                FileType::RegularFile => copy_file(source, destination, &name)?,
                _ => {
                    return Err(UpdateError(
                        "generated assets contain a non-regular entry".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn copy_file(
        source: &impl AsFd,
        destination: &impl AsFd,
        name: &CStr,
    ) -> Result<(), UpdateError> {
        let input = openat(source, name, FILE_FLAGS, Mode::empty())
            .map_err(|error| failure("open generated file", error))?;
        require_named_identity(source, name, identity(&input)?, "generated file")?;
        let output = openat(
            destination,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| failure("create temporary file", error))?;
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let count = read(&input, &mut buffer[..])
                .map_err(|error| failure("read generated file", error))?;
            if count == 0 {
                break;
            }
            let mut remaining = &buffer[..count];
            while !remaining.is_empty() {
                let written = write(&output, remaining)
                    .map_err(|error| failure("write temporary file", error))?;
                if written == 0 {
                    return Err(UpdateError("write temporary file made no progress".to_owned()));
                }
                remaining = &remaining[written..];
            }
        }
        fchmod(&output, Mode::from_raw_mode(0o644))
            .map_err(|error| failure("chmod temporary file", error))?;
        fsync(&output).map_err(|error| failure("sync temporary file", error))?;
        Ok(())
    }

    fn rename_no_replace(
        old_parent: &impl AsFd,
        old_name: &CStr,
        new_parent: &impl AsFd,
        new_name: &CStr,
        label: &str,
    ) -> Result<(), UpdateError> {
        renameat_with(old_parent, old_name, new_parent, new_name, RenameFlags::NOREPLACE)
            .map_err(|error| failure(label, error))
    }

    fn exchange(parent: &impl AsFd, left: &CStr, right: &CStr) -> Result<(), UpdateError> {
        renameat_with(parent, left, parent, right, RenameFlags::EXCHANGE)
            .map_err(|error| failure("atomically exchange asset directories", error))
    }

    fn remove_named_tree(
        parent: &impl AsFd,
        name: &CStr,
        tree: &impl AsFd,
        expected: Identity,
        label: &str,
    ) -> Result<(), UpdateError> {
        require_named_identity(parent, name, expected, label)?;
        clear_tree(tree)?;
        require_named_identity(parent, name, expected, label)?;
        unlinkat(parent, name, AtFlags::REMOVEDIR)
            .map_err(|error| failure(&format!("remove {label}"), error))
    }

    fn clear_tree(directory: &impl AsFd) -> Result<(), UpdateError> {
        fchmod(directory, Mode::from_raw_mode(0o700))
            .map_err(|error| failure("make retired directory removable", error))?;
        for name in directory_names(directory)? {
            let stat = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| failure("inspect retired entry", error))?;
            let entry_identity = Identity::from_stat(&stat);
            match entry_identity.kind {
                FileType::Directory => {
                    let child = open_directory_at(directory, &name, "retired directory")?;
                    require_named_identity(
                        directory,
                        &name,
                        identity(&child)?,
                        "retired directory",
                    )?;
                    clear_tree(&child)?;
                    require_named_identity(directory, &name, entry_identity, "retired directory")?;
                    unlinkat(directory, &name, AtFlags::REMOVEDIR)
                        .map_err(|error| failure("remove retired directory", error))?;
                }
                FileType::RegularFile => {
                    let child = openat(directory, &name, FILE_FLAGS, Mode::empty())
                        .map_err(|error| failure("open retired file", error))?;
                    require_named_identity(directory, &name, identity(&child)?, "retired file")?;
                    unlinkat(directory, &name, AtFlags::empty())
                        .map_err(|error| failure("remove retired file", error))?;
                }
                _ => {
                    return Err(UpdateError(
                        "retired asset tree contains a non-regular entry".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::ffi::OsStr;
        use std::fs::{self, Permissions};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        use std::path::PathBuf;
        use tempfile::TempDir;

        struct Fixture {
            _root: TempDir,
            workspace: PathBuf,
            generated: PathBuf,
            console: PathBuf,
            external: PathBuf,
            protected: PathBuf,
        }

        #[derive(Debug, PartialEq, Eq)]
        struct Snapshot {
            contents: Vec<u8>,
            inode: u64,
            mode: u32,
        }

        impl Fixture {
            fn new() -> Self {
                let root = tempfile::tempdir().unwrap();
                let canonical_root = fs::canonicalize(root.path()).unwrap();
                let workspace = canonical_root.join("workspace");
                let console = workspace.join("web/console");
                let generated = canonical_root.join("generated");
                let external = canonical_root.join("external");
                fs::create_dir_all(console.join("dist/assets")).unwrap();
                fs::write(console.join("dist/old.txt"), b"old\n").unwrap();
                fs::create_dir_all(generated.join("assets")).unwrap();
                fs::write(generated.join("asset-manifest.json"), b"generated manifest\n").unwrap();
                fs::write(generated.join("assets/app.js"), b"generated app\n").unwrap();
                fs::create_dir_all(&external).unwrap();
                let protected = external.join("protected.txt");
                fs::write(&protected, b"do not modify\n").unwrap();
                fs::set_permissions(&protected, Permissions::from_mode(0o444)).unwrap();
                Self { _root: root, workspace, generated, console, external, protected }
            }

            fn snapshot(&self) -> Snapshot {
                let metadata = fs::metadata(&self.protected).unwrap();
                Snapshot {
                    contents: fs::read(&self.protected).unwrap(),
                    inode: metadata.ino(),
                    mode: metadata.mode() & 0o777,
                }
            }
        }

        #[test]
        fn initial_symlinks_fail_closed_without_touching_external_files() {
            for attacked in ["destination", "backup", "component"] {
                let fixture = Fixture::new();
                let before = fixture.snapshot();
                match attacked {
                    "destination" => {
                        fs::remove_dir_all(fixture.console.join("dist")).unwrap();
                        symlink(&fixture.external, fixture.console.join("dist")).unwrap();
                    }
                    "backup" => {
                        symlink(&fixture.external, fixture.console.join(".dist-backup")).unwrap();
                    }
                    "component" => {
                        fs::remove_dir_all(fixture.workspace.join("web")).unwrap();
                        symlink(&fixture.external, fixture.workspace.join("web")).unwrap();
                    }
                    _ => unreachable!(),
                }
                assert!(update_assets(&fixture.workspace, &fixture.generated).is_err());
                assert_eq!(fixture.snapshot(), before, "external file changed for {attacked}");
            }
        }

        #[test]
        fn deterministic_barriers_reject_temp_parent_destination_and_backup_replacement() {
            for attacked in
                [HookStage::Temp, HookStage::Parent, HookStage::Destination, HookStage::Backup]
            {
                let fixture = Fixture::new();
                let before = fixture.snapshot();
                let mut attacked_once = false;
                let error = update_assets_with_hook(
                    &fixture.workspace,
                    &fixture.generated,
                    &mut |stage, context| {
                        if stage != attacked || attacked_once {
                            return Ok(());
                        }
                        attacked_once = true;
                        match stage {
                            HookStage::Temp => {
                                let temporary = context
                                    .parent_path
                                    .join(OsStr::from_bytes(context.temporary_name.to_bytes()));
                                fs::rename(
                                    &temporary,
                                    context.parent_path.join("attacker-saved-temp"),
                                )
                                .map_err(|error| failure("attack temporary directory", error))?;
                                symlink(&fixture.external, &temporary).map_err(|error| {
                                    failure("replace temporary directory", error)
                                })?;
                            }
                            HookStage::Parent => {
                                fs::rename(
                                    context.parent_path,
                                    fixture.workspace.join("console-saved"),
                                )
                                .map_err(|error| failure("attack parent directory", error))?;
                                symlink(&fixture.external, context.parent_path)
                                    .map_err(|error| failure("replace parent directory", error))?;
                            }
                            HookStage::Destination => {
                                fs::rename(
                                    context.parent_path.join("dist"),
                                    context.parent_path.join("dist-saved"),
                                )
                                .map_err(|error| failure("attack destination", error))?;
                                symlink(&fixture.external, context.parent_path.join("dist"))
                                    .map_err(|error| failure("replace destination", error))?;
                            }
                            HookStage::Backup => {
                                symlink(
                                    &fixture.external,
                                    context.parent_path.join(".dist-backup"),
                                )
                                .map_err(|error| failure("replace backup", error))?;
                            }
                        }
                        Ok(())
                    },
                )
                .unwrap_err();
                assert!(attacked_once);
                assert!(!error.0.is_empty(), "empty failure for {attacked:?}");
                assert_eq!(fixture.snapshot(), before, "external file changed during {attacked:?}");
            }
        }

        #[test]
        fn normal_update_is_atomic_and_repeatably_deterministic() {
            let fixture = Fixture::new();
            let mut stages = Vec::new();
            update_assets_with_hook(
                &fixture.workspace,
                &fixture.generated,
                &mut |stage, context| {
                    stages.push(stage);
                    assert_eq!(
                        fs::read(context.parent_path.join("dist/old.txt")).unwrap(),
                        b"old\n"
                    );
                    assert!(!context.parent_path.join("dist/asset-manifest.json").exists());
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(
                stages,
                vec![HookStage::Temp, HookStage::Parent, HookStage::Destination, HookStage::Backup]
            );
            assert_eq!(
                fs::read(fixture.console.join("dist/asset-manifest.json")).unwrap(),
                b"generated manifest\n"
            );
            assert_eq!(
                fs::read(fixture.console.join("dist/assets/app.js")).unwrap(),
                b"generated app\n"
            );
            assert!(!fixture.console.join("dist/old.txt").exists());
            assert!(!fixture.console.join(".dist-backup").exists());
            let first = fs::read(fixture.console.join("dist/assets/app.js")).unwrap();
            update_assets(&fixture.workspace, &fixture.generated).unwrap();
            assert_eq!(fs::read(fixture.console.join("dist/assets/app.js")).unwrap(), first);
            assert_eq!(fs::metadata(fixture.console.join("dist")).unwrap().mode() & 0o777, 0o755);
            assert_eq!(
                fs::metadata(fixture.console.join("dist/assets/app.js")).unwrap().mode() & 0o777,
                0o644
            );
        }
    }
}

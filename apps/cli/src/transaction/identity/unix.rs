use super::*;

#[cfg(unix)]
pub(crate) struct SafeDir {
    pub(in crate::transaction) fd: OwnedFd,
    pub(in crate::transaction) path: PathBuf,
    pub(in crate::transaction) identity: FileIdentity,
}

#[cfg(unix)]
impl SafeDir {
    pub(crate) fn open_absolute(path: &Path) -> Result<Self, CliError> {
        if !path.is_absolute() {
            return Err(recovery_error("directory handle path is not absolute"));
        }
        let mut current_path = PathBuf::from("/");
        let mut fd = rustix::fs::open(
            "/",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    fd = rustix::fs::openat(
                        &fd,
                        name,
                        rustix::fs::OFlags::RDONLY
                            | rustix::fs::OFlags::DIRECTORY
                            | rustix::fs::OFlags::NOFOLLOW
                            | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    )?;
                    current_path.push(name);
                }
                _ => return Err(recovery_error("directory handle path is not normalized")),
            }
        }
        let identity = directory_identity(&fd)?;
        Ok(Self { fd, path: current_path, identity })
    }

    pub(in crate::transaction) fn open_or_create_absolute(path: &Path) -> Result<Self, CliError> {
        if !path.is_absolute() {
            return Err(recovery_error("directory creation path is not absolute"));
        }
        let mut current_path = PathBuf::from("/");
        let mut fd = rustix::fs::open(
            "/",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    let opened = rustix::fs::openat(
                        &fd,
                        name,
                        rustix::fs::OFlags::RDONLY
                            | rustix::fs::OFlags::DIRECTORY
                            | rustix::fs::OFlags::NOFOLLOW
                            | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    );
                    fd = match opened {
                        Ok(opened) => opened,
                        Err(rustix::io::Errno::NOENT) => {
                            match rustix::fs::mkdirat(
                                &fd,
                                name,
                                rustix::fs::Mode::RUSR
                                    | rustix::fs::Mode::WUSR
                                    | rustix::fs::Mode::XUSR
                                    | rustix::fs::Mode::RGRP
                                    | rustix::fs::Mode::XGRP
                                    | rustix::fs::Mode::ROTH
                                    | rustix::fs::Mode::XOTH,
                            ) {
                                Ok(()) => rustix::fs::fsync(&fd)?,
                                // Another batch worker may have created this
                                // exact component after our failed open. The
                                // authenticated NOFOLLOW open below decides
                                // whether the winner created an acceptable
                                // directory.
                                Err(rustix::io::Errno::EXIST) => {}
                                Err(error) => return Err(error.into()),
                            }
                            rustix::fs::openat(
                                &fd,
                                name,
                                rustix::fs::OFlags::RDONLY
                                    | rustix::fs::OFlags::DIRECTORY
                                    | rustix::fs::OFlags::NOFOLLOW
                                    | rustix::fs::OFlags::CLOEXEC,
                                rustix::fs::Mode::empty(),
                            )?
                        }
                        Err(error) => return Err(error.into()),
                    };
                    current_path.push(name);
                }
                _ => return Err(recovery_error("directory creation path is not normalized")),
            }
        }
        let identity = directory_identity(&fd)?;
        Ok(Self { fd, path: current_path, identity })
    }

    pub(crate) fn open_child(&self, name: &OsStr) -> Result<Self, CliError> {
        validate_single_name(name)?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        let identity = directory_identity(&fd)?;
        Ok(Self { fd, path: self.path.join(name), identity })
    }

    pub(crate) fn open_child_optional(&self, name: &OsStr) -> Result<Option<Self>, CliError> {
        validate_single_name(name)?;
        match rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => {
                let identity = directory_identity(&fd)?;
                Ok(Some(Self { fd, path: self.path.join(name), identity }))
            }
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(in crate::transaction) fn open_descendant(
        &self,
        relative: &Path,
    ) -> Result<Self, CliError> {
        if relative.as_os_str().is_empty() {
            let fd = rustix::io::dup(&self.fd)?;
            return Ok(Self { fd, path: self.path.clone(), identity: self.identity.clone() });
        }
        validate_relative_path(relative)?;
        let mut current = Self {
            fd: rustix::io::dup(&self.fd)?,
            path: self.path.clone(),
            identity: self.identity.clone(),
        };
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(recovery_error("descendant path is not normalized"));
            };
            current = current.open_child(name)?;
        }
        Ok(current)
    }

    pub(crate) fn verify_namespace(&self) -> Result<(), CliError> {
        let changed = || {
            CliError::new(
                ExitClass::Io,
                "outputIdentityChanged",
                format!("output directory changed after authentication: {}", self.path.display()),
            )
        };
        let current = Self::open_absolute(&self.path).map_err(|_| changed())?;
        if current.identity != self.identity {
            return Err(changed());
        }
        Ok(())
    }

    pub(crate) fn verify_private_namespace(&self) -> Result<(), CliError> {
        let stat = rustix::fs::fstat(&self.fd)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory
            || stat.st_uid != rustix::process::geteuid().as_raw()
            || stat.st_mode & 0o777 != 0o700
        {
            return Err(recovery_error(
                "managed directory is not private, owner-bound, and descriptor-authenticated",
            ));
        }
        self.verify_namespace()
    }

    pub(crate) fn open_child_private(&self, name: &OsStr) -> Result<Self, CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child(name)?;
        self.verify_private_namespace()?;
        child.verify_private_namespace()?;
        Ok(child)
    }

    pub(crate) fn open_child_private_optional(
        &self,
        name: &OsStr,
    ) -> Result<Option<Self>, CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child_optional(name)?;
        self.verify_private_namespace()?;
        if let Some(child) = &child {
            child.verify_private_namespace()?;
        }
        Ok(child)
    }

    pub(crate) fn open_regular(&self, name: &OsStr) -> Result<File, CliError> {
        validate_single_name(name)?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        let stat = rustix::fs::fstat(&fd)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
            return Err(CliError::new(
                ExitClass::Io,
                "outputTargetTypeDenied",
                format!("not a regular file: {}", self.path.join(name).display()),
            ));
        }
        Ok(File::from(fd))
    }

    pub(in crate::transaction) fn open_regular_append(
        &self,
        name: &OsStr,
    ) -> Result<File, CliError> {
        validate_single_name(name)?;
        self.verify_private_namespace()?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::APPEND
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        let file = File::from(fd);
        verify_private_regular(&file)?;
        self.verify_private_namespace()?;
        Ok(file)
    }

    pub(crate) fn open_regular_optional(&self, name: &OsStr) -> Result<Option<File>, CliError> {
        validate_single_name(name)?;
        match rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => {
                let stat = rustix::fs::fstat(&fd)?;
                if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    != rustix::fs::FileType::RegularFile
                {
                    return Err(recovery_error("optional managed file is not regular"));
                }
                Ok(Some(File::from(fd)))
            }
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn create_regular(&self, name: &OsStr) -> Result<File, CliError> {
        validate_single_name(name)?;
        self.verify_namespace()?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;
        self.verify_namespace()?;
        Ok(File::from(fd))
    }

    pub(crate) fn create_regular_private(&self, name: &OsStr) -> Result<File, CliError> {
        self.verify_private_namespace()?;
        let file = self.create_regular(name)?;
        verify_private_regular(&file)?;
        self.verify_private_namespace()?;
        Ok(file)
    }

    pub(crate) fn open_regular_private(&self, name: &OsStr) -> Result<File, CliError> {
        self.verify_private_namespace()?;
        let file = self.open_regular(name)?;
        verify_private_regular(&file)?;
        self.verify_private_namespace()?;
        Ok(file)
    }

    pub(in crate::transaction) fn inspect_regular(
        &self,
        name: &OsStr,
    ) -> Result<Option<FileIdentity>, CliError> {
        validate_single_name(name)?;
        match rustix::fs::statat(&self.fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat)
                if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    == rustix::fs::FileType::RegularFile =>
            {
                let file = self.open_regular(name)?;
                Ok(Some(file_identity(&file)?))
            }
            Ok(_) => Err(CliError::new(
                ExitClass::Io,
                "outputTargetTypeDenied",
                format!(
                    "output target is not a regular non-link file: {}",
                    self.path.join(name).display()
                ),
            )),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn sync(&self) -> Result<(), CliError> {
        rustix::fs::fsync(&self.fd)?;
        Ok(())
    }

    pub(crate) fn names(&self) -> Result<Vec<OsString>, CliError> {
        self.names_bounded(MAX_RECOVERY_DIRECTORY_ENTRIES)
    }

    pub(crate) fn names_private(&self) -> Result<Vec<OsString>, CliError> {
        self.verify_private_namespace()?;
        let names = self.names()?;
        self.verify_private_namespace()?;
        Ok(names)
    }

    pub(crate) fn names_bounded(&self, limit: usize) -> Result<Vec<OsString>, CliError> {
        use std::os::unix::ffi::OsStringExt as _;
        let mut directory = rustix::fs::Dir::read_from(&self.fd)?;
        let mut names = Vec::new();
        while let Some(entry) = directory.read() {
            let entry = entry?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if names.len() >= limit {
                return Err(CliError::new(
                    ExitClass::Io,
                    "transactionRecoveryLimit",
                    format!("recovery scan exceeded {limit} entries under {}", self.path.display()),
                ));
            }
            names.try_reserve(1).map_err(|error| {
                CliError::new(
                    ExitClass::Io,
                    "transactionRecoveryLimit",
                    format!("cannot reserve recovery directory entry: {error}"),
                )
            })?;
            names.push(OsString::from_vec(bytes.to_vec()));
        }
        Ok(names)
    }

    pub(crate) fn for_each_name_bounded(
        &self,
        limit: usize,
        context: &ExecutionContext,
        mut visit: impl FnMut(&OsStr) -> Result<(), CliError>,
    ) -> Result<(), CliError> {
        use std::os::unix::ffi::OsStrExt as _;
        let mut directory = rustix::fs::Dir::read_from(&self.fd)?;
        let mut scanned = 0_usize;
        while let Some(entry) = directory.read() {
            context.checkpoint().map_err(CliError::from)?;
            let entry = entry?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if scanned >= limit {
                return Err(CliError::new(
                    ExitClass::Io,
                    "transactionRecoveryLimit",
                    format!("recovery scan exceeded {limit} entries under {}", self.path.display()),
                ));
            }
            scanned += 1;
            let name = OsStr::from_bytes(bytes);
            let _scratch = context
                .reserve_memory(directory_name_memory_bytes(name))
                .map_err(CliError::from)?;
            visit(name)?;
        }
        Ok(())
    }

    pub(crate) fn create_child_private(&self, name: &OsStr) -> Result<Self, CliError> {
        validate_single_name(name)?;
        self.verify_private_namespace()?;
        rustix::fs::mkdirat(
            &self.fd,
            name,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
        )?;
        self.sync()?;
        let child = self.open_child(name)?;
        self.verify_private_namespace()?;
        child.verify_private_namespace()?;
        Ok(child)
    }

    pub(crate) fn rename_child_private_no_replace(
        &self,
        source: &OsStr,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        self.rename_child_no_replace(source, destination)?;
        self.verify_private_namespace()
    }

    pub(crate) fn rename_child_private_to_no_replace(
        &self,
        source: &OsStr,
        destination_directory: &Self,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        destination_directory.verify_private_namespace()?;
        self.rename_child_to_no_replace(source, destination_directory, destination)?;
        self.verify_private_namespace()?;
        destination_directory.verify_private_namespace()
    }

    pub(crate) fn remove_regular_private(&self, name: &OsStr) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        let file = self.open_regular_private(name)?;
        drop(file);
        self.remove_regular(name)?;
        self.verify_private_namespace()
    }

    pub(crate) fn remove_empty_child_private(&self, name: &OsStr) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child(name)?;
        child.verify_private_namespace()?;
        self.remove_empty_child(name)?;
        self.verify_private_namespace()
    }

    pub(crate) fn rename_child_no_replace(
        &self,
        source: &OsStr,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        validate_single_name(source)?;
        validate_single_name(destination)?;
        self.verify_namespace()?;
        rustix::fs::renameat_with(
            &self.fd,
            source,
            &self.fd,
            destination,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(crate) fn rename_child_to_no_replace(
        &self,
        source: &OsStr,
        destination_directory: &Self,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        validate_single_name(source)?;
        validate_single_name(destination)?;
        self.verify_namespace()?;
        destination_directory.verify_namespace()?;
        rustix::fs::renameat_with(
            &self.fd,
            source,
            &destination_directory.fd,
            destination,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
        self.sync()?;
        destination_directory.sync()?;
        self.verify_namespace()?;
        destination_directory.verify_namespace()
    }

    pub(crate) fn remove_regular(&self, name: &OsStr) -> Result<(), CliError> {
        validate_single_name(name)?;
        self.verify_namespace()?;
        let file = self.open_regular(name)?;
        if rustix::fs::fstat(&file)?.st_nlink != 1 {
            return Err(recovery_error("managed file has an external hard link"));
        }
        rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::empty())?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(in crate::transaction) fn remove_committed_backup(
        &self,
        name: &OsStr,
        expected: &FileIdentity,
    ) -> Result<(), CliError> {
        validate_single_name(name)?;
        self.verify_private_namespace()?;
        let file = self.open_regular(name)?;
        if file_identity(&file)? != *expected {
            return Err(recovery_error("committed backup identity changed before cleanup"));
        }
        // A pre-existing output may legitimately have another hard-link name.
        // The transaction directory is private and the journal binds this exact
        // inode, so unlinking only its private backup name cannot remove or
        // mutate the caller-owned alias.
        rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::empty())?;
        self.sync()?;
        self.verify_private_namespace()
    }

    pub(crate) fn remove_empty_child(&self, name: &OsStr) -> Result<(), CliError> {
        validate_single_name(name)?;
        self.verify_namespace()?;
        let child = self.open_child(name)?;
        if !child.is_empty()? {
            return Err(recovery_error("managed directory is not empty"));
        }
        rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::REMOVEDIR)?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(crate) fn is_empty(&self) -> Result<bool, CliError> {
        use std::os::unix::ffi::OsStrExt as _;
        let mut directory = rustix::fs::Dir::read_from(&self.fd)?;
        while let Some(entry) = directory.read() {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn measured_tree_bytes(
        &self,
        max_depth: u8,
        max_entries: usize,
    ) -> Result<u64, CliError> {
        fn visit(
            directory: &SafeDir,
            depth: u8,
            max_depth: u8,
            entries: &mut usize,
            max_entries: usize,
        ) -> Result<u64, CliError> {
            use std::os::unix::ffi::OsStrExt as _;
            if depth > max_depth {
                return Err(recovery_error("managed storage depth exceeds its limit"));
            }
            let mut reader = rustix::fs::Dir::read_from(&directory.fd)?;
            let mut total = 0_u64;
            while let Some(entry) = reader.read() {
                let entry = entry?;
                let name = entry.file_name();
                if name.to_bytes() == b"." || name.to_bytes() == b".." {
                    continue;
                }
                *entries = entries
                    .checked_add(1)
                    .ok_or_else(|| recovery_error("managed storage entry count overflow"))?;
                if *entries > max_entries {
                    return Err(recovery_error("managed storage entry count exceeds its limit"));
                }
                let stat =
                    rustix::fs::statat(&directory.fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
                match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
                    rustix::fs::FileType::Directory => {
                        let child = directory.open_child(OsStr::from_bytes(name.to_bytes()))?;
                        total = total
                            .checked_add(visit(&child, depth + 1, max_depth, entries, max_entries)?)
                            .ok_or_else(|| recovery_error("managed storage byte count overflow"))?;
                    }
                    rustix::fs::FileType::RegularFile if stat.st_nlink == 1 => {
                        total = total
                            .checked_add(u64::try_from(stat.st_size).map_err(|_| {
                                recovery_error("managed file size is not representable")
                            })?)
                            .ok_or_else(|| recovery_error("managed storage byte count overflow"))?;
                    }
                    _ => return Err(recovery_error("managed storage contains an unsafe object")),
                }
            }
            Ok(total)
        }
        let mut entries = 0;
        visit(self, 0, max_depth, &mut entries, max_entries)
    }
}

fn directory_name_memory_bytes(name: &OsStr) -> u64 {
    u64::try_from(name.as_encoded_bytes().len())
        .unwrap_or(u64::MAX)
        .saturating_mul(2)
        .saturating_add(u64::try_from(std::mem::size_of::<OsString>()).unwrap_or(u64::MAX))
        .saturating_add(64)
}

#[cfg(unix)]
pub(in crate::transaction) fn fd_identity(
    fd: &impl std::os::fd::AsFd,
) -> Result<FileIdentity, CliError> {
    let stat = rustix::fs::fstat(fd)?;
    Ok(FileIdentity {
        platform: "unix".into(),
        first: u64::try_from(stat.st_dev).unwrap_or(u64::MAX),
        #[allow(clippy::useless_conversion)]
        second: u64::try_from(stat.st_ino).unwrap_or(u64::MAX),
        size: u64::try_from(stat.st_size).unwrap_or(u64::MAX),
    })
}

#[cfg(unix)]
pub(in crate::transaction) fn directory_identity(
    fd: &impl std::os::fd::AsFd,
) -> Result<FileIdentity, CliError> {
    let mut identity = fd_identity(fd)?;
    identity.size = 0;
    Ok(identity)
}

#[cfg(unix)]
pub(in crate::transaction) fn verify_private_regular(file: &File) -> Result<(), CliError> {
    let stat = rustix::fs::fstat(file)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o777 != 0o600
        || stat.st_nlink != 1
    {
        return Err(recovery_error("managed file is not private, owner-bound, and singly linked"));
    }
    Ok(())
}

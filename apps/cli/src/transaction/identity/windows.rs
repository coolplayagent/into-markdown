use super::{
    CliError, Component, Digest, ExecutionContext, ExitClass, File, FileIdentity, OpenOptions,
    OsStr, OsString, Path, PathBuf, Read, TransactionSource, Write, file_identity, io,
    recovery_error, validate_relative_path, validate_single_name, verify_name_identity,
};

#[cfg(windows)]
pub(crate) struct SafeDir {
    pub(in crate::transaction) directory: cap_std::fs::Dir,
    pub(in crate::transaction) path: PathBuf,
    pub(in crate::transaction) identity: FileIdentity,
}

#[cfg(windows)]
impl SafeDir {
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const REPARSE_ATTRIBUTE: u64 = 0x0000_0400;

    fn from_file(path: PathBuf, file: File) -> Result<Self, CliError> {
        let metadata = file.metadata()?;
        let information = winapi_util::file::information(&file)?;
        if !metadata.is_dir() || information.file_attributes() & Self::REPARSE_ATTRIBUTE != 0 {
            return Err(recovery_error("directory handle is not a regular non-reparse directory"));
        }
        let identity = FileIdentity {
            platform: "windows".into(),
            first: information.volume_serial_number(),
            second: information.file_index(),
            size: 0,
        };
        Ok(Self { directory: cap_std::fs::Dir::from_std_file(file), path, identity })
    }

    fn open_direct(path: &Path) -> Result<Self, CliError> {
        use std::os::windows::fs::OpenOptionsExt as _;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .share_mode(0x1 | 0x2 | 0x4)
            .custom_flags(Self::BACKUP_SEMANTICS | Self::OPEN_REPARSE_POINT);
        Self::from_file(path.to_path_buf(), options.open(path)?)
    }

    pub(crate) fn open_absolute(path: &Path) -> Result<Self, CliError> {
        if !path.is_absolute() {
            return Err(recovery_error("directory handle path is not absolute"));
        }
        let mut root = PathBuf::new();
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => root.push(component.as_os_str()),
                Component::Normal(name) => names.push(name.to_os_string()),
                _ => return Err(recovery_error("directory handle path is not normalized")),
            }
        }
        let mut current = Self::open_direct(&root)?;
        for name in names {
            current = current.open_child(&name)?;
        }
        Ok(current)
    }

    pub(in crate::transaction) fn open_or_create_absolute(path: &Path) -> Result<Self, CliError> {
        if !path.is_absolute() {
            return Err(recovery_error("directory creation path is not absolute"));
        }
        let mut root = PathBuf::new();
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => root.push(component.as_os_str()),
                Component::Normal(name) => names.push(name.to_os_string()),
                _ => return Err(recovery_error("directory creation path is not normalized")),
            }
        }
        let mut current = Self::open_direct(&root)?;
        for name in names {
            current = if let Some(child) = current.open_child_optional(&name)? {
                child
            } else {
                if let Err(error) = current.directory.create_dir(&name) {
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                } else {
                    current.sync()?;
                }
                current.open_child(&name)?
            };
        }
        Ok(current)
    }

    pub(crate) fn open_child(&self, name: &OsStr) -> Result<Self, CliError> {
        use cap_std::fs::OpenOptionsExt as _;
        validate_single_name(name)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options
            .read(true)
            .share_mode(0x1 | 0x2 | 0x4)
            .custom_flags(Self::BACKUP_SEMANTICS | Self::OPEN_REPARSE_POINT);
        let file = self.directory.open_with(name, &options)?.into_std();
        Self::from_file(self.path.join(name), file)
    }

    pub(crate) fn open_child_optional(&self, name: &OsStr) -> Result<Option<Self>, CliError> {
        validate_single_name(name)?;
        match self.directory.symlink_metadata(name) {
            Ok(_) => self.open_child(name).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(in crate::transaction) fn open_descendant(
        &self,
        relative: &Path,
    ) -> Result<Self, CliError> {
        if relative.as_os_str().is_empty() {
            let current = Self::open_absolute(&self.path)?;
            if current.identity != self.identity {
                return Err(recovery_error("directory identity changed"));
            }
            return Ok(current);
        }
        validate_relative_path(relative)?;
        let mut current = Self::open_absolute(&self.path)?;
        if current.identity != self.identity {
            return Err(recovery_error("directory identity changed"));
        }
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(recovery_error("descendant path is not normalized"));
            };
            current = current.open_child(name)?;
        }
        Ok(current)
    }

    pub(crate) fn open_regular(&self, name: &OsStr) -> Result<File, CliError> {
        use cap_std::fs::OpenOptionsExt as _;
        validate_single_name(name)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).custom_flags(Self::OPEN_REPARSE_POINT);
        let file = self.directory.open_with(name, &options)?.into_std();
        let information = winapi_util::file::information(&file)?;
        if !file.metadata()?.is_file()
            || information.file_attributes() & Self::REPARSE_ATTRIBUTE != 0
            || information.number_of_links() != 1
        {
            return Err(recovery_error("managed file identity rejected"));
        }
        Ok(file)
    }

    pub(in crate::transaction) fn open_regular_append(
        &self,
        name: &OsStr,
    ) -> Result<File, CliError> {
        use cap_std::fs::OpenOptionsExt as _;
        validate_single_name(name)?;
        self.verify_private_namespace()?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.append(true).custom_flags(Self::OPEN_REPARSE_POINT);
        let file = self.directory.open_with(name, &options)?.into_std();
        let information = winapi_util::file::information(&file)?;
        if !file.metadata()?.is_file()
            || information.file_attributes() & Self::REPARSE_ATTRIBUTE != 0
            || information.number_of_links() != 1
        {
            return Err(recovery_error("managed append file identity rejected"));
        }
        self.verify_private_namespace()?;
        Ok(file)
    }

    pub(in crate::transaction) fn open_lease_file(&self, name: &OsStr) -> Result<File, CliError> {
        use cap_std::fs::OpenOptionsExt as _;
        validate_single_name(name)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).custom_flags(Self::OPEN_REPARSE_POINT);
        let file = self.directory.open_with(name, &options)?.into_std();
        let information = winapi_util::file::information(&file)?;
        if !file.metadata()?.is_file()
            || information.file_attributes() & Self::REPARSE_ATTRIBUTE != 0
            || information.number_of_links() != 2
        {
            return Err(recovery_error("transaction lease identity rejected"));
        }
        Ok(file)
    }

    pub(in crate::transaction) fn inspect_lease_file(
        &self,
        name: &OsStr,
    ) -> Result<Option<FileIdentity>, CliError> {
        validate_single_name(name)?;
        match self.directory.symlink_metadata(name) {
            Ok(_) => self.open_lease_file(name).and_then(|file| file_identity(&file)).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(in crate::transaction) fn remove_lease_file(&self, name: &OsStr) -> Result<(), CliError> {
        let file = self.open_lease_file(name)?;
        drop(file);
        self.verify_namespace()?;
        self.directory.remove_file(name)?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(crate) fn open_regular_optional(&self, name: &OsStr) -> Result<Option<File>, CliError> {
        validate_single_name(name)?;
        match self.directory.symlink_metadata(name) {
            Ok(_) => self.open_regular(name).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn verify_private_namespace(&self) -> Result<(), CliError> {
        into_markdown_process_plugin::verify_windows_plugin_store_path(&self.path).map_err(
            |error| {
                recovery_error(format!(
                    "private transaction directory rejected ({}): {error}",
                    self.path.display()
                ))
            },
        )?;
        self.verify_namespace()
    }

    pub(crate) fn open_child_private(&self, name: &OsStr) -> Result<Self, CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child(name)?;
        child.verify_private_namespace()?;
        Ok(child)
    }

    pub(crate) fn open_child_private_optional(
        &self,
        name: &OsStr,
    ) -> Result<Option<Self>, CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child_optional(name)?;
        if let Some(child) = &child {
            child.verify_private_namespace()?;
        }
        Ok(child)
    }

    pub(crate) fn create_regular(&self, name: &OsStr) -> Result<File, CliError> {
        use cap_std::fs::OpenOptionsExt as _;
        validate_single_name(name)?;
        self.verify_namespace()?;
        let mut options = cap_std::fs::OpenOptions::new();
        options
            .create_new(true)
            .write(true)
            .share_mode(0x1 | 0x2 | 0x4)
            .custom_flags(Self::OPEN_REPARSE_POINT);
        let file = self.directory.open_with(name, &options)?.into_std();
        self.verify_namespace()?;
        Ok(file)
    }

    pub(crate) fn create_regular_private(&self, name: &OsStr) -> Result<File, CliError> {
        self.verify_private_namespace()?;
        let file = self.create_regular(name)?;
        into_markdown_process_plugin::verify_windows_plugin_store_child(&self.path.join(name))
            .map_err(|error| {
                recovery_error(format!(
                    "private transaction member rejected ({}): {error}",
                    self.path.join(name).display()
                ))
            })?;
        Ok(file)
    }

    pub(crate) fn open_regular_private(&self, name: &OsStr) -> Result<File, CliError> {
        self.verify_private_namespace()?;
        let file = self.open_regular(name)?;
        into_markdown_process_plugin::verify_windows_plugin_store_child(&self.path.join(name))
            .map_err(|error| {
                recovery_error(format!(
                    "private transaction member rejected ({}): {error}",
                    self.path.join(name).display()
                ))
            })?;
        Ok(file)
    }

    pub(crate) fn names(&self) -> Result<Vec<OsString>, CliError> {
        self.names_bounded(super::MAX_RECOVERY_DIRECTORY_ENTRIES)
    }

    pub(crate) fn names_private(&self) -> Result<Vec<OsString>, CliError> {
        self.verify_private_namespace()?;
        let names = self.names()?;
        self.verify_private_namespace()?;
        Ok(names)
    }

    pub(crate) fn names_bounded(&self, limit: usize) -> Result<Vec<OsString>, CliError> {
        let mut names = Vec::new();
        for entry in self.directory.entries()? {
            let entry = entry?;
            if names.len() >= limit {
                return Err(recovery_error("recovery directory entry limit exceeded"));
            }
            names.push(entry.file_name());
        }
        Ok(names)
    }

    pub(crate) fn for_each_name_bounded(
        &self,
        limit: usize,
        context: &ExecutionContext,
        mut visit: impl FnMut(&OsStr) -> Result<(), CliError>,
    ) -> Result<(), CliError> {
        for (scanned, entry) in self.directory.entries()?.enumerate() {
            context.checkpoint().map_err(CliError::from)?;
            let name = entry?.file_name();
            if scanned >= limit {
                return Err(recovery_error(format!(
                    "recovery scan exceeded {limit} entries under {}",
                    self.path.display()
                )));
            }
            let _scratch = context
                .reserve_memory(directory_name_memory_bytes(&name))
                .map_err(CliError::from)?;
            visit(&name)?;
        }
        Ok(())
    }

    pub(crate) fn create_child_private(&self, name: &OsStr) -> Result<Self, CliError> {
        validate_single_name(name)?;
        self.verify_private_namespace()?;
        into_markdown_process_plugin::create_windows_plugin_store_directory(&self.path.join(name))
            .map_err(|error| recovery_error(error.to_string()))?;
        self.sync()?;
        self.open_child_private(name)
    }

    pub(crate) fn rename_child_no_replace(
        &self,
        source: &OsStr,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        self.verify_namespace()?;
        let pinned = self.directory.try_clone()?.into_std_file();
        into_markdown_process_plugin::rename_windows_plugin_file_no_replace(
            &pinned,
            source,
            destination,
        )
        .map_err(|error| recovery_error(error.to_string()))?;
        self.sync()?;
        self.verify_namespace()
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

    pub(crate) fn rename_child_to_no_replace(
        &self,
        source: &OsStr,
        destination_directory: &Self,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        self.verify_namespace()?;
        destination_directory.verify_namespace()?;
        let source_handle = self.directory.try_clone()?.into_std_file();
        let destination_handle = destination_directory.directory.try_clone()?.into_std_file();
        into_markdown_process_plugin::move_windows_plugin_file_no_replace(
            &source_handle,
            source,
            &destination_handle,
            destination,
        )
        .map_err(|error| recovery_error(error.to_string()))?;
        self.sync()?;
        destination_directory.sync()?;
        Ok(())
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

    pub(crate) fn remove_regular(&self, name: &OsStr) -> Result<(), CliError> {
        let expected = self.inspect_regular(name)?.ok_or_else(|| recovery_error("file missing"))?;
        self.verify_namespace()?;
        verify_name_identity(self, name, Some(&expected))?;
        self.directory.remove_file(name)?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(in crate::transaction) fn remove_committed_backup(
        &self,
        name: &OsStr,
        expected: &FileIdentity,
    ) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        verify_name_identity(self, name, Some(expected))?;
        self.directory.remove_file(name)?;
        self.sync()?;
        self.verify_private_namespace()
    }

    pub(crate) fn remove_regular_private(&self, name: &OsStr) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        let file = self.open_regular_private(name)?;
        drop(file);
        self.remove_regular(name)?;
        self.verify_private_namespace()
    }

    pub(crate) fn remove_empty_child(&self, name: &OsStr) -> Result<(), CliError> {
        let child = self.open_child(name)?;
        if !child.is_empty()? {
            return Err(recovery_error("managed directory is not empty"));
        }
        drop(child);
        self.verify_namespace()?;
        self.directory.remove_dir(name)?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(crate) fn is_empty(&self) -> Result<bool, CliError> {
        Ok(self.directory.entries()?.next().transpose()?.is_none())
    }

    pub(crate) fn remove_empty_child_private(&self, name: &OsStr) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child_private(name)?;
        drop(child);
        self.remove_empty_child(name)?;
        self.verify_private_namespace()
    }

    pub(in crate::transaction) fn inspect_regular(
        &self,
        name: &OsStr,
    ) -> Result<Option<FileIdentity>, CliError> {
        self.open_regular_optional(name)?.map(|file| file_identity(&file)).transpose()
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
            if depth > max_depth {
                return Err(recovery_error("managed storage depth exceeds its limit"));
            }
            let mut total = 0_u64;
            for entry in directory.directory.entries()? {
                let name = entry?.file_name();
                *entries = entries.saturating_add(1);
                if *entries > max_entries {
                    return Err(recovery_error("managed storage entry count exceeds its limit"));
                }
                if let Some(file) = directory.open_regular_optional(&name)? {
                    total = total
                        .checked_add(file.metadata()?.len())
                        .ok_or_else(|| recovery_error("managed storage byte count overflow"))?;
                } else {
                    let child = directory.open_child(&name)?;
                    total = total
                        .checked_add(visit(&child, depth + 1, max_depth, entries, max_entries)?)
                        .ok_or_else(|| recovery_error("managed storage byte count overflow"))?;
                }
            }
            Ok(total)
        }
        let mut entries = 0;
        visit(self, 0, max_depth, &mut entries, max_entries)
    }

    pub(in crate::transaction) fn verify_namespace(&self) -> Result<(), CliError> {
        let current = Self::open_absolute(&self.path)?;
        if current.identity != self.identity {
            return Err(CliError::new(
                ExitClass::Io,
                "outputIdentityChanged",
                format!("output directory changed after authentication: {}", self.path.display()),
            ));
        }
        Ok(())
    }

    pub(crate) fn sync(&self) -> Result<(), CliError> {
        match self.directory.try_clone()?.into_std_file().sync_all() {
            Ok(()) => Ok(()),
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || matches!(error.raw_os_error(), Some(1 | 6)) =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
}

fn directory_name_memory_bytes(name: &OsStr) -> u64 {
    u64::try_from(name.as_encoded_bytes().len())
        .unwrap_or(u64::MAX)
        .saturating_mul(2)
        .saturating_add(u64::try_from(std::mem::size_of::<OsString>()).unwrap_or(u64::MAX))
        .saturating_add(64)
}

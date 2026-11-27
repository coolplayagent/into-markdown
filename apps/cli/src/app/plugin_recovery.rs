//! Recovery before reading configuration shared by plugin management and diagnostics.

use super::*;

enum PluginStoreParent {
    UserData(PathBuf),
    #[cfg(not(test))]
    Config(PathBuf),
}

impl PluginStoreParent {
    fn locate() -> Result<Self, CliError> {
        #[cfg(test)]
        {
            TEST_USER_DATA_ANCHOR
                .with(|slot| slot.borrow().clone())
                .map(Self::UserData)
                .ok_or_else(|| CliError::internal("test user-data anchor was not injected"))
        }
        #[cfg(not(test))]
        {
            // global_config_path validates an explicit user-data override.
            let config = config::global_config_path()?;
            let directory =
                config.parent().ok_or_else(|| CliError::config("global scope unavailable"))?;
            if let Some(configured) = std::env::var_os("INTO_MARKDOWN_USER_DATA_HOME") {
                Ok(Self::UserData(PathBuf::from(configured)))
            } else {
                Ok(Self::Config(directory.to_owned()))
            }
        }
    }

    fn product_path(&self) -> PathBuf {
        match self {
            Self::UserData(anchor) => anchor.join("into-markdown"),
            #[cfg(not(test))]
            Self::Config(directory) => directory.join("plugin-data"),
        }
    }

    fn prepare(&self) -> Result<PathBuf, CliError> {
        let product = match self {
            Self::UserData(anchor) => {
                #[cfg(test)]
                create_private_user_data_directory(anchor)?;
                canonical_private_user_data_directory(anchor)?.join("into-markdown")
            }
            #[cfg(not(test))]
            Self::Config(directory) => {
                ensure_private_user_data_path(directory)?;
                if fs::symlink_metadata(directory)?.file_type().is_symlink() {
                    return Err(CliError::config("global config parent link rejected"));
                }
                let identity = project_directory_identity(directory)?;
                let canonical = fs::canonicalize(directory)?;
                if project_directory_identity(&canonical)? != identity {
                    return Err(CliError::config("global config parent identity changed"));
                }
                canonical.join("plugin-data")
            }
        };
        prepare_user_data_anchor(&product)?;
        Ok(fs::canonicalize(product)?)
    }
}

fn entry_present(path: &Path) -> Result<bool, CliError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn global_plugin_store_scope() -> Result<(PathBuf, PathBuf), CliError> {
    Ok((PluginStoreParent::locate()?.prepare()?, PathBuf::from("plugins")))
}

pub(super) fn existing_global_plugin_manager() -> Result<PluginManager, CliError> {
    let location = PluginStoreParent::locate()?;
    if !entry_present(&location.product_path().join("plugins"))? {
        return Err(CliError::new(ExitClass::Io, "notFound", "plugin store is not installed"));
    }
    PluginManager::open_persisted_scoped(&location.prepare()?, Path::new("plugins"))
        .map_err(plugin_manager_error)
}

pub(crate) fn recover_plugins_before_config_load(
    cwd: &Path,
    no_config: bool,
) -> Result<(), CliError> {
    if no_config {
        return Ok(());
    }
    let location = PluginStoreParent::locate()?;
    let anchor = location.product_path();
    let global_root = anchor.join("plugins");
    {
        // A reader takes the existing CLI lock shared. A writer creates it
        // before recording the first joint transaction in either store.
        let _reader = if entry_present(&global_root.join(PLUGIN_CLI_LOCK))? {
            let root = canonical_private_user_data_directory(&location.prepare()?.join("plugins"))?;
            Some(lock_cli_plugin_file(&root, false)?)
        } else {
            None
        };
        recover_config_scopes(cwd)?;
        if !joint_journal_present(&global_root)? {
            if !entry_present(&anchor.join("project-plugins"))? {
                return Ok(());
            }
            let project = ProjectScopeAuthority::under_anchor(cwd, anchor)?;
            if !joint_journal_present(&project.anchor.join(project.store_relative))? {
                return Ok(());
            }
        }
    }
    let (global_anchor, global_relative) = global_plugin_store_scope()?;
    let mut global_manager = PluginManager::open_persisted_scoped(&global_anchor, &global_relative)
        .map_err(plugin_manager_error)?;
    let _cli_lock = acquire_cli_plugin_lock(global_manager.root())?;
    let execution = plugin_lifecycle_execution_context();
    // Acquiring either scope lock first performs its config-journal recovery.
    // The pending joint journal identifies which exact scope must then be
    // reconciled; `cwd` remains relevant for project identity validation.
    recover_config_scopes(cwd)?;
    recover_pending_cli_plugin_transaction(
        &global_anchor,
        &global_relative,
        &mut global_manager,
        &execution,
    )?;
    let (project_anchor, project_relative) = project_plugin_store_scope(cwd)?;
    let project_manager = PluginManager::open_existing_scoped(
        &project_anchor,
        &project_relative,
        global_manager.trusted_signers(),
    )
    .map_err(plugin_manager_error)?;
    if let Some(project_manager) = project_manager {
        recover_pending_cli_plugin_transaction_at(
            project_manager.root(),
            &global_anchor,
            &global_relative,
            &mut global_manager,
            &execution,
        )?;
    }
    Ok(())
}

fn joint_journal_present(root: &Path) -> Result<bool, CliError> {
    for name in
        [PLUGIN_CLI_TRANSACTION, PLUGIN_CLI_TRANSACTION_NEXT, PLUGIN_CLI_TRANSACTION_PREVIOUS]
    {
        if entry_present(&root.join(name))? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn acquire_cli_plugin_lock(root: &Path) -> Result<CliPluginLock, CliError> {
    lock_cli_plugin_file(root, true)
}

fn lock_cli_plugin_file(root: &Path, exclusive: bool) -> Result<CliPluginLock, CliError> {
    let mut options = OpenOptions::new();
    options.read(true).write(exclusive).create(exclusive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(root.join(PLUGIN_CLI_LOCK))?;
    let metadata = file.metadata()?;
    #[cfg(unix)]
    let linked = {
        use std::os::unix::fs::MetadataExt as _;
        metadata.nlink() != 1
    };
    #[cfg(windows)]
    let linked = {
        let identity = winapi_util::file::information(&file)?;
        identity.file_attributes() & 0x400 != 0 || identity.number_of_links() != 1
    };
    if !metadata.is_file() || linked {
        return Err(CliError::new(
            ExitClass::Policy,
            "pluginTransactionAuthority",
            "plugin transaction lock identity rejected",
        ));
    }
    let result = if exclusive { file.try_lock() } else { file.try_lock_shared() };
    result.map_err(|_| {
        CliError::new(
            ExitClass::Policy,
            "pluginTransactionConflict",
            "another plugin transaction is active",
        )
    })?;
    Ok(CliPluginLock(file))
}

fn recover_config_scopes(cwd: &Path) -> Result<(), CliError> {
    if config::scope_has_pending_transaction(crate::args::Scope::Global, cwd)? {
        drop(config::lock_scope(crate::args::Scope::Global, cwd)?);
    }
    if config::scope_has_pending_transaction(crate::args::Scope::Project, cwd)? {
        drop(config::lock_scope(crate::args::Scope::Project, cwd)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::*;

    #[test]
    fn diagnostic_queries_do_not_initialize_a_plugin_store() {
        let temporary = tempfile::tempdir().unwrap();
        let user_data = temporary.path().join("user-data");
        for command in ["doctor", "plugins", "capabilities"] {
            let mut stdout = Vec::new();
            run(
                [command, "--json"].into_iter().map(OsString::from).collect(),
                RunContext {
                    user_data_anchor: Some(user_data.clone()),
                    stdout: &mut stdout,
                    stderr: &mut Vec::new(),
                    stdin_is_terminal: true,
                    cwd: temporary.path().to_owned(),
                },
            )
            .unwrap();
            assert!(serde_json::from_slice::<serde_json::Value>(&stdout).is_ok());
            assert!(!user_data.exists(), "{command} created a user data directory");
        }
    }

    #[test]
    fn clean_existing_store_needs_no_writer_lock_for_diagnostics() {
        let temporary = tempfile::tempdir().unwrap();
        let user_data = temporary.path().join("user-data");
        let _data = TestUserDataGuard::set(Some(user_data.clone()));
        let _config = config::TestGlobalConfigGuard::set(Some(user_data.join("config.toml")));
        let (anchor, relative) = global_plugin_store_scope().unwrap();
        let store = anchor.join(relative);
        create_private_user_data_directory(&store).unwrap();
        recover_plugins_before_config_load(temporary.path(), false).unwrap();
        assert_eq!(fs::read_dir(&store).unwrap().count(), 0);

        let writer = acquire_cli_plugin_lock(&store).unwrap();
        let error = recover_plugins_before_config_load(temporary.path(), false).unwrap_err();
        assert_eq!(error.code(), "pluginTransactionConflict");
        drop(writer);
        recover_plugins_before_config_load(temporary.path(), false).unwrap();
        assert_eq!(fs::read_dir(&store).unwrap().count(), 1);
    }
}

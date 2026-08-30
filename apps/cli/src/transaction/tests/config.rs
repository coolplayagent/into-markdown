#[cfg(unix)]
use super::super::config::{
    atomic_replace_config_inner, atomic_replace_config_inner_with_barriers,
};
use super::*;

#[cfg(unix)]
#[test]
fn config_replace_is_fd_relative_durable_and_preserves_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let target = root.join("config.toml");
    fs::write(&target, b"old").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    atomic_replace_config(&target, b"new", true).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"new");
    assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o640);

    let created = root.join("created.toml");
    atomic_replace_config(&created, b"created", false).unwrap();
    assert_eq!(fs::metadata(&created).unwrap().permissions().mode() & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn config_replace_rejects_target_and_temporary_identity_races() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let target = root.join("config.toml");
    fs::write(&target, b"old").unwrap();
    let held = root.join("held.toml");
    let error = atomic_replace_config_inner(&target, b"new", true, |_, _, _| {
        fs::rename(&target, &held)?;
        fs::write(&target, b"racer")?;
        Ok(())
    })
    .unwrap_err();
    assert_eq!(error.code(), "outputIdentityChanged");
    assert_eq!(fs::read(&target).unwrap(), b"racer");

    fs::remove_file(&target).unwrap();
    fs::rename(&held, &target).unwrap();
    let error = atomic_replace_config_inner(&target, b"new", true, |parent, _, temporary_name| {
        let path = parent.path.join(temporary_name);
        fs::remove_file(&path)?;
        fs::write(path, b"attacker temporary")?;
        Ok(())
    })
    .unwrap_err();
    assert_eq!(error.code(), "outputIdentityChanged");
    assert_eq!(fs::read(&target).unwrap(), b"old");
}

#[cfg(unix)]
#[test]
fn config_publish_atomic_primitive_closes_post_check_destination_races() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();

    let absent = root.join("absent.toml");
    let error = atomic_replace_config_inner_with_barriers(
        &absent,
        b"new",
        false,
        |_, _, _| Ok(()),
        |_, _, _| {
            fs::write(&absent, b"racer")?;
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "io");
    assert_eq!(fs::read(&absent).unwrap(), b"racer");

    let target = root.join("existing.toml");
    let held = root.join("original-held.toml");
    fs::write(&target, b"old").unwrap();
    let error = atomic_replace_config_inner_with_barriers(
        &target,
        b"new",
        true,
        |_, _, _| Ok(()),
        |_, _, _| {
            fs::rename(&target, &held)?;
            fs::write(&target, b"racer")?;
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "outputIdentityChanged");
    assert_eq!(fs::read(&target).unwrap(), b"racer");
    assert_eq!(fs::read(&held).unwrap(), b"old");
}

#[cfg(unix)]
#[test]
fn config_publish_reauthenticates_parent_and_temporary_after_final_check() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let parent = root.join("config");
    let held = root.join("config-held");
    fs::create_dir(&parent).unwrap();
    let target = parent.join("settings.toml");
    fs::write(&target, b"old").unwrap();
    let error = atomic_replace_config_inner_with_barriers(
        &target,
        b"new",
        true,
        |_, _, _| Ok(()),
        |_, _, _| {
            fs::rename(&parent, &held)?;
            fs::create_dir(&parent)?;
            fs::write(parent.join("settings.toml"), b"attacker")?;
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "outputIdentityChanged");
    assert_eq!(fs::read(parent.join("settings.toml")).unwrap(), b"attacker");
    assert_eq!(fs::read(held.join("settings.toml")).unwrap(), b"old");

    let target = held.join("settings.toml");
    let attacker_temporary = Arc::new(Mutex::new(None::<PathBuf>));
    let captured = Arc::clone(&attacker_temporary);
    let error = atomic_replace_config_inner_with_barriers(
        &target,
        b"new",
        true,
        |_, _, _| Ok(()),
        move |directory, _, temporary_name| {
            let path = directory.path.join(temporary_name);
            fs::remove_file(&path)?;
            fs::write(&path, b"attacker temporary")?;
            *captured.lock().unwrap() = Some(path);
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "outputIdentityChanged");
    assert_eq!(fs::read(&target).unwrap(), b"old");
    let attacker_temporary = attacker_temporary.lock().unwrap().clone().unwrap();
    assert_eq!(fs::read(attacker_temporary).unwrap(), b"attacker temporary");
}

#[cfg(unix)]
#[test]
fn config_replace_rejects_parent_swap_and_symlink_paths() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let parent = root.join("config");
    let held = root.join("config-held");
    fs::create_dir(&parent).unwrap();
    let target = parent.join("settings.toml");
    fs::write(&target, b"old").unwrap();
    let error = atomic_replace_config_inner(&target, b"new", true, |_, _, _| {
        fs::rename(&parent, &held)?;
        fs::create_dir(&parent)?;
        fs::write(parent.join("settings.toml"), b"attacker")?;
        Ok(())
    })
    .unwrap_err();
    assert_eq!(error.code(), "outputIdentityChanged");
    assert_eq!(fs::read(parent.join("settings.toml")).unwrap(), b"attacker");

    let destination = root.join("destination.toml");
    fs::write(&destination, b"keep").unwrap();
    let link = root.join("link.toml");
    symlink(&destination, &link).unwrap();
    assert!(atomic_replace_config(&link, b"new", true).is_err());
    assert_eq!(fs::read(destination).unwrap(), b"keep");

    let real_parent = root.join("real-parent");
    fs::create_dir(&real_parent).unwrap();
    let linked_parent = root.join("linked-parent");
    symlink(&real_parent, &linked_parent).unwrap();
    assert!(atomic_replace_config(&linked_parent.join("new.toml"), b"new", false).is_err());
    assert!(!real_parent.join("new.toml").exists());
}

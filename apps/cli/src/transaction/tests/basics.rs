use super::*;

#[test]
fn file_backed_primary_and_byte_companion_commit_as_one_set() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let source_path = root.join("encoded.tmp");
    fs::write(&source_path, b"streamed primary").unwrap();
    let source = File::open(&source_path).unwrap();
    let primary = root.join("document.md");
    let asset = root.join("asset.bin");
    prepare_file_and_bytes(
        &FileTarget { path: primary.clone(), file: &source },
        &[Target { path: asset.clone(), bytes: b"asset" }],
        false,
        &context(),
    )
    .unwrap()
    .commit()
    .unwrap();

    assert_eq!(fs::read(primary).unwrap(), b"streamed primary");
    assert_eq!(fs::read(asset).unwrap(), b"asset");
    assert!(!root.join(REGISTRY_NAME).exists());
}

#[test]
fn concurrent_parent_creation_authenticates_the_winning_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let target = temporary.path().canonicalize().unwrap().join("a/b/c");
    let barrier = Arc::new(Barrier::new(16));
    let identities = std::thread::scope(|scope| {
        let handles = (0..16)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let target = target.clone();
                scope.spawn(move || {
                    barrier.wait();
                    SafeDir::open_or_create_absolute(&target).unwrap().identity
                })
            })
            .collect::<Vec<_>>();
        handles.into_iter().map(|handle| handle.join().unwrap()).collect::<Vec<_>>()
    });
    assert!(identities.windows(2).all(|pair| pair[0] == pair[1]));
}

#[cfg(unix)]
#[test]
fn output_transaction_resolves_an_existing_symlinked_parent_once() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let physical = root.join("physical");
    fs::create_dir(&physical).unwrap();
    let alias = root.join("alias");
    symlink(&physical, &alias).unwrap();
    let requested = alias.join("new/nested/document.md");
    let target = [Target { path: requested, bytes: b"converted" }];

    prepare(&target, false, &context()).unwrap().commit().unwrap();

    assert_eq!(fs::read(physical.join("new/nested/document.md")).unwrap(), b"converted");
    assert!(manager_directories(&physical).is_empty());
}

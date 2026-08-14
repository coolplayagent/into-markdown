//! Cargo.lock-backed release component authority.

use crate::schema::Component;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Deserialize)]
struct CargoLock {
    package: Vec<LockedPackage>,
}

#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
}

pub(crate) fn load(lock: &str, approvals: &str, errors: &mut Vec<String>) -> Vec<Component> {
    let lock: CargoLock = match toml::from_str(lock) {
        Ok(lock) => lock,
        Err(error) => {
            errors.push(format!("invalid Cargo.lock release authority: {error}"));
            return Vec::new();
        }
    };
    let mut approved = BTreeMap::new();
    for (line_number, line) in approvals.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 3 {
            errors.push(format!("invalid Rust approval line {}", line_number + 1));
            continue;
        }
        let key = (fields[0].to_owned(), fields[1].to_owned());
        if approved.insert(key.clone(), fields[2].to_owned()).is_some() {
            errors.push(format!("duplicate Rust release authority {}@{}", key.0, key.1));
        }
    }
    let mut locked = BTreeSet::new();
    let mut components = Vec::new();
    for package in lock.package {
        if package.source.as_deref()
            != Some("registry+https://github.com/rust-lang/crates.io-index")
        {
            continue;
        }
        let key = (package.name, package.version);
        locked.insert(key.clone());
        let Some(license) = approved.get(&key) else {
            errors.push(format!("unreviewed Cargo release component {}@{}", key.0, key.1));
            continue;
        };
        if package.checksum.as_deref().is_none_or(|hash| {
            hash.len() != 64
                || !hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            errors.push(format!("Cargo release component {}@{} lacks SHA-256", key.0, key.1));
        }
        components.push(Component {
            id: format!("cargo:{}@{}", key.0, key.1),
            kind: "rust-library".to_owned(),
            status: "reviewed".to_owned(),
            _included_in_release: false,
            manual_only: false,
            version: Some(key.1.clone()),
            source: Some(format!("https://crates.io/crates/{}/{}", key.0, key.1)),
            license: Some(license.clone()),
            obligations: Some(
                "Preserve the concluded upstream license and notices; Cargo.lock fixes the exact crates.io checksum."
                    .to_owned(),
            ),
        });
    }
    for stale in approved.keys().filter(|key| !locked.contains(*key)) {
        errors.push(format!("stale Rust release authority {}@{}", stale.0, stale.1));
    }
    components
}

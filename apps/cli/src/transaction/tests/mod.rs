use super::*;
use into_markdown::{ExecutionOptions, ResourceLimits};
use std::sync::{Arc, Barrier};

fn context() -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
}

fn manager_directories(root: &Path) -> Vec<PathBuf> {
    let registry = root.join(REGISTRY_NAME);
    let Ok(entries) = fs::read_dir(registry) else { return Vec::new() };
    entries
        .filter_map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name();
            managed_nonce(&name).map(|_| entry.path())
        })
        .collect()
}

fn manager_artifacts(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else { return Vec::new() };
    entries
        .filter_map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let text = name.to_string_lossy();
            (text == REGISTRY_NAME || text == PARENT_LEASE_NAME).then(|| entry.path())
        })
        .collect()
}

mod basics;
mod budget;
mod concurrency;
mod config;
mod process_helper;
mod recovery;
mod scale;

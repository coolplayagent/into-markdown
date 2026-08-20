//! Independent installed catalog authority and CLI comparison.

use crate::process::{CommandSpec, Executor, prepare_home};
use crate::request::ValidatedRequest;
use into_markdown_converters::{CoreCatalogAuthority, CoreCatalogAuthorityEntry};
use license_check::schema::ArchiveProjection;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CliFormat {
    format: String,
    family: String,
    status: String,
    source: String,
    extensions: Vec<String>,
    #[serde(default)]
    runtime_component: Option<String>,
    #[serde(default)]
    install_hint: Option<String>,
}

pub(crate) fn load_authority(
    request: &ValidatedRequest,
    projection: &ArchiveProjection,
) -> Result<CoreCatalogAuthority, String> {
    let path = request.install_root.join("core-catalog.json");
    let entry = projection
        .files
        .iter()
        .find(|file| file.path == "core-catalog.json")
        .ok_or_else(|| "catalog authority is not in archive manifest".to_owned())?;
    let bytes =
        fs::read(&path).map_err(|error| format!("cannot read catalog authority: {error}"))?;
    if bytes.len() as u64 != entry.bytes || format!("{:x}", Sha256::digest(&bytes)) != entry.sha256
    {
        return Err("catalog authority differs from archive manifest".into());
    }
    let authority: CoreCatalogAuthority = serde_json::from_slice(&bytes)
        .map_err(|error| format!("catalog authority is invalid: {error}"))?;
    if authority.schema_version != 1
        || format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&authority.entries).map_err(|error| error.to_string())?
            )
        ) != authority.entries_sha256
        || format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&authority.optional_runtimes)
                    .map_err(|error| error.to_string())?
            )
        ) != authority.optional_runtimes_sha256
    {
        return Err("catalog authority entry hash is invalid".into());
    }
    if authority.entries.is_empty() {
        return Err("catalog authority is empty".into());
    }
    validate_compiled_authority(&authority)?;
    Ok(authority)
}

fn validate_compiled_authority(authority: &CoreCatalogAuthority) -> Result<(), String> {
    let trusted = into_markdown_converters::core_catalog_authority()
        .map_err(|error| format!("cannot construct trusted core catalog: {error}"))?;
    if *authority != trusted {
        return Err("packaged catalog differs from the compiled core catalog authority".into());
    }
    Ok(())
}

pub(crate) fn compare_cli(
    request: &ValidatedRequest,
    root: &Path,
    authority: &CoreCatalogAuthority,
    executor: &dyn Executor,
) -> Result<(), String> {
    let home = prepare_home(root, "catalog-home")?;
    let args = vec!["formats".into(), "--json".into(), "--no-config".into()];
    let output = executor.execute(CommandSpec {
        program: &request.into_md,
        arguments: &args,
        current_dir: root,
        home: &home,
        environment: ValidatedRequest::cli_environment(&home),
        stdin: &[],
        timeout: request.timeout,
        cancel_file: request.cancel_file.as_deref(),
    })?;
    if output.exit_code != Some(0) || !output.stderr.is_empty() {
        return Err("formats command did not complete cleanly".into());
    }
    let reported: Vec<CliFormat> = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("formats output is invalid: {error}"))?;
    let reported = reported
        .into_iter()
        .map(|entry| CoreCatalogAuthorityEntry {
            format: entry.format,
            family: entry.family,
            extensions: entry.extensions,
            status: entry.status,
            source: entry.source,
            runtime_component: entry.runtime_component,
            install_hint: entry.install_hint,
        })
        .collect::<Vec<_>>();
    if reported != authority.entries {
        return Err("installed CLI formats differ from release catalog authority".into());
    }
    if reported.iter().any(|entry| entry.status != "available" || entry.source != "core") {
        return Err("installed CLI catalog contains non-production core entries".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::CommandOutput;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    struct FakeExecutor(Mutex<Option<CommandOutput>>);

    impl Executor for FakeExecutor {
        fn execute(&self, _: CommandSpec<'_>) -> Result<CommandOutput, String> {
            self.0.lock().unwrap().take().ok_or_else(|| "unexpected command".into())
        }
    }

    #[test]
    fn self_reported_catalog_cannot_override_release_authority() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("into-md");
        fs::write(&executable, b"placeholder").unwrap();
        let request = ValidatedRequest {
            install_root: PathBuf::new(),
            into_md: executable.clone(),
            rust_library: temporary.path().to_owned(),
            manifest: executable.clone(),
            fixtures: temporary.path().to_owned(),
            audio_fixture: executable.clone(),
            temp_root: temporary.path().to_owned(),
            report: temporary.path().join("report"),
            archive_sha256: "a".repeat(64),
            cargo: executable.clone(),
            rustc: executable,
            timeout: Duration::from_secs(1),
            cancel_file: None,
        };
        let authority = CoreCatalogAuthority {
            schema_version: 1,
            entries_sha256: "a".repeat(64),
            entries: vec![CoreCatalogAuthorityEntry {
                format: "text".into(),
                family: "text".into(),
                extensions: vec!["txt".into()],
                status: "available".into(),
                source: "core".into(),
                runtime_component: None,
                install_hint: None,
            }],
            optional_runtimes_sha256: "b".repeat(64),
            optional_runtimes: vec![],
        };
        let executor = FakeExecutor(Mutex::new(Some(CommandOutput {
            exit_code: Some(0),
            stdout: br#"[{"format":"text","family":"text","extensions":["txt"],"status":"planned","source":"core"}]"#.to_vec(),
            stderr: vec![],
        })));
        let error = compare_cli(&request, temporary.path(), &authority, &executor).unwrap_err();
        assert!(error.contains("differ from release catalog authority"));
    }

    #[test]
    fn jointly_rehashed_catalog_and_cli_cannot_forge_compiled_authority() {
        let mut forged = into_markdown_converters::core_catalog_authority().unwrap();
        forged.entries[0].family = "forged".into();
        forged.entries_sha256 =
            format!("{:x}", Sha256::digest(serde_json::to_vec(&forged.entries).unwrap()));
        assert_eq!(
            validate_compiled_authority(&forged).unwrap_err(),
            "packaged catalog differs from the compiled core catalog authority"
        );
    }
}

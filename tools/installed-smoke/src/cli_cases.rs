//! Installed CLI cases shared by every platform package.

use crate::catalog;
use crate::process::{CommandOutput, CommandSpec, Executor, prepare_home};
use crate::report::{CapabilityResult, CaseResult};
use crate::request::ValidatedRequest;
use into_markdown_converters::CoreCatalogAuthority;
use license_check::schema::ArchiveProjection;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const GOLDENS: [(&str, &str, &str); 11] = [
    (
        "text-file",
        "text/normal.txt",
        "7409dc576cbe54a382d7253c5019f173812fb18391f200500af467444d084c55",
    ),
    (
        "docx",
        "docx/normal.docx",
        "75e91c063ae865bd2ae46f084407dee1df4a4e0bf3995d7ce9495814db0e3006",
    ),
    (
        "epub",
        "epub/normal.epub",
        "5ab7b571b4e58865126f51f61f4d4df2bedfcd9b2b835212d4352f2910a842f6",
    ),
    (
        "outlook-msg",
        "msg/normal.msg",
        "3a07cae4cdf5e934f63c3d47d1f42f6a1fc5a14f0b20b46cbbd9003f45db67db",
    ),
    ("rtf", "rtf/normal.rtf", "8629ab8e7dbc3b820d277c15397f9f4fd0d3b660ca3e0902f0960dda8342c889"),
    (
        "pptx",
        "pptx/normal.pptx",
        "81f5089a96e2875cfcd2dd254d06b211801124b8525a4491292ee853fc7a42f9",
    ),
    (
        "xlsx",
        "xlsx/normal.xlsx",
        "d3872577eb79d4d2812e4a7b1c14f64e3241ba729aa4105be0d2d47c7ffe32a8",
    ),
    (
        "xlsb",
        "xlsb/normal.xlsb",
        "4f0bb1503bdce8a8571ee61b7f034a94425963876d5bf95d49f709709fc9f1ee",
    ),
    ("odt", "odt/normal.odt", "9599eaa0b53968a54eda9915a7e8e59cf1bc8444786f29ae9fb7dfc3600c3b69"),
    ("ods", "ods/normal.ods", "34087e4a175f614e604a67f9755063363d4dda096fccf68f9bd92b7589b93864"),
    ("odp", "odp/normal.odp", "5276964b0d08d9658d89cc450ec2f60dc4689685bdb21d768528d46a2d07cb78"),
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DoctorEntry {
    id: String,
    status: String,
    detail: String,
}

pub(crate) fn run(
    request: &ValidatedRequest,
    root: &Path,
    authority: &CoreCatalogAuthority,
    projection: &ArchiveProjection,
    executor: &dyn Executor,
    cases: &mut Vec<CaseResult>,
    capabilities: &mut Vec<CapabilityResult>,
) -> Result<(), String> {
    record(cases, "version", version(request, root, executor));
    record(cases, "formats-authority", catalog::compare_cli(request, root, authority, executor));
    let doctor = doctor(request, root, executor)?;
    capabilities.extend(runtime_capabilities(authority, &doctor)?);

    for (id, fixture, golden) in GOLDENS {
        record(cases, id, markdown_fixture(request, root, fixture, golden, executor));
    }
    record(cases, "text-stdin-result-json", stdin_dto(request, root, executor));
    record(cases, "corrupt-docx", corrupt(request, root, executor));
    record(cases, "zip", zip_fixture(request, root, executor));
    record(
        cases,
        "pdf-runtime",
        optional_conversion(
            request,
            root,
            authority,
            projection,
            &doctor,
            "pdf",
            "pdfium",
            FormatRuntimeBinding::Required,
            "pdf/structures.pdf",
            &[],
            executor,
        ),
    );
    record(
        cases,
        "image-ocr-runtime",
        optional_conversion(
            request,
            root,
            authority,
            projection,
            &doctor,
            "image",
            "official.ocr.ppocrv6",
            FormatRuntimeBinding::ModeOptional,
            "ocr/ocr-english-clear-1.png",
            &["--ocr", "always"],
            executor,
        ),
    );
    record(
        cases,
        "audio-transcription-runtime",
        media_conversion(request, root, authority, projection, &doctor, executor),
    );
    for (id, fixture, golden) in [
        (
            "legacy-doc-core",
            "legacy/normal.doc",
            "75e91c063ae865bd2ae46f084407dee1df4a4e0bf3995d7ce9495814db0e3006",
        ),
        (
            "legacy-ppt-core",
            "legacy/normal.ppt",
            "6398b674f4d8e5968892fb24e953acd63627e4adbd0b2acaa340d193cef48fc4",
        ),
        (
            "legacy-xls-core",
            "legacy/normal.xls",
            "4c3e870f2da63ebb4c94addc2600824f5e4285ab9bdfee86172f189002ae3104",
        ),
    ] {
        record(cases, id, legacy_conversion(request, root, fixture, golden, executor));
    }
    if request.cancelled() {
        return Err("smoke run cancelled".into());
    }
    Ok(())
}

fn media_conversion(
    request: &ValidatedRequest,
    root: &Path,
    authority: &CoreCatalogAuthority,
    projection: &ArchiveProjection,
    doctor: &BTreeMap<String, DoctorEntry>,
    executor: &dyn Executor,
) -> Result<(), String> {
    let entry = authority
        .entries
        .iter()
        .find(|entry| entry.format == "audio")
        .ok_or_else(|| "audio format is absent from catalog authority".to_owned())?;
    let runtime = authority
        .optional_runtimes
        .iter()
        .find(|entry| entry.component == "official.media.whisper")
        .ok_or_else(|| "ASR runtime is absent from catalog authority".to_owned())?;
    if entry.runtime_component.as_deref() != Some(runtime.component.as_str())
        || entry.install_hint.as_deref() != Some(runtime.install_hint.as_str())
    {
        return Err("audio format and ASR runtime authority disagree".into());
    }
    let path = &request.audio_fixture;
    let path_string = path.display().to_string();
    let available = doctor.get("runtime.asr").is_some_and(|entry| entry.status == "ok");
    let projected = runtime_is_projected(projection, "official.media.whisper")?;
    if projected && !available {
        return Err("projected ASR runtime is unavailable after installation".into());
    }
    if available {
        for (extra, expect_diarization) in [
            (vec!["--ai", "audio-transcription=only"], false),
            (vec!["--diarize", "--expected-speakers", "1"], true),
        ] {
            let mut arguments =
                vec![path_string.as_str(), "--emit", "result-json", "--log-format", "json"];
            arguments.extend(extra);
            let output = cli(request, root, &arguments, &[], executor)?;
            let dto: serde_json::Value = serde_json::from_slice(&output.stdout)
                .map_err(|error| format!("audio result DTO is invalid: {error}"))?;
            if output.exit_code != Some(0) {
                return Err("available ASR runtime returned a nonzero exit".into());
            }
            if !output.stderr.is_empty() {
                return Err("available ASR runtime polluted JSON stderr".into());
            }
            if dto["schemaVersion"] != 1 {
                return Err("available ASR runtime returned an unsupported DTO".into());
            }
            if dto["document"]["blocks"].as_array().is_none_or(Vec::is_empty) {
                return Err("available ASR runtime produced no transcript blocks".into());
            }
            if dto["document"]["metadata"]["properties"]["media.model"]
                .as_str()
                .is_none_or(str::is_empty)
            {
                return Err("available ASR runtime omitted its model identity".into());
            }
            if expect_diarization
                && dto["document"]["metadata"]["properties"]["media.diarizer"]
                    .as_str()
                    .is_none_or(str::is_empty)
            {
                return Err("available diarization runtime omitted its model identity".into());
            }
        }
        return Ok(());
    }
    let output = cli(
        request,
        root,
        &[path_string.as_str(), "--ai", "audio-transcription=only", "--log-format", "json"],
        &[],
        executor,
    )?;
    let event: serde_json::Value = serde_json::from_slice(&output.stderr)
        .map_err(|_| "missing ASR runtime error is not JSON")?;
    let hint = runtime.install_hint.split('`').nth(1).unwrap_or(&runtime.install_hint);
    if output.exit_code == Some(9)
        && output.stdout.is_empty()
        && event["code"] == "componentUnavailable"
        && event["exitCode"] == 9
        && event["message"].as_str().is_some_and(|message| message.contains(hint))
    {
        Ok(())
    } else {
        Err("missing ASR runtime did not return its exact setup contract".into())
    }
}

fn legacy_conversion(
    request: &ValidatedRequest,
    root: &Path,
    fixture: &str,
    golden: &str,
    executor: &dyn Executor,
) -> Result<(), String> {
    let path = fixture_path(request, fixture)?;
    let output = cli(
        request,
        root,
        &[
            &path.display().to_string(),
            "--no-config",
            "--log-format",
            "json",
            "--max-temporary-size",
            "2GiB",
            "--asset-mode",
            "embed",
        ],
        &[],
        executor,
    )?;
    if output.exit_code == Some(0)
        && output.stderr.is_empty()
        && format!("{:x}", Sha256::digest(&output.stdout)) == golden
    {
        if Path::new(fixture)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("ppt"))
        {
            let dto = cli(
                request,
                root,
                &[
                    &path.display().to_string(),
                    "--no-config",
                    "--emit",
                    "result-json",
                    "--asset-mode",
                    "embed",
                ],
                &[],
                executor,
            )?;
            let value: serde_json::Value = serde_json::from_slice(&dto.stdout)
                .map_err(|error| format!("native PPT result DTO is invalid: {error}"))?;
            let assets = value["assets"].as_array().ok_or("native PPT assets are absent")?;
            if dto.exit_code != Some(0)
                || !dto.stderr.is_empty()
                || assets.len() != 1
                || assets[0]["mediaType"] != "image/png"
                || assets[0]["dataBase64"].as_str().is_none_or(str::is_empty)
            {
                return Err("native PPT safe image asset contract failed".into());
            }
        }
        return Ok(());
    }
    Err("native legacy Office output differs from golden".into())
}

fn record(cases: &mut Vec<CaseResult>, id: &str, result: Result<(), String>) {
    cases.push(match result {
        Ok(()) => CaseResult::passed(id, "contract satisfied"),
        Err(error) => CaseResult::failed(id, "contractFailed", &error),
    });
}

fn version(request: &ValidatedRequest, root: &Path, executor: &dyn Executor) -> Result<(), String> {
    let output = cli(request, root, &["--version"], &[], executor)?;
    let value = std::str::from_utf8(&output.stdout).map_err(|_| "version output is not UTF-8")?;
    if output.exit_code == Some(0) && value.starts_with("into-md ") && output.stderr.is_empty() {
        Ok(())
    } else {
        Err("version contract failed".into())
    }
}

fn doctor(
    request: &ValidatedRequest,
    root: &Path,
    executor: &dyn Executor,
) -> Result<BTreeMap<String, DoctorEntry>, String> {
    let output = cli(request, root, &["doctor", "--json"], &[], executor)?;
    if output.exit_code != Some(0) {
        return Err("doctor command failed".into());
    }
    let entries: Vec<DoctorEntry> = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("doctor output is invalid: {error}"))?;
    Ok(entries.into_iter().map(|entry| (entry.id.clone(), entry)).collect())
}

fn runtime_capabilities(
    authority: &CoreCatalogAuthority,
    doctor: &BTreeMap<String, DoctorEntry>,
) -> Result<Vec<CapabilityResult>, String> {
    let mut result = Vec::new();
    for entry in &authority.optional_runtimes {
        let doctor_id = doctor_id(&entry.component)?;
        let status = doctor.get(doctor_id).map_or("missing", |value| value.status.as_str());
        let hint_present = doctor.get(doctor_id).is_some_and(|value| !value.detail.is_empty())
            && !entry.install_hint.is_empty();
        result.push(CapabilityResult {
            id: entry.component.clone(),
            status: status.into(),
            install_hint_present: hint_present,
        });
    }
    Ok(result)
}

fn markdown_fixture(
    request: &ValidatedRequest,
    root: &Path,
    fixture: &str,
    golden: &str,
    executor: &dyn Executor,
) -> Result<(), String> {
    let path = fixture_path(request, fixture)?;
    let path_string = path.display().to_string();
    let output =
        cli(request, root, &[&path_string, "--no-config", "--asset-mode", "embed"], &[], executor)?;
    if output.exit_code != Some(0) || !output.stderr.is_empty() {
        return Err("fixture conversion failed".into());
    }
    if format!("{:x}", Sha256::digest(&output.stdout)) != golden {
        return Err("fixture Markdown differs from golden".into());
    }
    Ok(())
}

fn stdin_dto(
    request: &ValidatedRequest,
    root: &Path,
    executor: &dyn Executor,
) -> Result<(), String> {
    let input = fs::read(fixture_path(request, "text/normal.txt")?)
        .map_err(|error| format!("cannot read text fixture: {error}"))?;
    let output = cli(
        request,
        root,
        &["-", "--format", "text", "--emit", "result-json", "--no-config"],
        &input,
        executor,
    )?;
    let dto: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("result DTO is invalid: {error}"))?;
    if output.exit_code == Some(0)
        && dto["schemaVersion"] == 1
        && dto["markdown"] == "Alpha 中文 line  \nSecond line\n"
        && dto["document"]["blocks"].as_array().is_some_and(|blocks| !blocks.is_empty())
    {
        Ok(())
    } else {
        Err("stdin result DTO differs from golden contract".into())
    }
}

fn corrupt(request: &ValidatedRequest, root: &Path, executor: &dyn Executor) -> Result<(), String> {
    let path = fixture_path(request, "docx/corrupt.docx")?;
    let output = cli(
        request,
        root,
        &[&path.display().to_string(), "--no-config", "--log-format", "json"],
        &[],
        executor,
    )?;
    let event: serde_json::Value =
        serde_json::from_slice(&output.stderr).map_err(|_| "corrupt-input error is not JSON")?;
    if output.exit_code == Some(3)
        && output.stdout.is_empty()
        && event["code"] == "malformed"
        && event["exitCode"] == 3
    {
        Ok(())
    } else {
        Err(format!(
            "corrupt-input exit contract drifted (exit {:?}, stdout {} bytes, code {}, reported exit {})",
            output.exit_code,
            output.stdout.len(),
            event["code"],
            event["exitCode"]
        ))
    }
}

fn zip_fixture(
    request: &ValidatedRequest,
    root: &Path,
    executor: &dyn Executor,
) -> Result<(), String> {
    let path = root.join("normal.zip");
    let file =
        fs::File::create(&path).map_err(|error| format!("cannot create ZIP fixture: {error}"))?;
    let mut archive = zip::ZipWriter::new(file);
    archive
        .start_file("notes/readme.txt", zip::write::SimpleFileOptions::default())
        .map_err(|error| format!("cannot create ZIP fixture: {error}"))?;
    archive
        .write_all(b"Installed ZIP smoke\n")
        .map_err(|error| format!("cannot write ZIP fixture: {error}"))?;
    archive.finish().map_err(|error| format!("cannot finish ZIP fixture: {error}"))?;
    let output = cli(request, root, &[&path.display().to_string(), "--no-config"], &[], executor)?;
    let contains_marker = std::str::from_utf8(&output.stdout)
        .is_ok_and(|value| value.contains("Installed ZIP smoke"));
    if output.exit_code == Some(0) && contains_marker {
        Ok(())
    } else {
        Err(format!(
            "ZIP conversion contract failed (exit {:?}, stdout {} bytes, stderr {} bytes, marker {contains_marker})",
            output.exit_code,
            output.stdout.len(),
            output.stderr.len()
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn optional_conversion(
    request: &ValidatedRequest,
    root: &Path,
    authority: &CoreCatalogAuthority,
    projection: &ArchiveProjection,
    doctor: &BTreeMap<String, DoctorEntry>,
    format: &str,
    runtime_component: &str,
    binding: FormatRuntimeBinding,
    fixture: &str,
    extra: &[&str],
    executor: &dyn Executor,
) -> Result<(), String> {
    let entry = authority
        .entries
        .iter()
        .find(|entry| entry.format == format)
        .ok_or_else(|| "optional format is absent from catalog authority".to_owned())?;
    let runtime = authority
        .optional_runtimes
        .iter()
        .find(|entry| entry.component == runtime_component)
        .ok_or_else(|| "optional runtime is absent from catalog authority".to_owned())?;
    let format_binding_matches = match binding {
        FormatRuntimeBinding::Required => {
            entry.runtime_component.as_deref() == Some(runtime.component.as_str())
                && entry.install_hint.as_deref() == Some(runtime.install_hint.as_str())
        }
        FormatRuntimeBinding::ModeOptional => {
            entry.runtime_component.is_none() && entry.install_hint.is_none()
        }
    };
    if !format_binding_matches {
        return Err("format and runtime authority disagree".into());
    }
    let path = fixture_path(request, fixture)?;
    let mut args = vec![path.display().to_string()];
    if format != "image" {
        args.push("--no-config".into());
    }
    args.extend(["--log-format".into(), "json".into()]);
    if format == "image" {
        args.extend(["--assets-dir".into(), root.join("image-assets").display().to_string()]);
    }
    args.extend(extra.iter().map(|value| (*value).to_owned()));
    let references = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = cli(request, root, &references, &[], executor)?;
    let available =
        doctor.get(doctor_id(runtime_component)?).is_some_and(|entry| entry.status == "ok");
    let projected = runtime_is_projected(projection, runtime_component)?;
    if projected && !available {
        return Err("projected optional runtime is unavailable after installation".into());
    }
    if available {
        if output.exit_code == Some(0) && !output.stdout.is_empty() {
            return Ok(());
        }
        return Err("available optional runtime failed conversion".into());
    }
    let event: serde_json::Value =
        serde_json::from_slice(&output.stderr).map_err(|_| "optional-runtime error is not JSON")?;
    let message = event["message"].as_str().unwrap_or_default();
    let hint = runtime.install_hint.as_str();
    let hint_token = hint.split('`').nth(1).unwrap_or(hint);
    if output.exit_code == Some(9)
        && output.stdout.is_empty()
        && event["code"] == "componentUnavailable"
        && event["exitCode"] == 9
        && !hint_token.is_empty()
        && message.contains(hint_token)
    {
        Ok(())
    } else {
        Err("missing optional runtime did not return its exact code and installation hint".into())
    }
}

fn runtime_is_projected(
    projection: &ArchiveProjection,
    runtime_component: &str,
) -> Result<bool, String> {
    let required: &[&str] = match runtime_component {
        "pdfium" => &["pdfium"],
        "official.ocr.ppocrv6" | "official.media.whisper" => &[],
        _ => return Err("catalog authority contains an unknown optional runtime".into()),
    };
    if required.is_empty() {
        return Ok(false);
    }
    let present = required
        .iter()
        .filter(|component| projection.components.iter().any(|value| value == **component))
        .count();
    if present != 0 && present != required.len() {
        return Err("archive projection contains an incomplete optional runtime".into());
    }
    Ok(present == required.len())
}

#[derive(Clone, Copy)]
enum FormatRuntimeBinding {
    Required,
    ModeOptional,
}

fn doctor_id(component: &str) -> Result<&str, String> {
    match component {
        "official.ocr.ppocrv6" => Ok("runtime.ocr"),
        "pdfium" => Ok("runtime.pdfium"),
        "official.media.whisper" => Ok("runtime.asr"),
        _ => Err("catalog authority contains an unknown optional runtime".into()),
    }
}

fn fixture_path(request: &ValidatedRequest, relative: &str) -> Result<PathBuf, String> {
    let path = request.fixtures.join(relative);
    let path = path.canonicalize().map_err(|_| format!("fixture is missing: {relative}"))?;
    if !path.starts_with(&request.fixtures) || !path.is_file() {
        return Err("fixture escapes installed fixture root".into());
    }
    Ok(path)
}

fn cli(
    request: &ValidatedRequest,
    root: &Path,
    arguments: &[&str],
    stdin: &[u8],
    executor: &dyn Executor,
) -> Result<CommandOutput, String> {
    let home = prepare_home(root, "cli-home")?;
    let arguments = arguments.iter().map(|value| (*value).to_owned()).collect::<Vec<_>>();
    executor.execute(CommandSpec {
        program: &request.into_md,
        arguments: &arguments,
        current_dir: root,
        home: &home,
        environment: ValidatedRequest::cli_environment(&home),
        stdin,
        timeout: request.timeout,
        cancel_file: request.cancel_file.as_deref(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{CommandOutput, CommandSpec};
    use into_markdown_converters::{CoreCatalogAuthorityEntry, CoreRuntimeAuthorityEntry};
    use std::time::Duration;

    struct FakeExecutor {
        output: std::sync::Mutex<Option<CommandOutput>>,
    }

    impl Executor for FakeExecutor {
        fn execute(&self, _: CommandSpec<'_>) -> Result<CommandOutput, String> {
            self.output.lock().unwrap().take().ok_or_else(|| "unexpected command".into())
        }
    }

    #[test]
    fn missing_optional_runtime_requires_exact_code_exit_and_hint() {
        let (temporary, request) = fixture_request("pdf/structures.pdf");
        let authority = pdf_authority();
        let executor = FakeExecutor {
            output: std::sync::Mutex::new(Some(CommandOutput {
                exit_code: Some(9),
                stdout: vec![],
                stderr: br#"{"code":"componentUnavailable","exitCode":9,"message":"install the pinned PDFium runtime"}"#.to_vec(),
            })),
        };
        optional_conversion(
            &request,
            temporary.path(),
            &authority,
            &projection(&[]),
            &BTreeMap::new(),
            "pdf",
            "pdfium",
            FormatRuntimeBinding::Required,
            "pdf/structures.pdf",
            &[],
            &executor,
        )
        .unwrap();
    }

    #[test]
    fn optional_format_requires_exact_runtime_component_and_hint_authority() {
        let (temporary, request) = fixture_request("pdf/structures.pdf");
        let authority = pdf_authority();
        for mutate in [
            |entry: &mut CoreCatalogAuthorityEntry| entry.runtime_component = None,
            |entry: &mut CoreCatalogAuthorityEntry| entry.install_hint = None,
            |entry: &mut CoreCatalogAuthorityEntry| {
                entry.install_hint = Some("forged installation hint".into());
            },
        ] {
            let mut mutated = authority.clone();
            mutate(&mut mutated.entries[0]);
            let executor = FakeExecutor {
                output: std::sync::Mutex::new(Some(CommandOutput {
                    exit_code: Some(0),
                    stdout: vec![],
                    stderr: vec![],
                })),
            };
            let error = optional_conversion(
                &request,
                temporary.path(),
                &mutated,
                &projection(&[]),
                &BTreeMap::new(),
                "pdf",
                "pdfium",
                FormatRuntimeBinding::Required,
                "pdf/structures.pdf",
                &[],
                &executor,
            )
            .unwrap_err();
            assert_eq!(error, "format and runtime authority disagree");
        }
    }

    #[test]
    fn malformed_input_contract_rejects_unstable_exit() {
        let (temporary, request) = fixture_request("docx/corrupt.docx");
        let executor = FakeExecutor {
            output: std::sync::Mutex::new(Some(CommandOutput {
                exit_code: Some(1),
                stdout: vec![],
                stderr: br#"{"code":"malformed","exitCode":1}"#.to_vec(),
            })),
        };
        assert!(corrupt(&request, temporary.path(), &executor).is_err());
    }

    #[test]
    fn projected_runtime_must_be_complete_and_cannot_report_missing() {
        let complete = projection(&["pdfium"]);
        assert!(runtime_is_projected(&complete, "pdfium").unwrap());
        let absent = projection(&[]);
        assert!(!runtime_is_projected(&absent, "pdfium").unwrap());

        assert!(!runtime_is_projected(&projection(&[]), "official.ocr.ppocrv6").unwrap());
        assert!(!runtime_is_projected(&projection(&[]), "official.media.whisper").unwrap());
    }

    fn fixture_request(relative: &str) -> (tempfile::TempDir, ValidatedRequest) {
        let temporary = tempfile::tempdir().unwrap();
        let fixture_root = temporary.path().join("fixtures");
        let fixture = fixture_root.join(relative);
        fs::create_dir_all(fixture.parent().unwrap()).unwrap();
        fs::write(&fixture, b"fixture").unwrap();
        let placeholder = temporary.path().join("placeholder");
        fs::write(&placeholder, b"binary").unwrap();
        let rust_library = temporary.path().join("rust");
        fs::create_dir(&rust_library).unwrap();
        let request = ValidatedRequest {
            install_root: PathBuf::new(),
            into_md: placeholder.clone(),
            rust_library,
            manifest: placeholder.clone(),
            fixtures: fixture_root.canonicalize().unwrap(),
            audio_fixture: fixture.canonicalize().unwrap(),
            temp_root: PathBuf::new(),
            report: PathBuf::new(),
            archive_sha256: "a".repeat(64),
            cargo: placeholder.clone(),
            rustc: placeholder,
            timeout: Duration::from_secs(1),
            cancel_file: None,
        };
        (temporary, request)
    }

    fn pdf_authority() -> CoreCatalogAuthority {
        CoreCatalogAuthority {
            schema_version: 1,
            entries_sha256: "a".repeat(64),
            entries: vec![CoreCatalogAuthorityEntry {
                format: "pdf".into(),
                family: "document".into(),
                extensions: vec!["pdf".into()],
                status: "available".into(),
                source: "core".into(),
                runtime_component: Some("pdfium".into()),
                install_hint: Some("install the pinned PDFium runtime".into()),
            }],
            optional_runtimes_sha256: "b".repeat(64),
            optional_runtimes: vec![CoreRuntimeAuthorityEntry {
                id: "runtime.pdfium".into(),
                component: "pdfium".into(),
                install_hint: "install the pinned PDFium runtime".into(),
            }],
        }
    }

    fn projection(components: &[&str]) -> ArchiveProjection {
        ArchiveProjection {
            schema_version: 1,
            target: "aarch64-apple-darwin".into(),
            version: "0.0.0".into(),
            source_revision: "a".repeat(40),
            components: components.iter().map(|value| (*value).to_owned()).collect(),
            files: vec![],
            license_materials: vec![],
            ffmpeg_evidence: None,
            native_transformations: vec![],
        }
    }
}

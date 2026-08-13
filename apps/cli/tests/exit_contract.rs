//! Real-process exit-status contracts shared by Cargo and Bazel.

use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn binary() -> PathBuf {
    option_env!("CARGO_BIN_EXE_into-md")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("INTO_MD_BIN").map(PathBuf::from))
        .expect("Cargo or Bazel must provide the into-md binary")
}

fn run(arguments: &[&str]) -> (i32, String) {
    let output = Command::new(binary()).args(arguments).output().unwrap();
    (
        output.status.code().expect("CLI must exit normally"),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn run_with_stdin(arguments: &[&str], input: &[u8]) -> std::process::Output {
    let mut child = Command::new(binary())
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn provider_test_requires_double_authorization_and_never_emits_secret() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let canary = "PROVIDER_SECRET_CANARY_7f912e";
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0_u8];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        assert!(
            String::from_utf8_lossy(&request)
                .contains("Authorization: Bearer PROVIDER_SECRET_CANARY_7f912e\r\n")
        );
        let body = br#"{"object":"list","data":[{"id":"model","object":"model","created":0,"owned_by":"test"}],"has_more":false}"#;
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        stream.write_all(body).unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("provider.toml");
    std::fs::write(
        &config,
        format!("schema_version = 1\n[providers.local]\ntype = \"openai-compatible\"\nbase_url = \"http://{address}/v1\"\nmodel = \"model\"\napi_key_env = \"PROVIDER_TEST_KEY\"\ncapabilities = [\"image-description\"]\n"),
    ).unwrap();

    let denied = Command::new(binary())
        .args([
            "--config",
            config.to_str().unwrap(),
            "providers",
            "test",
            "local",
            "--allow-network",
        ])
        .env("PROVIDER_TEST_KEY", canary)
        .output()
        .unwrap();
    assert_eq!(denied.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&denied.stderr).contains("privateNetworkDenied"));
    assert!(!denied.stdout.windows(canary.len()).any(|bytes| bytes == canary.as_bytes()));
    assert!(!denied.stderr.windows(canary.len()).any(|bytes| bytes == canary.as_bytes()));

    let output = Command::new(binary())
        .args([
            "--config",
            config.to_str().unwrap(),
            "providers",
            "--json",
            "test",
            "local",
            "--allow-network",
            "--allow-private-network",
        ])
        .env("PROVIDER_TEST_KEY", canary)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schemaVersion"], 1);
    assert_eq!(value["configuredModelAvailable"], true);
    assert!(!output.stdout.windows(canary.len()).any(|bytes| bytes == canary.as_bytes()));
    assert!(!output.stderr.windows(canary.len()).any(|bytes| bytes == canary.as_bytes()));
    let config_bytes = std::fs::read(config).unwrap();
    assert!(!config_bytes.windows(canary.len()).any(|bytes| bytes == canary.as_bytes()));
    worker.join().unwrap();
}

#[test]
fn real_cli_preserves_stable_policy_component_and_usage_exits() {
    let (exit, stderr) = run(&["--no-config", "https://example.invalid/document"]);
    assert_eq!(exit, 5);
    assert!(stderr.contains("networkDenied"));

    let (exit, stderr) = run(&["--no-config", "models", "install", "missing"]);
    assert_eq!(exit, 9);
    assert!(stderr.contains("componentUnavailable"));

    let (exit, _) = run(&["--definitely-unknown-option"]);
    assert_eq!(exit, 2);
}

#[test]
fn real_cli_converts_txt_files_and_explicit_charset_stdin() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mixed.txt");
    std::fs::write(&path, "中文\r\nEnglish\n\nCafe\u{301} 😀\n").unwrap();
    let output =
        Command::new(binary()).args(["--no-config", path.to_str().unwrap()]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "中文  \nEnglish\n\nCafe\u{301} 😀\n");

    let output = run_with_stdin(&["--no-config", "--charset", "cp1252", "-"], b"caf\xe9\n");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "café\n");
}

#[test]
fn real_cli_converts_notebook_without_executing_active_content() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("safe.ipynb");
    std::fs::write(
        &path,
        br#"{"nbformat":4,"nbformat_minor":5,"metadata":{"language_info":{"name":"python"}},"cells":[{"id":"code","cell_type":"code","metadata":{},"execution_count":1,"source":"NEVER_EXECUTE()","outputs":[{"output_type":"display_data","metadata":{},"data":{"text/html":"<script>NEVER_EXECUTE</script>"}}]}]}"#,
    )
    .unwrap();

    let output =
        Command::new(binary()).args(["--no-config", path.to_str().unwrap()]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let markdown = String::from_utf8(output.stdout).unwrap();
    assert!(markdown.contains("```python\nNEVER_EXECUTE()\n```"));
    assert!(markdown.contains("```html\n<script>NEVER_EXECUTE</script>\n```"));
}

#[test]
fn real_cli_converts_rtf_file_stdin_json_and_bundle_without_active_services() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("safe.rtf");
    let png = "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c6360606060000000050001a5f645400000000049454e44ae426082";
    let rtf = format!(
        "{{\\rtf1\\ansi before{{\\object{{\\*\\objdata 010203}}{{\\result hidden}}}}{{\\pict\\pngblip {png}}}after\\par}}"
    );
    std::fs::write(&path, &rtf).unwrap();

    let markdown = Command::new(binary())
        .args(["--no-config", "--asset-mode", "embed", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(markdown.status.success(), "{}", String::from_utf8_lossy(&markdown.stderr));
    let markdown = String::from_utf8(markdown.stdout).unwrap();
    assert!(markdown.contains("before"));
    assert!(markdown.contains("after"));
    assert!(!markdown.contains("hidden"));
    assert!(markdown.contains("!["));

    let stdin = run_with_stdin(
        &["--no-config", "--format", "rtf", "--asset-mode", "embed", "-"],
        rtf.as_bytes(),
    );
    assert!(stdin.status.success(), "{}", String::from_utf8_lossy(&stdin.stderr));
    assert!(String::from_utf8_lossy(&stdin.stdout).contains("before"));

    let result = Command::new(binary())
        .args([
            "--no-config",
            "--emit",
            "result-json",
            "--asset-mode",
            "embed",
            path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let result: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(result["assets"].as_array().unwrap().len(), 1);
    assert_eq!(result["assets"][0]["mediaType"], "image/png");
    assert!(!result["assets"][0]["dataBase64"].as_str().unwrap().is_empty());

    let bundle = Command::new(binary())
        .args(["--no-config", "--emit", "bundle", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(bundle.status.success(), "{}", String::from_utf8_lossy(&bundle.stderr));
    let archive = zip::ZipArchive::new(std::io::Cursor::new(bundle.stdout)).unwrap();
    assert!(archive.file_names().any(|name| {
        name.starts_with("assets/")
            && std::path::Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
    }));
}

#[test]
fn real_cli_rejects_invalid_notebook_schema_and_aggregate_fields() {
    let missing_id = br#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"cell_type":"raw","metadata":{},"source":"x"}]}"#;
    let output = run_with_stdin(&["--no-config", "--format", "ipynb", "-"], missing_id);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed"));

    let aggregate = br#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"id":"raw","cell_type":"raw","metadata":{},"source":["0123456789","0123456789"]}]}"#;
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("bounded.toml");
    std::fs::write(&config, "schema_version = 1\n[conversion.limits]\nmax_field_bytes = 16\n")
        .unwrap();
    let output = run_with_stdin(
        &["--config", config.to_str().unwrap(), "--format", "ipynb", "-"],
        aggregate,
    );
    assert_eq!(output.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&output.stderr).contains("resourceLimit"));

    let inline_attachment = br#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"id":"markdown","cell_type":"markdown","metadata":{},"source":"prefix ![x](attachment:a)","attachments":{"a":{"image/png":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="}}}]}"#;
    let output = run_with_stdin(&["--no-config", "--format", "ipynb", "-"], inline_attachment);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed"));

    std::fs::write(&config, "schema_version = 1\n[conversion.limits]\nmax_field_bytes = 20\n")
        .unwrap();
    let combined_error = br#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"id":"error","cell_type":"code","metadata":{},"execution_count":null,"source":"x","outputs":[{"output_type":"error","ename":"Error","evalue":"value","traceback":"12345678"}]}]}"#;
    let output = run_with_stdin(
        &["--config", config.to_str().unwrap(), "--format", "ipynb", "-"],
        combined_error,
    );
    assert_eq!(output.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&output.stderr).contains("resourceLimit"));

    // This PNG has valid chunk lengths and CRCs, but its IDAT zlib header is corrupted.
    let corrupt_codec = br#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"id":"image","cell_type":"raw","metadata":{},"source":"x","attachments":{"bad.png":{"image/png":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVQA2mNk+A8AAQUBAWP5UNAAAAAASUVORK5CYII="}}}]}"#;
    let output = run_with_stdin(&["--no-config", "--format", "ipynb", "-"], corrupt_codec);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed"));
}

#[test]
fn real_cli_keeps_external_markdown_images_offline_in_default_extract_and_result_json() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("diagram.md");
    std::fs::write(&path, "![diagram](https://cdn.example.com/diagram.png)\n").unwrap();

    let output =
        Command::new(binary()).args(["--no-config", path.to_str().unwrap()]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "![diagram](<https://cdn.example.com/diagram.png>)\n"
    );

    let output = Command::new(binary())
        .args(["--no-config", "--emit", "result-json", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["assets"][0]["dataBase64"], "");
    assert_eq!(result["assets"][0]["externalUri"], "https://cdn.example.com/diagram.png");
    assert!(!directory.path().join("diagram_assets").exists());

    let bundle = Command::new(binary())
        .args(["--no-config", "--emit", "bundle", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(bundle.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&bundle.stderr).contains("bundleAssetMissingContent"));
    assert!(bundle.stdout.is_empty());
}

#[test]
fn structured_text_is_never_consumed_by_txt_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let mut large_json = "[".repeat(200);
    large_json.push('"');
    large_json.extend(std::iter::repeat_n('x', 1024 * 1024 + 50_000));
    large_json.push('"');
    large_json.push_str(&"]".repeat(200));

    let json_path = directory.path().join("misleading.txt");
    std::fs::write(&json_path, &large_json).unwrap();
    let output =
        Command::new(binary()).args(["--no-config", json_path.to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("# JSON"));

    let output = run_with_stdin(&["--no-config", "-"], large_json.as_bytes());
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("# JSON"));

    let csv_path = directory.path().join("table.txt");
    std::fs::write(&csv_path, b"name,age\nAlice,42\nBob,30\n").unwrap();
    let output =
        Command::new(binary()).args(["--no-config", csv_path.to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("<strong>name</strong>"));

    let output = run_with_stdin(&["--no-config", "-"], b"name\tage\nAlice\t42\nBob\t30\n");
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("<strong>age</strong>"));

    let output =
        run_with_stdin(&["--no-config", "-"], b"Today, we walked home\nTomorrow, we will rest\n");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Today, we walked home  \nTomorrow, we will rest\n"
    );

    let prefix = "{\"value\":\"";
    let suffix = "\"}";
    let mut boundary_junk = String::new();
    boundary_junk.push_str(prefix);
    boundary_junk.extend(std::iter::repeat_n('x', 1024 * 1024 - prefix.len() - suffix.len()));
    boundary_junk.push_str(suffix);
    boundary_junk.push_str(" trailing prose");
    let output = run_with_stdin(&["--no-config", "-"], boundary_junk.as_bytes());
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed"));
}

#[test]
fn json_depth_4096_never_uses_the_process_stack() {
    let mut deepest_allowed = "[".repeat(4096);
    deepest_allowed.push('0');
    deepest_allowed.push_str(&"]".repeat(4096));
    let output = run_with_stdin(
        &["--no-config", "--format", "json", "--max-depth", "4096", "-"],
        deepest_allowed.as_bytes(),
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("# JSON"));

    let mut excessive = "[".repeat(4097);
    excessive.push('0');
    excessive.push_str(&"]".repeat(4097));
    let output = run_with_stdin(
        &["--no-config", "--format", "json", "--max-depth", "4096", "-"],
        excessive.as_bytes(),
    );
    assert_eq!(output.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&output.stderr).contains("resourceLimit"));
}

#[test]
fn full_input_unicode_controls_never_auto_detect_as_text() {
    let directory = tempfile::tempdir().unwrap();
    for (name, suffix) in [("del", b"\x7f".as_slice()), ("c1", b"\xc2\x80".as_slice())] {
        let path = directory.path().join(name);
        let mut contents = vec![b'A'; 70 * 1024];
        contents.extend_from_slice(suffix);
        std::fs::write(&path, contents).unwrap();

        let detected = Command::new(binary())
            .args(["formats", "detect", "--no-config", "--json", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(detected.status.success(), "{}", String::from_utf8_lossy(&detected.stderr));
        let detected: serde_json::Value = serde_json::from_slice(&detected.stdout).unwrap();
        assert!(
            detected["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .all(|candidate| candidate["format"] != "text")
        );

        let converted =
            Command::new(binary()).args(["--no-config", path.to_str().unwrap()]).output().unwrap();
        assert!(!converted.status.success());
    }

    let safe_path = directory.path().join("safe.txt");
    let mut safe = vec![b'A'; 70 * 1024];
    safe.extend_from_slice(" 安全文本\tline\r\n".as_bytes());
    std::fs::write(&safe_path, safe).unwrap();
    let converted =
        Command::new(binary()).args(["--no-config", safe_path.to_str().unwrap()]).output().unwrap();
    assert!(converted.status.success(), "{}", String::from_utf8_lossy(&converted.stderr));
}

#[test]
fn excessive_txt_inlines_exit_as_resource_limit_not_internal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("too-many-lines.txt");
    std::fs::write(&path, "x\n".repeat(500_001)).unwrap();
    let output =
        Command::new(binary()).args(["--no-config", path.to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(5));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("resourceLimit"));
    assert!(!stderr.contains("internal"));
}

//! Real-process exit-status contracts shared by Cargo and Bazel.

use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn manifest_runfile(logical_paths: &[&str]) -> Option<PathBuf> {
    let manifest = std::env::var_os("RUNFILES_MANIFEST_FILE")?;
    let metadata = std::fs::metadata(&manifest).ok()?;
    if metadata.len() > 64 * 1024 * 1024 {
        return None;
    }
    let contents = std::fs::read_to_string(manifest).ok()?;
    contents.lines().find_map(|line| {
        let (logical, physical) = line.split_once(' ')?;
        logical_paths
            .contains(&logical)
            .then(|| PathBuf::from(physical))
            .filter(|path| path.is_file())
    })
}

fn directory_runfile(logical_paths: &[&str]) -> Option<PathBuf> {
    let root = std::env::var_os("RUNFILES_DIR")
        .or_else(|| std::env::var_os("TEST_SRCDIR"))
        .map(PathBuf::from)?;
    logical_paths.iter().map(|logical| root.join(logical)).find(|path| path.is_file())
}

fn runfile(logical_paths: &[&str]) -> Option<PathBuf> {
    manifest_runfile(logical_paths).or_else(|| directory_runfile(logical_paths))
}

fn existing_binary(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then(|| path.canonicalize().ok()).flatten()
}

fn binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_into-md").map(PathBuf::from)
        && let Some(path) = existing_binary(path)
    {
        return path;
    }
    if let Some(path) = std::env::var_os("INTO_MD_BIN").map(PathBuf::from).and_then(existing_binary)
    {
        return path;
    }
    let name = if cfg!(windows) { "into-md.exe" } else { "into-md" };
    runfile(&[&format!("_main/apps/cli/{name}"), &format!("into_markdown/apps/cli/{name}")])
        .and_then(existing_binary)
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

fn run_isolated_ocr_capability_verify() -> std::process::Output {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let user_data = temporary.path().join("user-data");
    let home = temporary.path().join("home");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&home).unwrap();
    #[cfg(windows)]
    into_markdown_process_plugin::create_windows_plugin_store_directory(&user_data).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new().mode(0o700).create(&user_data).unwrap();
    }
    Command::new(binary())
        .args(["--no-config", "capabilities", "verify", "ocr", "--json"])
        .current_dir(project)
        .env("APPDATA", &user_data)
        .env("LOCALAPPDATA", &user_data)
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", home)
        .env("INTO_MARKDOWN_USER_DATA_HOME", user_data)
        .output()
        .unwrap()
}

#[cfg(not(feature = "embedded-runtime"))]
#[test]
fn non_embedded_ocr_verification_retains_external_plugin_semantics() {
    let output = run_isolated_ocr_capability_verify();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown plugin 'official.ocr.ppocrv6'"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "embedded-runtime")]
#[test]
fn embedded_ocr_verification_reports_the_public_core_identity() {
    let output = run_isolated_ocr_capability_verify();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let verification: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(verification["schemaVersion"], 1);
    assert_eq!(verification["capability"], "ocr");
    assert_eq!(verification["source"], "core:ocr");
    assert_eq!(verification["status"], "ready");
    assert_eq!(verification["version"], env!("CARGO_PKG_VERSION"));
    assert!(verification.get("plugin").is_none());
}

#[test]
fn real_cli_relabels_a_meeting_ir_without_running_transcription() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let input = root.join("document-ir.json");
    let output = root.join("meeting.md");
    let range = into_markdown::TimeRange { start_ms: 1_000, end_ms: 2_000 };
    let document = into_markdown::Document {
        blocks: vec![into_markdown::BlockNode {
            id: into_markdown::NodeId("segment-1".into()),
            block: into_markdown::Block::TimedSegment {
                range,
                speaker: Some("speaker-1".into()),
                speaker_confidence: Some(0.9),
                tokens: Vec::new(),
                content: vec![into_markdown::Inline::Text {
                    value: "hello".into(),
                    marks: Vec::new(),
                }],
            },
            provenance: into_markdown::Provenance {
                kind: into_markdown::ProvenanceKind::AiProvider,
                provider: "test/model@sha256:abcd".into(),
                locator: into_markdown::SourceLocator {
                    time: Some(range),
                    ..into_markdown::SourceLocator::default()
                },
                confidence: Some(0.8),
            },
        }],
        ..into_markdown::Document::default()
    };
    std::fs::write(&input, document.to_json().unwrap()).unwrap();
    let result = Command::new(binary())
        .args([
            "--no-config",
            "transcript",
            "relabel",
            input.to_str().unwrap(),
            "--speaker",
            "speaker-1=张三",
            "-o",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let markdown = std::fs::read_to_string(output).unwrap();
    assert_eq!(markdown, "`00:00:01.000 – 00:00:02.000` **张三:** hello\n");
}

#[test]
fn plugin_list_uses_only_isolated_production_user_data() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let user_data = temporary.path().join("user-data");
    let home = temporary.path().join("home");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&home).unwrap();
    #[cfg(windows)]
    into_markdown_process_plugin::create_windows_plugin_store_directory(&user_data).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new().mode(0o700).create(&user_data).unwrap();
    }
    let outside_sentinel = temporary.path().join("outside-sentinel");
    std::fs::write(&outside_sentinel, b"unchanged").unwrap();
    let output = Command::new(binary())
        .args(["--no-config", "plugins", "--json"])
        .current_dir(&project)
        .env("APPDATA", &user_data)
        .env("LOCALAPPDATA", &user_data)
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", &home)
        .env("INTO_MARKDOWN_USER_DATA_HOME", &user_data)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "[]");
    assert_eq!(std::fs::read_dir(project).unwrap().count(), 0);
    assert!(
        !user_data.exists() || std::fs::read_dir(user_data).unwrap().next().is_none(),
        "read-only plugin listing must not create store artifacts"
    );
    assert_eq!(std::fs::read(outside_sentinel).unwrap(), b"unchanged");
}

#[test]
fn image_description_cli_uses_real_provider_only_under_explicit_mode_and_network_policy() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("provider.toml");
    let image = directory.path().join("image.png");
    std::fs::copy(fixture("small/ocr/ocr-english-clear-1.png"), &image).unwrap();
    let image = image.canonicalize().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    std::fs::write(
        &config,
        format!(
            "schema_version = 1\n[providers.local]\ntype = \"openai-compatible\"\n\
             base_url = \"http://{address}/v1\"\nmodel = \"controlled-vision\"\n\
             api_key_env = \"IMAGE_DESCRIPTION_TEST_KEY\"\n\
             capabilities = [\"image-description\"]\n\
             allowed_hosts = [\"127.0.0.1\"]\n\
             allow_private_network = true\n"
        ),
    )
    .unwrap();

    let off = run_image_description(&config, &image, "off", true);
    assert!(off.status.success(), "{}", String::from_utf8_lossy(&off.stderr));
    assert!(!String::from_utf8_lossy(&off.stdout).contains("A controlled CLI image."));
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );

    listener.set_nonblocking(false).unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with(b"POST /v1/responses HTTP/1.1\r\n"));
            let split = request.windows(4).position(|part| part == b"\r\n\r\n").unwrap() + 4;
            let body: serde_json::Value = serde_json::from_slice(&request[split..]).unwrap();
            assert_eq!(body["model"], "controlled-vision");
            assert_eq!(body["max_output_tokens"], 512);
            assert_eq!(
                body["input"][0]["content"][0]["text"],
                "Describe the visible content of this image accurately and concisely. \
                 Do not infer hidden text or metadata."
            );
            assert!(
                body["input"][0]["content"][1]["image_url"]
                    .as_str()
                    .unwrap()
                    .starts_with("data:image/png;base64,")
            );
            let body = image_description_response();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
    });
    for mode in ["fallback", "prefer", "only"] {
        let output = run_image_description(&config, &image, mode, true);
        assert!(
            output.status.success(),
            "mode {mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("A controlled CLI image."),
            "mode {mode} stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("pdf-page"));
    }
    server.join().unwrap();

    let absent = Command::new(binary())
        .args([
            "--no-config",
            image.to_str().unwrap(),
            "--ai",
            "image-description=only",
            "--asset-mode",
            "embed",
            "--emit",
            "ir-json",
        ])
        .output()
        .unwrap();
    assert_eq!(absent.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&absent.stderr).contains("requires --ai-provider"));
}

fn run_image_description(
    config: &std::path::Path,
    image: &std::path::Path,
    mode: &str,
    allow_network: bool,
) -> std::process::Output {
    let mut command = Command::new(binary());
    command.args([
        "--no-config",
        "--config",
        config.to_str().unwrap(),
        image.to_str().unwrap(),
        "--ai",
        &format!("image-description={mode}"),
        "--ai-provider",
        "local",
        "--asset-mode",
        "embed",
        "--emit",
        "ir-json",
    ]);
    if allow_network {
        command.args(["--allow-network", "--allow-private-network", "--allow-host", "127.0.0.1"]);
    }
    command.env("IMAGE_DESCRIPTION_TEST_KEY", "fixed-test-secret").output().unwrap()
}

fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
    }
    let header = std::str::from_utf8(&request).unwrap();
    let content_length = header
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length: ").or_else(|| line.strip_prefix("content-length: "))
        })
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let start = request.len();
    request.resize(start + content_length, 0);
    stream.read_exact(&mut request[start..]).unwrap();
    request
}

fn image_description_response() -> &'static [u8] {
    br#"{"id":"resp_cli_image","object":"response","created_at":1720000000,"status":"completed","completed_at":1720000001,"error":null,"incomplete_details":null,"input":[],"model":"controlled-vision","output":[{"id":"msg_cli_image","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"A controlled CLI image.","annotations":[],"logprobs":[]}]}],"background":false,"instructions":null,"max_output_tokens":512,"metadata":{},"parallel_tool_calls":false,"previous_response_id":null,"reasoning":{},"reasoning_effort":null,"service_tier":"default","store":false,"temperature":1.0,"text":{"format":{"type":"text"}},"tool_choice":"auto","tools":[],"top_p":1.0,"truncation":"disabled","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2},"user":null}"#
}

fn fixture(relative: &str) -> PathBuf {
    if std::env::var_os("TEST_SRCDIR").is_some()
        && let Some(manifest) =
            runfile(&["_main/fixtures/manifest.json", "into_markdown/fixtures/manifest.json"])
    {
        return manifest.parent().expect("fixture root").join(relative);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(relative)
}

#[test]
fn presentationml_extensions_complete_real_cli_conversion() {
    let directory = tempfile::tempdir().unwrap();
    for relative in [
        "small/pptx/normal.pptx",
        "small/pptx/macro.pptm",
        "small/pptx/slideshow.ppsx",
        "small/pptx/macro-slideshow.ppsm",
        "small/pptx/template.potx",
    ] {
        let fixture = fixture(relative);
        let input = directory.path().join(fixture.file_name().unwrap());
        std::fs::copy(fixture, &input).unwrap();
        let output = Command::new(binary())
            .args(["--no-config", "--ocr", "off", input.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(output.status.success(), "{relative}: {}", String::from_utf8_lossy(&output.stderr));
        let markdown = String::from_utf8(output.stdout).unwrap();
        assert!(markdown.starts_with("## Slide 1:"), "{relative}: {markdown}");
        assert!(markdown.contains("Speaker notes"), "{relative}: {markdown}");
    }
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
        format!("schema_version = 1\n[providers.local]\ntype = \"openai-compatible\"\nbase_url = \"http://{address}/v1\"\nmodel = \"model\"\napi_key_env = \"PROVIDER_TEST_KEY\"\ncapabilities = [\"image-description\"]\nallowed_hosts = [\"127.0.0.1\"]\nallow_private_network = true\n"),
    ).unwrap();

    let denied = Command::new(binary())
        .args([
            "--no-config",
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
    assert_eq!(denied.status.code(), Some(5), "{}", String::from_utf8_lossy(&denied.stderr));
    assert!(String::from_utf8_lossy(&denied.stderr).contains("privateNetworkDenied"));
    assert!(!denied.stdout.windows(canary.len()).any(|bytes| bytes == canary.as_bytes()));
    assert!(!denied.stderr.windows(canary.len()).any(|bytes| bytes == canary.as_bytes()));

    let output = Command::new(binary())
        .args([
            "--no-config",
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

    let (exit, stderr) = run(&["--no-config", "capabilities", "show", "missing"]);
    assert_eq!(exit, 2);
    assert!(stderr.contains("unknown capability"));

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
        &["--no-config", "--config", config.to_str().unwrap(), "--format", "ipynb", "-"],
        aggregate,
    );
    assert_eq!(output.status.code(), Some(5), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stderr).contains("resourceLimit"));

    let inline_attachment = br#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"id":"markdown","cell_type":"markdown","metadata":{},"source":"prefix ![x](attachment:a)","attachments":{"a":{"image/png":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="}}}]}"#;
    let output = run_with_stdin(&["--no-config", "--format", "ipynb", "-"], inline_attachment);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed"));

    std::fs::write(&config, "schema_version = 1\n[conversion.limits]\nmax_field_bytes = 20\n")
        .unwrap();
    let combined_error = br#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"id":"error","cell_type":"code","metadata":{},"execution_count":null,"source":"x","outputs":[{"output_type":"error","ename":"Error","evalue":"value","traceback":"12345678"}]}]}"#;
    let output = run_with_stdin(
        &["--no-config", "--config", config.to_str().unwrap(), "--format", "ipynb", "-"],
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
        "![diagram](https://cdn.example.com/diagram.png)\n"
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

    for (name, contents) in [
        ("incomplete-json.txt", b"{ \"a\":".as_slice()),
        ("incomplete-xml.txt", b"<root>text".as_slice()),
    ] {
        let path = directory.path().join(name);
        std::fs::write(&path, contents).unwrap();
        let output =
            Command::new(binary()).args(["--no-config", path.to_str().unwrap()]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(!String::from_utf8_lossy(&output.stdout).starts_with("# JSON"));
        assert!(!String::from_utf8_lossy(&output.stdout).starts_with("# XML"));
    }

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
    assert!(String::from_utf8_lossy(&output.stdout).contains("**name**"));

    let output = run_with_stdin(&["--no-config", "-"], b"name\tage\nAlice\t42\nBob\t30\n");
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("**age**"));

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

#[test]
fn drawio_cli_file_stdin_batch_and_explicit_format_contract() {
    let source = br#"<mxGraphModel><root><mxCell id="a" vertex="1" value="Start"/><mxCell id="b" vertex="1" value="End"/><mxCell id="e" edge="1" source="a" target="b" value="Continue"/></root></mxGraphModel>"#;
    let result = run_with_stdin(
        &["-", "--no-config", "--format", "drawio", "--emit", "result-json"],
        source,
    );
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let dto: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(dto.to_string().contains("drawio"));
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("图.drawio");
    std::fs::write(&file, source).unwrap();
    let output = Command::new(binary()).arg(&file).args(["--no-config"]).output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("## Connections"));
    let xml = Command::new(binary())
        .arg(&file)
        .args(["--no-config", "--format", "xml"])
        .output()
        .unwrap();
    assert!(xml.status.success());
    assert!(String::from_utf8_lossy(&xml.stdout).contains("xml-attribute"));
    let second = dir.path().join("second.xml");
    std::fs::write(&second, source).unwrap();
    let batch_dir = dir.path().join("converted");
    let batch = Command::new(binary())
        .arg(&file)
        .arg(&second)
        .args(["--no-config", "--output-dir"])
        .arg(&batch_dir)
        .output()
        .unwrap();
    assert!(batch.status.success(), "{}", String::from_utf8_lossy(&batch.stderr));
    for name in ["图.md", "second.md"] {
        assert!(std::fs::read_to_string(batch_dir.join(name)).unwrap().contains("## Connections"));
    }
    std::fs::write(&file, b"<ordinary/>").unwrap();
    let bad = Command::new(binary())
        .arg(&file)
        .args(["--no-config", "--log-format", "json"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("malformed"));
}

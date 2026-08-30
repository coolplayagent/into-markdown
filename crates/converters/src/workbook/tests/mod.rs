mod coordinates;
mod extras;
mod formulas;
mod opc;
mod resources;
mod stream_mode;
mod support;
mod xlsb;
mod xlsx;

use self::support::{convert_with_credit, limited_context, rewrite_package, xlsx};
use into_markdown_core::{ConversionError, ConversionOptions};
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn abnormal_declarations_survive_physical_process_limit() {
    const CHILD: &str = "INTO_MARKDOWN_WORKBOOK_LIMIT_CHILD";
    const MARKER: &str = "workbook-preflight-resource-limit";
    if std::env::var_os(CHILD).is_some() {
        install_current_test_process_limits();
        let base = xlsx(
            r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1"/><sheetData/></worksheet>"#,
        );
        let content_types = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/></Types>"#;
        let workbook_rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/></Relationships>"#;
        let abnormal_sst = r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" uniqueCount="4294967295"/>"#;
        let input = rewrite_package(
            &base,
            &[
                ("[Content_Types].xml", content_types.to_owned()),
                ("xl/_rels/workbook.xml.rels", workbook_rels.to_owned()),
            ],
            &[("xl/sharedStrings.xml", abnormal_sst.to_owned())],
        );
        let root = limited_context(64 * 1024 * 1024);
        let outcome = convert_with_credit(&input, &ConversionOptions::default(), &root);
        assert!(matches!(
            outcome,
            Err(ConversionError::ResourceLimit { limit: "max_table_cells", .. })
        ));
        eprintln!("{MARKER}");
        return;
    }

    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "workbook::tests::abnormal_declarations_survive_physical_process_limit",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("bounded workbook child timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert!(status.success(), "bounded child failed: {status:?}: {stderr}");
    assert!(stderr.contains(MARKER), "bounded child omitted stable marker: {stderr}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(status.signal(), None, "bounded child terminated by signal");
    }
}
#[cfg(unix)]
fn install_current_test_process_limits() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    fn lower_soft(resource: Resource, desired: u64) {
        let current = getrlimit(resource);
        let desired = current.maximum.map_or(desired, |maximum| desired.min(maximum));
        setrlimit(resource, Rlimit { current: Some(desired), maximum: current.maximum })
            .unwrap_or_else(|error| panic!("cannot lower {resource:?}: {error}"));
    }
    #[cfg(target_os = "macos")]
    // Darwin maps the shared cache into each process at a very high virtual
    // address, so a conventional sub-GiB RLIMIT_AS is below the test
    // harness's existing mapping and is rejected with EINVAL. This still
    // imposes a finite ceiling without introducing a production worker.
    let address_bytes = 512 * 1024 * 1024 * 1024;
    #[cfg(not(target_os = "macos"))]
    let address_bytes = 384 * 1024 * 1024;
    lower_soft(Resource::As, address_bytes);
    #[cfg(not(target_os = "macos"))]
    lower_soft(Resource::Data, 256 * 1024 * 1024);
    lower_soft(Resource::Cpu, 5);
    lower_soft(Resource::Core, 0);
}

#[cfg(windows)]
fn install_current_test_process_limits() {
    // Windows CI exercises the same deterministic ResourceLimit assertion;
    // the process-level Job Object harness is owned by the Windows runner.
}

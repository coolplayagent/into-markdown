//! Deterministic stage-two packer for the optional single-file native runtimes.

use sha2::{Digest as _, Sha256};
use std::env;
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use zip::write::SimpleFileOptions;

const PDFIUM_ENV: &str = "INTO_MD_EMBEDDED_PDFIUM_ROOT";
const OCR_ENV: &str = "INTO_MD_EMBEDDED_OCR_ROOT";

#[derive(Clone)]
struct InputFile {
    relative: String,
    source: PathBuf,
    bytes: u64,
    sha256: String,
    executable: bool,
}

fn main() {
    println!("cargo:rerun-if-env-changed={PDFIUM_ENV}");
    println!("cargo:rerun-if-env-changed={OCR_ENV}");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let generated = out.join("embedded_runtime_payloads.rs");
    if env::var_os("CARGO_FEATURE_EMBEDDED_RUNTIME").is_none() {
        fs::write(
            generated,
            "pub(super) const EMBEDDED_RUNTIME_ENABLED: bool = false;\n\
             pub(super) static PDFIUM_ARCHIVE: &[u8] = &[];\n\
             pub(super) const PDFIUM_ARCHIVE_SHA256: &str = \"\";\n\
             pub(super) static PDFIUM_FILES: &[EmbeddedFile] = &[];\n\
             pub(super) static OCR_ARCHIVE: &[u8] = &[];\n\
             pub(super) const OCR_ARCHIVE_SHA256: &str = \"\";\n\
             pub(super) static OCR_FILES: &[EmbeddedFile] = &[];\n",
        )
        .expect("write disabled embedded-runtime constants");
        return;
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("Cargo target OS is available");
    let ocr_root = required_root(OCR_ENV);
    let ocr_files = collect_files(&ocr_root);
    validate_ocr(&ocr_files);
    let ocr_zip = out.join("embedded-ocr.zip");
    let ocr_sha = write_archive(&ocr_zip, &ocr_files);
    let (pdfium_archive, pdfium_sha, pdfium_files) = if target_os == "windows" {
        ("&[]".to_owned(), String::new(), String::new())
    } else {
        let pdfium_root = required_root(PDFIUM_ENV);
        let pdfium_files = collect_files(&pdfium_root);
        validate_pdfium(&pdfium_files);
        let pdfium_zip = out.join("embedded-pdfium.zip");
        let pdfium_sha = write_archive(&pdfium_zip, &pdfium_files);
        (
            format!("include_bytes!({:?})", pdfium_zip.display().to_string()),
            pdfium_sha,
            render_files(&pdfium_files),
        )
    };
    let source = format!(
        "pub(super) const EMBEDDED_RUNTIME_ENABLED: bool = true;\n\
         pub(super) static PDFIUM_ARCHIVE: &[u8] = {pdfium_archive};\n\
         pub(super) const PDFIUM_ARCHIVE_SHA256: &str = {pdfium_sha:?};\n\
         pub(super) static PDFIUM_FILES: &[EmbeddedFile] = &[{pdfium_files}];\n\
         pub(super) static OCR_ARCHIVE: &[u8] = include_bytes!({ocr_zip:?});\n\
         pub(super) const OCR_ARCHIVE_SHA256: &str = {ocr_sha:?};\n\
         pub(super) static OCR_FILES: &[EmbeddedFile] = &[{ocr_files}];\n",
        ocr_zip = ocr_zip.display().to_string(),
        ocr_files = render_files(&ocr_files),
    );
    fs::write(generated, source).expect("write embedded-runtime constants");
}

fn required_root(name: &str) -> PathBuf {
    let root = PathBuf::from(env::var_os(name).unwrap_or_else(|| {
        panic!("feature embedded-runtime requires {name} to name an assembled runtime root")
    }));
    let root = root.canonicalize().unwrap_or_else(|error| panic!("{name} is unavailable: {error}"));
    assert!(root.is_dir(), "{name} is not a directory");
    println!("cargo:rerun-if-changed={}", root.display());
    root
}

fn collect_files(root: &Path) -> Vec<InputFile> {
    let mut pending = vec![root.to_owned()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut children = fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("cannot enumerate {}: {error}", directory.display()))
            .map(|entry| entry.expect("runtime directory entry is readable").path())
            .collect::<Vec<_>>();
        children.sort();
        for path in children.into_iter().rev() {
            let metadata = fs::symlink_metadata(&path)
                .unwrap_or_else(|error| panic!("cannot inspect {}: {error}", path.display()));
            assert!(
                !metadata.file_type().is_symlink(),
                "runtime payload contains link: {}",
                path.display()
            );
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            assert!(metadata.is_file(), "runtime payload contains non-file: {}", path.display());
            let relative_path = path.strip_prefix(root).expect("runtime file remains below root");
            assert!(
                relative_path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
                "runtime path is not portable"
            );
            let relative = relative_path.to_string_lossy().replace('\\', "/");
            assert!(!relative.is_empty() && !relative.contains(':'), "runtime path is invalid");
            let mut input = File::open(&path).expect("runtime input opens");
            let mut hasher = Sha256::new();
            let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
            loop {
                let read = input.read(&mut buffer).expect("runtime input reads");
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            let executable = is_executable_path(&relative);
            files.push(InputFile {
                relative,
                source: path,
                bytes: metadata.len(),
                sha256: format!("{:x}", hasher.finalize()),
                executable,
            });
        }
    }
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    assert!(!files.is_empty(), "runtime payload is empty");
    files
}

fn is_executable_path(relative: &str) -> bool {
    relative.starts_with("bin/")
        && !Path::new(relative)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
}

fn validate_pdfium(files: &[InputFile]) {
    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("Cargo target OS is available");
    let expected = if target_os == "macos" {
        "lib/pdfium/libpdfium.dylib"
    } else if target_os == "linux" {
        "lib/pdfium/libpdfium.so"
    } else {
        panic!("embedded PDFium is unsupported on this target")
    };
    assert!(
        files.len() == 1 && files[0].relative == expected,
        "PDFium payload must contain only {expected}"
    );
}

fn validate_ocr(files: &[InputFile]) {
    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("Cargo target OS is available");
    let provider = if target_os == "windows" {
        "bin/into-md-ocr-provider.exe"
    } else {
        "bin/into-md-ocr-provider"
    };
    let worker = if target_os == "windows" {
        "bin/onnxruntime-worker.exe"
    } else {
        "bin/onnxruntime-worker"
    };
    for required in ["provider.json", "official-publisher.json", provider, worker] {
        assert!(
            files.iter().any(|file| file.relative == required),
            "OCR payload is missing {required}"
        );
    }
    assert!(
        files.iter().any(|file| file.relative.starts_with("onnxruntime/")),
        "OCR payload is missing ONNX Runtime"
    );
    assert!(
        files.iter().any(|file| file.relative.starts_with("models/")),
        "OCR payload is missing models"
    );
    assert!(
        files.iter().all(|file| {
            file.relative == "provider.json"
                || file.relative == "official-publisher.json"
                || file.relative.starts_with("bin/")
                || file.relative.starts_with("onnxruntime/")
                || file.relative.starts_with("models/")
        }),
        "OCR payload contains an audit, installer, or unrelated file"
    );
}

fn write_archive(path: &Path, files: &[InputFile]) -> String {
    let output = File::create(path).expect("create embedded runtime archive");
    let mut archive = zip::ZipWriter::new(output);
    let timestamp = zip::DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0)
        .expect("fixed ZIP timestamp is valid");
    for file in files {
        let mode = if file.executable { 0o700 } else { 0o600 };
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(9))
            .last_modified_time(timestamp)
            .unix_permissions(mode);
        archive.start_file(&file.relative, options).expect("start runtime ZIP member");
        let mut input = File::open(&file.source).expect("open runtime payload file");
        std::io::copy(&mut input, &mut archive).expect("compress runtime payload file");
    }
    archive.finish().expect("finish embedded runtime archive");
    let bytes = fs::read(path).expect("read completed runtime archive");
    format!("{:x}", Sha256::digest(bytes))
}

fn render_files(files: &[InputFile]) -> String {
    files
        .iter()
        .map(|file| {
            format!(
                "EmbeddedFile {{ path: {:?}, bytes: {}, sha256: {:?}, executable: {} }}",
                file.relative, file.bytes, file.sha256, file.executable
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

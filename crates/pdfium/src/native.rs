use crate::{Backend, Error, ImageBitmap, Limits, PixelFormat};
use libloading::Library;
use object::{Architecture, BinaryFormat, NameOrOrdinal, Object as _};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, c_char, c_int, c_ulong, c_void};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const MANIFEST: &str = include_str!("../../../third_party/pdfium/manifest.json");
const CONSUMED_EXPORTS: &[&str] = &[
    "FPDF_InitLibraryWithConfig",
    "FPDF_DestroyLibrary",
    "FPDF_LoadMemDocument64",
    "FPDF_CloseDocument",
    "FPDF_GetPageCount",
    "FPDF_LoadPage",
    "FPDF_ClosePage",
    "FPDFText_LoadPage",
    "FPDFText_ClosePage",
    "FPDFText_CountChars",
    "FPDFText_GetText",
    "FPDFPage_CountObjects",
    "FPDFPage_GetObject",
    "FPDFPageObj_GetType",
    "FPDFBitmap_CreateEx",
    "FPDFBitmap_Destroy",
    "FPDFBitmap_GetBuffer",
    "FPDFBitmap_GetFormat",
    "FPDFBitmap_GetHeight",
    "FPDFBitmap_GetStride",
    "FPDFBitmap_GetWidth",
    "FPDFImageObj_GetBitmap",
    "FPDF_RenderPageBitmap",
    "FPDF_GetLastError",
];
const MAX_RUNTIME_LIBRARY_BYTES: u64 = 16 * 1024 * 1024;
static RUNTIME_ACTIVE: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeManifest {
    schema_version: u32,
    version: String,
    chromium_build: u32,
    source: String,
    release_download_base: String,
    upstream_source: String,
    license: String,
    distribution_license_note: String,
    required_exports: Vec<String>,
    targets: BTreeMap<String, ManifestTarget>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestTarget {
    asset: String,
    archive_size: u64,
    archive_sha256: String,
    library: String,
    library_size: u64,
    library_sha256: String,
    format_pattern: String,
    allowed_dependencies: Vec<String>,
}

/// Supported runtime target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacArm64,
    LinuxX64,
    LinuxArm64,
    WindowsX64,
}

/// Reviewed artifact identity for one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// Rust target triple.
    pub target: String,
    /// Expected dynamic-library filename.
    pub library: String,
    /// Relative path inside the reviewed archive.
    pub library_path: String,
    /// Exact expected extracted-library size.
    pub library_size: u64,
    /// SHA-256 of the extracted dynamic library (not archive metadata).
    pub library_sha256: String,
    /// SHA-256 of the release archive.
    pub archive_sha256: String,
    /// Exact reviewed archive size.
    pub archive_size: u64,
    /// Complete dynamic dependency allowlist.
    pub allowed_dependencies: Vec<String>,
    /// Exact ABI exports consumed by the boundary.
    pub required_exports: Vec<String>,
}

impl Platform {
    /// Resolve a supported target without silently falling back.
    pub fn from_os_arch(os: &str, arch: &str) -> Result<Self, Error> {
        match (os, arch) {
            ("macos", "aarch64") => Ok(Self::MacArm64),
            ("linux", "x86_64") => Ok(Self::LinuxX64),
            ("linux", "aarch64") => Ok(Self::LinuxArm64),
            ("windows", "x86_64") => Ok(Self::WindowsX64),
            _ => Err(Error::UnsupportedPlatform { os: os.into(), arch: arch.into() }),
        }
    }

    /// Current compilation target.
    pub fn current() -> Result<Self, Error> {
        Self::from_os_arch(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// Pinned binary data for this target.
    pub fn artifact(self) -> Result<Artifact, Error> {
        artifact_from_manifest(MANIFEST, self)
    }
}

fn artifact_from_manifest(text: &str, platform: Platform) -> Result<Artifact, Error> {
    let manifest: RuntimeManifest = serde_json::from_str(text)
        .map_err(|error| Error::BinaryValidation(format!("invalid embedded manifest: {error}")))?;
    validate_manifest(&manifest)?;
    let target = platform.target();
    let item = manifest.targets.get(target).ok_or_else(|| {
        Error::BinaryValidation(format!("embedded manifest lacks target {target}"))
    })?;
    let library = Path::new(&item.library)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::BinaryValidation(format!("invalid library path for {target}")))?;
    Ok(Artifact {
        target: target.into(),
        library: library.into(),
        library_path: item.library.clone(),
        library_size: item.library_size,
        library_sha256: item.library_sha256.clone(),
        archive_sha256: item.archive_sha256.clone(),
        archive_size: item.archive_size,
        allowed_dependencies: item.allowed_dependencies.clone(),
        required_exports: manifest.required_exports,
    })
}

pub(crate) fn version() -> Result<String, Error> {
    let manifest: RuntimeManifest = serde_json::from_str(MANIFEST)
        .map_err(|error| Error::BinaryValidation(format!("invalid embedded manifest: {error}")))?;
    validate_manifest(&manifest)?;
    Ok(manifest.version)
}

impl Platform {
    const fn target(self) -> &'static str {
        match self {
            Self::MacArm64 => "aarch64-apple-darwin",
            Self::LinuxX64 => "x86_64-unknown-linux-gnu",
            Self::LinuxArm64 => "aarch64-unknown-linux-gnu",
            Self::WindowsX64 => "x86_64-pc-windows-msvc",
        }
    }
}

fn validate_manifest(manifest: &RuntimeManifest) -> Result<(), Error> {
    let expected_targets = BTreeSet::from([
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    ]);
    if manifest.schema_version != 1
        || manifest.version != "153.0.7999.0"
        || manifest.chromium_build != 7999
        || manifest.source
            != "https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium%2F7999"
        || manifest.release_download_base
            != "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/7999"
        || manifest.upstream_source
            != "https://pdfium.googlesource.com/pdfium/+/refs/heads/chromium/7999"
        || manifest.license != "BSD-3-Clause"
        || manifest.distribution_license_note.trim().is_empty()
        || manifest.targets.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected_targets
    {
        return Err(Error::BinaryValidation("embedded manifest authority is invalid".into()));
    }
    let exports = manifest.required_exports.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if manifest.required_exports.len() != CONSUMED_EXPORTS.len()
        || exports != CONSUMED_EXPORTS.iter().copied().collect()
    {
        return Err(Error::BinaryValidation(
            "embedded manifest ABI exports do not exactly match the consumed FFI".into(),
        ));
    }
    for (target, item) in &manifest.targets {
        if item.archive_size == 0
            || item.library_size == 0
            || item.library_size > MAX_RUNTIME_LIBRARY_BYTES
            || !sha256(&item.archive_sha256)
            || !sha256(&item.library_sha256)
            || item.format_pattern.is_empty()
            || item.allowed_dependencies.is_empty()
            || item.allowed_dependencies.iter().collect::<BTreeSet<_>>().len()
                != item.allowed_dependencies.len()
            || !matches!(
                Path::new(&item.asset).components().collect::<Vec<_>>().as_slice(),
                [std::path::Component::Normal(_)]
            )
            || Path::new(&item.library).is_absolute()
            || Path::new(&item.library)
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(Error::BinaryValidation(format!(
                "embedded manifest target {target} has invalid fields"
            )));
        }
    }
    Ok(())
}

fn sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

type Handle = *mut c_void;
type Init = unsafe extern "C" fn(*const Config);
type Destroy = unsafe extern "C" fn();
type LoadDocument = unsafe extern "C" fn(*const c_void, usize, *const c_char) -> Handle;
type Close = unsafe extern "C" fn(Handle);
type Count = unsafe extern "C" fn(Handle) -> c_int;
type LoadPage = unsafe extern "C" fn(Handle, c_int) -> Handle;
type GetText = unsafe extern "C" fn(Handle, c_int, c_int, *mut u16) -> c_int;
type GetObject = unsafe extern "C" fn(Handle, c_int) -> Handle;
type ObjectType = unsafe extern "C" fn(Handle) -> c_int;
type CreateBitmap = unsafe extern "C" fn(c_int, c_int, c_int, *mut c_void, c_int) -> Handle;
type Render = unsafe extern "C" fn(Handle, Handle, c_int, c_int, c_int, c_int, c_int, c_int);
type LastError = unsafe extern "C" fn() -> c_ulong;
type GetBitmap = unsafe extern "C" fn(Handle) -> Handle;
type GetBitmapInt = unsafe extern "C" fn(Handle) -> c_int;
type GetBitmapBuffer = unsafe extern "C" fn(Handle) -> *mut c_void;

// Chromium/7999's public/fpdfview.h says this release currently requires version 2. Version 2
// ends at `m_v8EmbedderSlot`; advertising a later version with this four-field prefix would let
// PDFium read beyond the Rust object.
const CONFIG_VERSION: c_int = 2;

#[repr(C)]
struct Config {
    version: c_int,
    user_font_paths: *const *const c_char,
    isolate: *mut c_void,
    slot: u32,
}

impl Config {
    const fn version_2() -> Self {
        Self {
            version: CONFIG_VERSION,
            user_font_paths: std::ptr::null(),
            isolate: std::ptr::null_mut(),
            slot: 0,
        }
    }
}

pub(crate) struct Native {
    _library: Library,
    _snapshot: Snapshot,
    destroy: Destroy,
    load_document: LoadDocument,
    close_document: Close,
    page_count: Count,
    load_page: LoadPage,
    close_page: Close,
    load_text: unsafe extern "C" fn(Handle) -> Handle,
    close_text: Close,
    text_count: Count,
    get_text: GetText,
    object_count: Count,
    get_object: GetObject,
    object_type: ObjectType,
    create_bitmap: CreateBitmap,
    destroy_bitmap: Close,
    render_page: Render,
    last_error: LastError,
    image_bitmap: GetBitmap,
    bitmap_width: GetBitmapInt,
    bitmap_height: GetBitmapInt,
    bitmap_stride: GetBitmapInt,
    bitmap_format: GetBitmapInt,
    bitmap_buffer: GetBitmapBuffer,
}

// PDFium calls are serialized by `Inner::gate`; handles never cross the safe lifetime boundary.
unsafe impl Send for Native {}
unsafe impl Sync for Native {}

impl Native {
    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        let platform = Platform::current()?;
        let artifact = platform.artifact()?;
        let mut snapshot = validated_snapshot(path, &artifact)?;
        validate_binary(snapshot.bytes(), platform, &artifact)?;
        if RUNTIME_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::Load("a PDFium runtime is already active in this process".into()));
        }
        let mut active_guard = ActiveGuard(true);
        // SAFETY: the absolute regular file was hashed, structurally parsed, target/dependencies
        // checked, and its complete used symbol set verified before OS mapping.
        let load_path = snapshot.load_path();
        let library =
            unsafe { Library::new(&load_path) }.map_err(|error| Error::Load(error.to_string()))?;
        snapshot.discard_validation_bytes();
        macro_rules! symbol { ($name:literal, $ty:ty) => {{
            // SAFETY: export existence was established from the parsed file and the signature is
            // the pinned public PDFium C ABI declared by Chromium build 7999 headers.
            let value = unsafe { library.get::<$ty>(concat!($name, "\0").as_bytes()) }
                .map_err(|error| Error::Load(error.to_string()))?;
            *value
        }} }
        let init = symbol!("FPDF_InitLibraryWithConfig", Init);
        let config = Config::version_2();
        let native = Self {
            _snapshot: snapshot,
            destroy: symbol!("FPDF_DestroyLibrary", Destroy),
            load_document: symbol!("FPDF_LoadMemDocument64", LoadDocument),
            close_document: symbol!("FPDF_CloseDocument", Close),
            page_count: symbol!("FPDF_GetPageCount", Count),
            load_page: symbol!("FPDF_LoadPage", LoadPage),
            close_page: symbol!("FPDF_ClosePage", Close),
            load_text: symbol!("FPDFText_LoadPage", unsafe extern "C" fn(Handle) -> Handle),
            close_text: symbol!("FPDFText_ClosePage", Close),
            text_count: symbol!("FPDFText_CountChars", Count),
            get_text: symbol!("FPDFText_GetText", GetText),
            object_count: symbol!("FPDFPage_CountObjects", Count),
            get_object: symbol!("FPDFPage_GetObject", GetObject),
            object_type: symbol!("FPDFPageObj_GetType", ObjectType),
            create_bitmap: symbol!("FPDFBitmap_CreateEx", CreateBitmap),
            destroy_bitmap: symbol!("FPDFBitmap_Destroy", Close),
            render_page: symbol!("FPDF_RenderPageBitmap", Render),
            last_error: symbol!("FPDF_GetLastError", LastError),
            image_bitmap: symbol!("FPDFImageObj_GetBitmap", GetBitmap),
            bitmap_width: symbol!("FPDFBitmap_GetWidth", GetBitmapInt),
            bitmap_height: symbol!("FPDFBitmap_GetHeight", GetBitmapInt),
            bitmap_stride: symbol!("FPDFBitmap_GetStride", GetBitmapInt),
            bitmap_format: symbol!("FPDFBitmap_GetFormat", GetBitmapInt),
            bitmap_buffer: symbol!("FPDFBitmap_GetBuffer", GetBitmapBuffer),
            _library: library,
        };
        // SAFETY: config version 3 and fields match the pinned fpdfview.h; it lives for the call.
        unsafe { init(&raw const config) };
        active_guard.0 = false;
        Ok(native)
    }

    fn error(&self, operation: &'static str) -> Error {
        Error::Native {
            operation,
            code: u32::try_from(unsafe { (self.last_error)() }).unwrap_or(u32::MAX),
        }
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        unsafe { (self.destroy)() };
        RUNTIME_ACTIVE.store(false, Ordering::Release);
    }
}

struct ActiveGuard(bool);
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        if self.0 {
            RUNTIME_ACTIVE.store(false, Ordering::Release);
        }
    }
}

impl Backend for Native {
    fn load_document(&self, bytes: &[u8], password: Option<&CStr>) -> Result<usize, Error> {
        let pointer = password.map_or(std::ptr::null(), CStr::as_ptr);
        let raw = unsafe { (self.load_document)(bytes.as_ptr().cast(), bytes.len(), pointer) };
        if raw.is_null() { Err(self.error("load_document")) } else { Ok(raw as usize) }
    }
    fn close_document(&self, document: usize) {
        unsafe { (self.close_document)(document as Handle) };
    }
    fn page_count(&self, document: usize) -> Result<u32, Error> {
        nonnegative("page_count", unsafe { (self.page_count)(document as Handle) })
    }
    fn load_page(&self, document: usize, index: u32) -> Result<usize, Error> {
        let index =
            c_int::try_from(index).map_err(|_| invalid("load_page", "index exceeds C int"))?;
        let raw = unsafe { (self.load_page)(document as Handle, index) };
        if raw.is_null() { Err(self.error("load_page")) } else { Ok(raw as usize) }
    }
    fn close_page(&self, page: usize) {
        unsafe { (self.close_page)(page as Handle) };
    }
    fn load_text_page(&self, page: usize) -> Result<usize, Error> {
        let raw = unsafe { (self.load_text)(page as Handle) };
        if raw.is_null() { Err(self.error("load_text_page")) } else { Ok(raw as usize) }
    }
    fn close_text_page(&self, text: usize) {
        unsafe { (self.close_text)(text as Handle) };
    }
    fn text(&self, text: usize, max_units: u32) -> Result<String, Error> {
        let count = nonnegative("text_count", unsafe { (self.text_count)(text as Handle) })?;
        if count > max_units {
            return Err(Error::ResourceLimit {
                limit: "max_text_units_per_page",
                actual: u64::from(count),
                maximum: u64::from(max_units),
            });
        }
        let requested =
            count.checked_add(1).ok_or_else(|| invalid("text", "terminator count overflow"))?;
        let capacity =
            usize::try_from(requested).map_err(|_| invalid("text", "count does not fit usize"))?;
        let mut units = Vec::new();
        units.try_reserve_exact(capacity).map_err(|_| Error::Allocation {
            operation: "text",
            bytes: u64::from(requested) * 2,
        })?;
        units.resize(capacity, 0_u16);
        let copied = unsafe {
            (self.get_text)(
                text as Handle,
                0,
                c_int::try_from(requested).map_err(|_| invalid("text", "count exceeds C int"))?,
                units.as_mut_ptr(),
            )
        };
        if copied <= 0 {
            return Err(self.error("get_text"));
        }
        let copied = usize::try_from(copied)
            .map_err(|_| invalid("text", "negative copied count"))?
            .min(units.len());
        if units.get(copied.saturating_sub(1)) == Some(&0) {
            units.truncate(copied - 1);
        } else {
            units.truncate(copied);
        }
        decode_utf16(&units)
    }
    fn image_objects(
        &self,
        page: usize,
        max_objects: u32,
        max_images: u32,
    ) -> Result<Vec<usize>, Error> {
        let count = nonnegative("object_count", unsafe { (self.object_count)(page as Handle) })?;
        if count > max_objects {
            return Err(Error::ResourceLimit {
                limit: "max_page_objects",
                actual: u64::from(count),
                maximum: u64::from(max_objects),
            });
        }
        let mut images = image_output(count, max_images)?;
        for index in 0..count {
            let object = unsafe {
                (self.get_object)(
                    page as Handle,
                    c_int::try_from(index)
                        .map_err(|_| invalid("image_count", "index exceeds C int"))?,
                )
            };
            if object.is_null() {
                return Err(self.error("get_object"));
            }
            if unsafe { (self.object_type)(object) } == 3 {
                if images.len() >= usize::try_from(max_images).unwrap_or(usize::MAX) {
                    return Err(Error::ResourceLimit {
                        limit: "max_images_per_page",
                        actual: u64::try_from(images.len()).unwrap_or(u64::MAX) + 1,
                        maximum: u64::from(max_images),
                    });
                }
                images.push(object as usize);
            }
        }
        Ok(images)
    }
    fn render(&self, page: usize, width: u32, height: u32) -> Result<Vec<u8>, Error> {
        let stride = width.checked_mul(4).ok_or_else(|| invalid("render", "stride overflow"))?;
        let (width_c, height_c, stride_c) =
            (c_int::try_from(width), c_int::try_from(height), c_int::try_from(stride));
        let (Ok(width_c), Ok(height_c), Ok(stride_c)) = (width_c, height_c, stride_c) else {
            return Err(invalid("render", "dimensions exceed C int"));
        };
        let size = u64::from(stride)
            .checked_mul(u64::from(height))
            .ok_or_else(|| invalid("render", "buffer size overflow"))?;
        let capacity =
            usize::try_from(size).map_err(|_| invalid("render", "buffer does not fit usize"))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Allocation { operation: "render", bytes: size })?;
        bytes.resize(capacity, 0_u8);
        let bitmap = unsafe {
            (self.create_bitmap)(width_c, height_c, 4, bytes.as_mut_ptr().cast(), stride_c)
        };
        if bitmap.is_null() {
            return Err(self.error("create_bitmap"));
        }
        unsafe {
            (self.render_page)(bitmap, page as Handle, 0, 0, width_c, height_c, 0, 0x10 | 0x800);
        };
        unsafe { (self.destroy_bitmap)(bitmap) };
        Ok(bytes)
    }
    fn image_bitmap(&self, image: usize, limits: Limits) -> Result<ImageBitmap, Error> {
        let bitmap = unsafe { (self.image_bitmap)(image as Handle) };
        if bitmap.is_null() {
            return Err(self.error("image_bitmap"));
        }
        let bitmap = BitmapGuard { raw: bitmap, destroy: self.destroy_bitmap };
        let width = nonnegative("image_bitmap_width", unsafe { (self.bitmap_width)(bitmap.raw) })?;
        let height =
            nonnegative("image_bitmap_height", unsafe { (self.bitmap_height)(bitmap.raw) })?;
        let stride =
            nonnegative("image_bitmap_stride", unsafe { (self.bitmap_stride)(bitmap.raw) })?;
        for dimension in [width, height] {
            if dimension > limits.max_render_dimension {
                return Err(Error::ResourceLimit {
                    limit: "max_render_dimension",
                    actual: u64::from(dimension),
                    maximum: u64::from(limits.max_render_dimension),
                });
            }
        }
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| invalid("image_bitmap", "pixel count overflow"))?;
        if pixels > limits.max_render_pixels {
            return Err(Error::ResourceLimit {
                limit: "max_render_pixels",
                actual: pixels,
                maximum: limits.max_render_pixels,
            });
        }
        let (format, bytes_per_pixel) = match unsafe { (self.bitmap_format)(bitmap.raw) } {
            1 => (PixelFormat::Gray, 1_u32),
            2 => (PixelFormat::Bgr, 3),
            3 => (PixelFormat::Bgrx, 4),
            4 => (PixelFormat::Bgra, 4),
            value => return Err(invalid("image_bitmap_format", &format!("unknown {value}"))),
        };
        let minimum_stride = width
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| invalid("image_bitmap", "minimum stride overflow"))?;
        if stride < minimum_stride {
            return Err(invalid("image_bitmap", "native stride is shorter than one row"));
        }
        let size = u64::from(stride)
            .checked_mul(u64::from(height))
            .ok_or_else(|| invalid("image_bitmap", "buffer size overflow"))?;
        if size > limits.max_bitmap_bytes {
            return Err(Error::ResourceLimit {
                limit: "max_bitmap_bytes",
                actual: size,
                maximum: limits.max_bitmap_bytes,
            });
        }
        let capacity = usize::try_from(size)
            .map_err(|_| invalid("image_bitmap", "buffer does not fit usize"))?;
        let source = unsafe { (self.bitmap_buffer)(bitmap.raw) }.cast::<u8>();
        if source.is_null() && capacity != 0 {
            return Err(self.error("image_bitmap_buffer"));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Allocation { operation: "image_bitmap", bytes: size })?;
        bytes.resize(capacity, 0);
        if capacity != 0 {
            // SAFETY: PDFium reports `stride * height` bytes owned by `bitmap`; the bitmap remains
            // alive through this copy, and all arithmetic/allocation was checked above.
            bytes.copy_from_slice(unsafe { std::slice::from_raw_parts(source, capacity) });
        }
        Ok(ImageBitmap { width, height, stride, format, bytes })
    }
}

struct BitmapGuard {
    raw: Handle,
    destroy: Close,
}
impl Drop for BitmapGuard {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.raw) };
    }
}

fn decode_utf16(units: &[u16]) -> Result<String, Error> {
    let capacity = units
        .len()
        .checked_mul(3)
        .ok_or(Error::Allocation { operation: "text_utf8", bytes: u64::MAX })?;
    let mut text = String::new();
    text.try_reserve_exact(capacity).map_err(|_| Error::Allocation {
        operation: "text_utf8",
        bytes: u64::try_from(capacity).unwrap_or(u64::MAX),
    })?;
    for character in char::decode_utf16(units.iter().copied()) {
        text.push(character.map_err(|error| {
            invalid("text", &format!("invalid UTF-16 surrogate {:x}", error.unpaired_surrogate()))
        })?);
    }
    Ok(text)
}

fn invalid(operation: &'static str, detail: &str) -> Error {
    Error::InvalidResult { operation, detail: detail.into() }
}
fn image_output(count: u32, maximum: u32) -> Result<Vec<usize>, Error> {
    let reserve = count.min(maximum);
    let mut images = Vec::new();
    if reserve != 0 {
        images.try_reserve_exact(usize::try_from(reserve).unwrap_or(usize::MAX)).map_err(|_| {
            Error::Allocation {
                operation: "image_objects",
                bytes: u64::from(reserve)
                    * u64::try_from(std::mem::size_of::<usize>()).unwrap_or(8),
            }
        })?;
    }
    Ok(images)
}
fn nonnegative(operation: &'static str, value: c_int) -> Result<u32, Error> {
    u32::try_from(value).map_err(|_| invalid(operation, "native count was negative"))
}

struct Snapshot {
    directory: tempfile::TempDir,
    path: PathBuf,
    file: Option<File>,
    bytes: Vec<u8>,
}

impl Snapshot {
    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[cfg(unix)]
    fn load_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd as _;
        debug_assert_eq!(self.path.parent(), Some(self.directory.path()));
        let fd = self.file.as_ref().expect("snapshot file remains open").as_raw_fd();
        if cfg!(target_os = "macos") {
            PathBuf::from(format!("/dev/fd/{fd}"))
        } else {
            PathBuf::from(format!("/proc/self/fd/{fd}"))
        }
    }

    #[cfg(windows)]
    fn load_path(&self) -> PathBuf {
        debug_assert_eq!(self.path.parent(), Some(self.directory.path()));
        self.path.clone()
    }

    fn discard_validation_bytes(&mut self) {
        self.bytes = Vec::new();
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        drop(self.file.take());
        #[cfg(windows)]
        if let Ok(metadata) = fs::metadata(&self.path) {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            let _ = fs::set_permissions(&self.path, permissions);
        }
    }
}

fn validated_snapshot(path: &Path, artifact: &Artifact) -> Result<Snapshot, Error> {
    validated_snapshot_with_hook(path, artifact, || {}, || {})
}

fn validated_snapshot_with_hook(
    path: &Path,
    artifact: &Artifact,
    after_metadata: impl FnOnce(),
    after_snapshot: impl FnOnce(),
) -> Result<Snapshot, Error> {
    if !path.is_absolute()
        || path.file_name().and_then(|value| value.to_str()) != Some(&artifact.library)
    {
        return Err(Error::InvalidPath(
            "path must be absolute and use the pinned library filename".into(),
        ));
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|error| Error::InvalidPath(error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::InvalidPath("runtime must be a non-symlink regular file".into()));
    }
    let canonical =
        fs::canonicalize(path).map_err(|error| Error::InvalidPath(error.to_string()))?;
    let parent = path.parent().ok_or_else(|| Error::InvalidPath("runtime has no parent".into()))?;
    let canonical_parent =
        fs::canonicalize(parent).map_err(|error| Error::InvalidPath(error.to_string()))?;
    if canonical.parent() != Some(canonical_parent.as_path()) {
        return Err(Error::InvalidPath("runtime escapes its canonical parent".into()));
    }
    let mut file = open_locked(&canonical)?;
    let opened_metadata = file.metadata().map_err(|error| Error::InvalidPath(error.to_string()))?;
    if !opened_metadata.is_file() {
        return Err(Error::InvalidPath("opened runtime is not a regular file".into()));
    }
    if opened_metadata.len() != artifact.library_size {
        return Err(Error::ResourceLimit {
            limit: "pdfium_runtime_bytes",
            actual: opened_metadata.len(),
            maximum: artifact.library_size,
        });
    }
    after_metadata();
    let bounded = artifact
        .library_size
        .checked_add(1)
        .ok_or_else(|| Error::BinaryValidation("runtime size bound overflowed".into()))?;
    let capacity = usize::try_from(bounded).map_err(|_| Error::ResourceLimit {
        limit: "pdfium_runtime_bytes",
        actual: bounded,
        maximum: artifact.library_size,
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| Error::Allocation { operation: "pdfium_runtime_snapshot", bytes: bounded })?;
    std::io::Read::by_ref(&mut file)
        .take(bounded)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::InvalidPath(error.to_string()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != artifact.library_size {
        return Err(Error::ResourceLimit {
            limit: "pdfium_runtime_bytes",
            actual: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            maximum: artifact.library_size,
        });
    }
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != artifact.library_sha256 {
        return Err(Error::DigestMismatch { expected: artifact.library_sha256.clone(), actual });
    }
    drop(file);
    let directory = tempfile::Builder::new()
        .prefix("into-markdown-pdfium-")
        .tempdir()
        .map_err(|error| Error::InvalidPath(format!("cannot create private snapshot: {error}")))?;
    let snapshot_path = directory.path().join(&artifact.library);
    let mut snapshot_file =
        OpenOptions::new().write(true).create_new(true).open(&snapshot_path).map_err(|error| {
            Error::InvalidPath(format!("cannot create private snapshot: {error}"))
        })?;
    snapshot_file
        .write_all(&bytes)
        .and_then(|()| snapshot_file.sync_all())
        .map_err(|error| Error::InvalidPath(format!("cannot write private snapshot: {error}")))?;
    set_snapshot_read_only(&snapshot_file)?;
    drop(snapshot_file);
    let snapshot_file = open_locked(&snapshot_path)?;
    after_snapshot();
    Ok(Snapshot { directory, path: snapshot_path, file: Some(snapshot_file), bytes })
}

#[cfg(unix)]
fn set_snapshot_read_only(file: &File) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(0o400))
        .map_err(|error| Error::InvalidPath(format!("cannot protect private snapshot: {error}")))
}

#[cfg(windows)]
fn set_snapshot_read_only(file: &File) -> Result<(), Error> {
    let mut permissions =
        file.metadata().map_err(|error| Error::InvalidPath(error.to_string()))?.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)
        .map_err(|error| Error::InvalidPath(format!("cannot protect private snapshot: {error}")))
}

#[cfg(unix)]
fn open_locked(path: &Path) -> Result<File, Error> {
    use std::os::unix::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| Error::InvalidPath(error.to_string()))
}

#[cfg(windows)]
fn open_locked(path: &Path) -> Result<File, Error> {
    use std::os::windows::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(path)
        .map_err(|error| Error::InvalidPath(error.to_string()))
}

fn validate_binary(bytes: &[u8], platform: Platform, artifact: &Artifact) -> Result<(), Error> {
    let file =
        object::File::parse(bytes).map_err(|error| Error::BinaryValidation(error.to_string()))?;
    let (format, architecture) = match platform {
        Platform::MacArm64 => (BinaryFormat::MachO, Architecture::Aarch64),
        Platform::LinuxX64 => (BinaryFormat::Elf, Architecture::X86_64),
        Platform::LinuxArm64 => (BinaryFormat::Elf, Architecture::Aarch64),
        Platform::WindowsX64 => (BinaryFormat::Coff, Architecture::X86_64),
    };
    if file.format() != format || file.architecture() != architecture || !file.is_64() {
        return Err(Error::BinaryValidation(format!(
            "expected {format:?}/{architecture:?}/64-bit, got {:?}/{:?}/{}-bit",
            file.format(),
            file.architecture(),
            if file.is_64() { 64 } else { 32 }
        )));
    }
    let exports = file.exports().map_err(|error| Error::BinaryValidation(error.to_string()))?;
    let mut found = [false; CONSUMED_EXPORTS.len()];
    for export in exports {
        let export = export.map_err(|error| Error::BinaryValidation(error.to_string()))?;
        let NameOrOrdinal::Name(name) = export.name() else {
            continue;
        };
        for (index, required) in artifact.required_exports.iter().enumerate() {
            if name == required.as_bytes()
                || platform == Platform::MacArm64
                    && name.strip_prefix(b"_") == Some(required.as_bytes())
            {
                found[index] = true;
            }
        }
    }
    for (required, present) in artifact.required_exports.iter().zip(found) {
        if !present {
            return Err(Error::BinaryValidation(format!("missing required ABI export {required}")));
        }
    }
    let imports = file.imports().map_err(|error| Error::BinaryValidation(error.to_string()))?;
    for import in imports {
        let import = import.map_err(|error| Error::BinaryValidation(error.to_string()))?;
        let library = String::from_utf8_lossy(import.library()).to_ascii_lowercase();
        if !artifact.allowed_dependencies.iter().any(|item| library == item.to_ascii_lowercase()) {
            return Err(Error::BinaryValidation(format!(
                "unreviewed dynamic dependency {library}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_config_exactly_matches_chromium_7999_version_2_prefix() {
        assert_eq!(usize::BITS, 64, "all supported PDFium targets are 64-bit");
        let config = Config::version_2();
        assert_eq!(config.version, 2);
        assert_eq!(std::mem::size_of::<Config>(), 32);
        assert_eq!(std::mem::align_of::<Config>(), 8);
        assert_eq!(std::mem::offset_of!(Config, version), 0);
        assert_eq!(std::mem::offset_of!(Config, user_font_paths), 8);
        assert_eq!(std::mem::offset_of!(Config, isolate), 16);
        assert_eq!(std::mem::offset_of!(Config, slot), 24);
    }

    #[test]
    fn manifest_is_strict_and_exactly_binds_consumed_ffi() {
        for platform in
            [Platform::MacArm64, Platform::LinuxX64, Platform::LinuxArm64, Platform::WindowsX64]
        {
            let artifact = platform.artifact().unwrap();
            assert_eq!(artifact.target, platform.target());
            assert!(artifact.library_size > 1_000_000);
            assert!(sha256(&artifact.library_sha256));
            assert!(sha256(&artifact.archive_sha256));
            assert_eq!(
                artifact.required_exports.iter().map(String::as_str).collect::<BTreeSet<_>>(),
                CONSUMED_EXPORTS.iter().copied().collect()
            );
        }
        let mut value: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
        value["required_exports"].as_array_mut().unwrap().pop();
        assert!(matches!(
            artifact_from_manifest(&value.to_string(), Platform::MacArm64),
            Err(Error::BinaryValidation(message)) if message.contains("exactly match")
        ));
        let mut value: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
        let duplicate = value["required_exports"][0].clone();
        value["required_exports"].as_array_mut().unwrap().push(duplicate);
        assert!(matches!(
            artifact_from_manifest(&value.to_string(), Platform::MacArm64),
            Err(Error::BinaryValidation(message)) if message.contains("exactly match")
        ));
        let mut value: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
        value["targets"]["aarch64-apple-darwin"]["unexpected"] = true.into();
        assert!(matches!(
            artifact_from_manifest(&value.to_string(), Platform::MacArm64),
            Err(Error::BinaryValidation(message)) if message.contains("unknown field")
        ));
    }
    #[test]
    fn unsupported_platform_is_stable() {
        assert_eq!(
            Platform::from_os_arch("freebsd", "x86_64"),
            Err(Error::UnsupportedPlatform { os: "freebsd".into(), arch: "x86_64".into() })
        );
    }
    #[test]
    fn rejects_relative_paths_before_io() {
        let artifact = Platform::LinuxX64.artifact().unwrap();
        assert!(matches!(
            validated_snapshot(Path::new("libpdfium.so"), &artifact),
            Err(Error::InvalidPath(_))
        ));
    }
    #[test]
    fn rejects_corrupt_runtime_by_real_digest() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = Platform::current().unwrap().artifact().unwrap();
        let path = dir.path().join(&artifact.library);
        File::create(&path).unwrap().set_len(artifact.library_size).unwrap();
        assert!(matches!(Native::load(&path), Err(Error::DigestMismatch { .. })));
    }

    #[test]
    fn runtime_read_is_size_bounded_and_detects_growth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("libpdfium.so");
        fs::write(&path, b"good").unwrap();
        let artifact = test_artifact("libpdfium.so", b"good");
        let oversized = test_artifact("libpdfium.so", b"goo");
        assert!(matches!(
            validated_snapshot(&path, &oversized),
            Err(Error::ResourceLimit { limit: "pdfium_runtime_bytes", .. })
        ));
        let result = validated_snapshot_with_hook(
            &path,
            &artifact,
            || {
                #[cfg(unix)]
                OpenOptions::new().append(true).open(&path).unwrap().write_all(b"x").unwrap();
                #[cfg(windows)]
                assert!(OpenOptions::new().append(true).open(&path).is_err());
            },
            || {},
        );
        #[cfg(unix)]
        assert!(matches!(result, Err(Error::ResourceLimit { limit: "pdfium_runtime_bytes", .. })));
        #[cfg(windows)]
        assert!(result.is_ok());
    }

    #[test]
    fn private_snapshot_survives_in_place_and_replacement_races() {
        for replace in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("libpdfium.so");
            fs::write(&path, b"good").unwrap();
            let artifact = test_artifact("libpdfium.so", b"good");
            let snapshot = validated_snapshot_with_hook(
                &path,
                &artifact,
                || {},
                || {
                    if replace {
                        fs::rename(&path, dir.path().join("old.so")).unwrap();
                    }
                    fs::write(&path, b"evil").unwrap();
                },
            )
            .unwrap();
            assert_eq!(snapshot.bytes(), b"good");
            assert_eq!(fs::read(snapshot.path()).unwrap(), b"good");
            assert_eq!(fs::read(&path).unwrap(), b"evil");
        }
    }

    #[test]
    fn empty_image_page_never_reserves_extreme_limit() {
        let images = image_output(0, u32::MAX).unwrap();
        assert_eq!(images.capacity(), 0);
    }

    #[test]
    fn utf16_conversion_is_bounded_and_rejects_invalid_surrogates() {
        assert_eq!(decode_utf16(&[u16::from(b'O'), u16::from(b'K')]).unwrap(), "OK");
        assert!(matches!(
            decode_utf16(&[0xd800]),
            Err(Error::InvalidResult { operation: "text", .. })
        ));
    }

    fn test_artifact(name: &str, bytes: &[u8]) -> Artifact {
        Artifact {
            target: "test".into(),
            library: name.into(),
            library_path: name.into(),
            library_size: u64::try_from(bytes.len()).unwrap(),
            library_sha256: format!("{:x}", Sha256::digest(bytes)),
            archive_sha256: "0".repeat(64),
            archive_size: 1,
            allowed_dependencies: vec![],
            required_exports: vec![],
        }
    }
}

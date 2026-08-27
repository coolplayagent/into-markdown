use crate::{
    Backend, Character, CharacterAllocationPlan, Error, ImageAllocationPlan, ImageBitmap,
    ImageObject, Limits, Link, LinkAllocationPlan, LinkTarget, PATH_SCAN_CHECKPOINT_OBJECTS,
    PageInfo, PathBoundsAllocationPlan, PdfRect, PixelFormat,
};
use libloading::Library;
use object::read::elf::Dyn as _;
use object::{Architecture, BinaryFormat, NameOrOrdinal, Object as _};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

struct FixedOutput<T> {
    slots: Box<[std::mem::MaybeUninit<T>]>,
    initialized: usize,
}

impl<T> FixedOutput<T> {
    fn new(length: usize, operation: &'static str) -> Result<Self, Error> {
        Ok(Self { slots: try_uninit_boxed_slice(length, operation)?, initialized: 0 })
    }

    fn len(&self) -> usize {
        self.initialized
    }

    fn push(&mut self, value: T, operation: &'static str) -> Result<(), Error> {
        let Some(slot) = self.slots.get_mut(self.initialized) else {
            return Err(invalid(operation, "materialized count exceeded preflight plan"));
        };
        slot.write(value);
        self.initialized += 1;
        Ok(())
    }

    fn into_vec(self, operation: &'static str) -> Result<Vec<T>, Error> {
        if self.initialized != self.slots.len() {
            return Err(invalid(operation, "materialized count changed after preflight"));
        }
        let this = std::mem::ManuallyDrop::new(self);
        let raw = Box::into_raw(unsafe { std::ptr::read(&raw const this.slots) });
        // SAFETY: every slot was initialized, checked immediately above; T and
        // MaybeUninit<T> have identical layout and allocation provenance.
        let initialized = unsafe { Box::from_raw(raw as *mut [T]) };
        Ok(initialized.into_vec())
    }
}

impl<T> Drop for FixedOutput<T> {
    fn drop(&mut self) {
        for slot in &mut self.slots[..self.initialized] {
            // SAFETY: the prefix tracked by `initialized` was written exactly once.
            unsafe { slot.assume_init_drop() };
        }
    }
}

fn try_uninit_boxed_slice<T>(
    length: usize,
    operation: &'static str,
) -> Result<Box<[std::mem::MaybeUninit<T>]>, Error> {
    if length == 0 {
        return Ok(Box::new([]));
    }
    let layout = std::alloc::Layout::array::<std::mem::MaybeUninit<T>>(length)
        .map_err(|_| Error::Allocation { operation, bytes: u64::MAX })?;
    // SAFETY: the non-zero valid layout is owned by the returned Box on success.
    let raw = unsafe { std::alloc::alloc(layout) }.cast::<std::mem::MaybeUninit<T>>();
    if raw.is_null() {
        return Err(Error::Allocation {
            operation,
            bytes: u64::try_from(layout.size()).unwrap_or(u64::MAX),
        });
    }
    let slice = std::ptr::slice_from_raw_parts_mut(raw, length);
    // SAFETY: `slice` is the exact allocation above and contains uninitialized slots.
    Ok(unsafe { Box::from_raw(slice) })
}

pub(crate) fn zeroed_boxed_bytes(
    length: usize,
    operation: &'static str,
) -> Result<Box<[u8]>, Error> {
    let mut bytes = try_uninit_boxed_slice::<u8>(length, operation)?;
    // SAFETY: writing zero initializes every byte in the exact-layout allocation.
    unsafe { std::ptr::write_bytes(bytes.as_mut_ptr().cast::<u8>(), 0, length) };
    let raw = Box::into_raw(bytes);
    // SAFETY: every byte was initialized above and MaybeUninit<u8> has u8 layout.
    Ok(unsafe { Box::from_raw(raw as *mut [u8]) })
}

pub(crate) fn fixed_string(value: &str, operation: &'static str) -> Result<String, Error> {
    let mut bytes = zeroed_boxed_bytes(value.len(), operation)?.into_vec();
    bytes.copy_from_slice(value.as_bytes());
    String::from_utf8(bytes).map_err(|_| invalid(operation, "source string is not UTF-8"))
}

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
    "FPDFText_GetUnicode",
    "FPDFText_GetCharBox",
    "FPDFText_GetFontSize",
    "FPDFText_GetFontInfo",
    "FPDFText_GetCharAngle",
    "FPDF_GetPageWidthF",
    "FPDF_GetPageHeightF",
    "FPDFPage_GetRotation",
    "FPDFPage_CountObjects",
    "FPDFPage_GetObject",
    "FPDFPageObj_GetType",
    "FPDFPageObj_GetBounds",
    "FPDFLink_Enumerate",
    "FPDFLink_GetAnnotRect",
    "FPDFLink_GetAction",
    "FPDFAction_GetType",
    "FPDFAction_GetURIPath",
    "FPDFLink_GetDest",
    "FPDFDest_GetDestPageIndex",
    "FPDFLink_LoadWebLinks",
    "FPDFLink_CountWebLinks",
    "FPDFLink_GetURL",
    "FPDFLink_CountRects",
    "FPDFLink_GetRect",
    "FPDFLink_CloseWebLinks",
    "FPDFBitmap_CreateEx",
    "FPDFBitmap_Destroy",
    "FPDFBitmap_GetBuffer",
    "FPDFBitmap_GetFormat",
    "FPDFBitmap_GetHeight",
    "FPDFBitmap_GetStride",
    "FPDFBitmap_GetWidth",
    "FPDFImageObj_GetBitmap",
    "FPDFImageObj_GetImagePixelSize",
    "FPDF_RenderPageBitmap",
    "FPDF_GetLastError",
];
const MAX_RUNTIME_LIBRARY_BYTES: u64 = 16 * 1024 * 1024;
#[cfg(target_os = "macos")]
const MAX_RELEASE_PROJECTION_BYTES: u64 = 32 * 1024 * 1024;
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

#[cfg(target_os = "macos")]
#[derive(Debug, Deserialize)]
struct ReleaseProjection {
    schema_version: u64,
    target: String,
    files: Vec<ReleaseProjectionFile>,
    #[serde(default)]
    native_transformations: Vec<NativeTransformation>,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Deserialize)]
struct ReleaseProjectionFile {
    path: String,
    bytes: u64,
    sha256: String,
    kind: String,
    component_id: Option<String>,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Deserialize)]
struct NativeTransformation {
    component_id: String,
    path: String,
    kind: String,
    source_bytes: u64,
    source_sha256: String,
    output_bytes: u64,
    output_sha256: String,
}

struct SignedDerivativeAuthority {
    bytes: u64,
    sha256: String,
}

/// Supported runtime target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacArm64,
    LinuxX64,
    LinuxArm64,
    WindowsX64,
    WindowsArm64,
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
            ("windows", "aarch64") => Ok(Self::WindowsArm64),
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
            Self::WindowsArm64 => "aarch64-pc-windows-msvc",
        }
    }
}

fn validate_manifest(manifest: &RuntimeManifest) -> Result<(), Error> {
    let expected_targets = BTreeSet::from([
        "aarch64-apple-darwin",
        "aarch64-pc-windows-msvc",
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

#[cfg(windows)]
const fn last_error_code(value: c_ulong) -> u32 {
    value
}

#[cfg(not(windows))]
fn last_error_code(value: c_ulong) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

type GetBitmap = unsafe extern "C" fn(Handle) -> Handle;
type GetImagePixelSize = unsafe extern "C" fn(Handle, *mut c_uint, *mut c_uint) -> c_int;
type GetBitmapInt = unsafe extern "C" fn(Handle) -> c_int;
type GetBitmapBuffer = unsafe extern "C" fn(Handle) -> *mut c_void;
type GetDouble = unsafe extern "C" fn(Handle, c_int) -> f64;
type GetFloat = unsafe extern "C" fn(Handle, c_int) -> f32;
type GetUnicode = unsafe extern "C" fn(Handle, c_int) -> c_uint;
type GetActionType = unsafe extern "C" fn(Handle) -> c_ulong;
type GetCharBox =
    unsafe extern "C" fn(Handle, c_int, *mut f64, *mut f64, *mut f64, *mut f64) -> c_int;
type GetFontInfo = unsafe extern "C" fn(Handle, c_int, *mut c_void, c_ulong, *mut c_int) -> c_ulong;
type GetPageFloat = unsafe extern "C" fn(Handle) -> f32;
type GetBounds = unsafe extern "C" fn(Handle, *mut f32, *mut f32, *mut f32, *mut f32) -> c_int;
type EnumerateLink = unsafe extern "C" fn(Handle, *mut c_int, *mut Handle) -> c_int;
type GetAnnotRect = unsafe extern "C" fn(Handle, *mut FsRectF) -> c_int;
type GetHandle = unsafe extern "C" fn(Handle) -> Handle;
type GetUri = unsafe extern "C" fn(Handle, Handle, *mut c_void, c_ulong) -> c_ulong;
type GetDestIndex = unsafe extern "C" fn(Handle, Handle) -> c_int;
type GetUrl = unsafe extern "C" fn(Handle, c_int, *mut u16, c_int) -> c_int;
type GetLinkRect =
    unsafe extern "C" fn(Handle, c_int, c_int, *mut f64, *mut f64, *mut f64, *mut f64) -> c_int;

#[repr(C)]
#[derive(Default)]
struct FsRectF {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

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
    text_unicode: GetUnicode,
    text_char_box: GetCharBox,
    text_font_size: GetDouble,
    text_font_info: GetFontInfo,
    text_char_angle: GetFloat,
    page_width: GetPageFloat,
    page_height: GetPageFloat,
    page_rotation: Count,
    object_count: Count,
    get_object: GetObject,
    object_type: ObjectType,
    object_bounds: GetBounds,
    enumerate_link: EnumerateLink,
    link_rect: GetAnnotRect,
    link_action: GetHandle,
    action_type: GetActionType,
    action_uri: GetUri,
    link_dest: unsafe extern "C" fn(Handle, Handle) -> Handle,
    dest_page_index: GetDestIndex,
    load_web_links: unsafe extern "C" fn(Handle) -> Handle,
    web_link_count: Count,
    web_link_url: GetUrl,
    web_link_rect_count: unsafe extern "C" fn(Handle, c_int) -> c_int,
    web_link_rect: GetLinkRect,
    close_web_links: Close,
    create_bitmap: CreateBitmap,
    destroy_bitmap: Close,
    render_page: Render,
    last_error: LastError,
    image_bitmap: GetBitmap,
    image_pixel_size: GetImagePixelSize,
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
            text_unicode: symbol!("FPDFText_GetUnicode", GetUnicode),
            text_char_box: symbol!("FPDFText_GetCharBox", GetCharBox),
            text_font_size: symbol!("FPDFText_GetFontSize", GetDouble),
            text_font_info: symbol!("FPDFText_GetFontInfo", GetFontInfo),
            text_char_angle: symbol!("FPDFText_GetCharAngle", GetFloat),
            page_width: symbol!("FPDF_GetPageWidthF", GetPageFloat),
            page_height: symbol!("FPDF_GetPageHeightF", GetPageFloat),
            page_rotation: symbol!("FPDFPage_GetRotation", Count),
            object_count: symbol!("FPDFPage_CountObjects", Count),
            get_object: symbol!("FPDFPage_GetObject", GetObject),
            object_type: symbol!("FPDFPageObj_GetType", ObjectType),
            object_bounds: symbol!("FPDFPageObj_GetBounds", GetBounds),
            enumerate_link: symbol!("FPDFLink_Enumerate", EnumerateLink),
            link_rect: symbol!("FPDFLink_GetAnnotRect", GetAnnotRect),
            link_action: symbol!("FPDFLink_GetAction", GetHandle),
            action_type: symbol!("FPDFAction_GetType", GetActionType),
            action_uri: symbol!("FPDFAction_GetURIPath", GetUri),
            link_dest: symbol!("FPDFLink_GetDest", unsafe extern "C" fn(Handle, Handle) -> Handle),
            dest_page_index: symbol!("FPDFDest_GetDestPageIndex", GetDestIndex),
            load_web_links: symbol!(
                "FPDFLink_LoadWebLinks",
                unsafe extern "C" fn(Handle) -> Handle
            ),
            web_link_count: symbol!("FPDFLink_CountWebLinks", Count),
            web_link_url: symbol!("FPDFLink_GetURL", GetUrl),
            web_link_rect_count: symbol!(
                "FPDFLink_CountRects",
                unsafe extern "C" fn(Handle, c_int) -> c_int
            ),
            web_link_rect: symbol!("FPDFLink_GetRect", GetLinkRect),
            close_web_links: symbol!("FPDFLink_CloseWebLinks", Close),
            create_bitmap: symbol!("FPDFBitmap_CreateEx", CreateBitmap),
            destroy_bitmap: symbol!("FPDFBitmap_Destroy", Close),
            render_page: symbol!("FPDF_RenderPageBitmap", Render),
            last_error: symbol!("FPDF_GetLastError", LastError),
            image_bitmap: symbol!("FPDFImageObj_GetBitmap", GetBitmap),
            image_pixel_size: symbol!("FPDFImageObj_GetImagePixelSize", GetImagePixelSize),
            bitmap_width: symbol!("FPDFBitmap_GetWidth", GetBitmapInt),
            bitmap_height: symbol!("FPDFBitmap_GetHeight", GetBitmapInt),
            bitmap_stride: symbol!("FPDFBitmap_GetStride", GetBitmapInt),
            bitmap_format: symbol!("FPDFBitmap_GetFormat", GetBitmapInt),
            bitmap_buffer: symbol!("FPDFBitmap_GetBuffer", GetBitmapBuffer),
            _library: library,
        };
        // SAFETY: config version 2 and fields match the pinned fpdfview.h; it lives for the call.
        unsafe { init(&raw const config) };
        active_guard.0 = false;
        Ok(native)
    }

    fn error(&self, operation: &'static str) -> Error {
        Error::Native { operation, code: last_error_code(unsafe { (self.last_error)() }) }
    }

    fn font_name_with_length(
        &self,
        text: usize,
        index: c_int,
        needed: c_ulong,
        maximum: u32,
        planned_capacity: u64,
    ) -> Result<Option<String>, Error> {
        let mut flags = 0_i32;
        if needed == 0 {
            return Ok(None);
        }
        let needed_u64 = c_ulong_to_u64(needed);
        if needed_u64 > u64::from(maximum) {
            return Err(Error::ResourceLimit {
                limit: "max_font_name_bytes",
                actual: needed_u64,
                maximum: u64::from(maximum),
            });
        }
        let capacity = usize::try_from(needed)
            .map_err(|_| invalid("font_name", "length does not fit usize"))?;
        if needed_u64 > planned_capacity {
            return Err(invalid("font_name", "allocator capacity exceeded preflight plan"));
        }
        let mut bytes = zeroed_boxed_bytes(capacity, "font_name")?;
        let copied = unsafe {
            (self.text_font_info)(
                text as Handle,
                index,
                bytes.as_mut_ptr().cast(),
                c_ulong::try_from(capacity)
                    .map_err(|_| invalid("font_name", "length exceeds C ulong"))?,
                &raw mut flags,
            )
        };
        if copied == 0 || copied > needed {
            return Err(invalid("font_name", "native length changed or was zero"));
        }
        let mut bytes = bytes.into_vec();
        bytes.truncate(usize::try_from(copied).unwrap_or(capacity));
        if bytes.last() == Some(&0) {
            let _ = bytes.pop();
        }
        if bytes.contains(&0) {
            return Err(invalid("font_name", "embedded NUL"));
        }
        let value = String::from_utf8(bytes)
            .map_err(|_| invalid("font_name", "name is not valid UTF-8"))?;
        if value.chars().any(char::is_control) {
            return Err(invalid("font_name", "name contains control characters"));
        }
        Ok((!value.is_empty()).then_some(value))
    }

    fn action_uri_with_length(
        &self,
        document: usize,
        action: Handle,
        needed: c_ulong,
        maximum: u32,
    ) -> Result<String, Error> {
        bounded_native_bytes("action_uri", needed, maximum, |buffer, length| unsafe {
            (self.action_uri)(document as Handle, action, buffer.cast(), length)
        })
    }

    fn web_uri_with_length(
        &self,
        web: Handle,
        index: u32,
        needed: c_int,
        maximum: u32,
    ) -> Result<String, Error> {
        let index =
            c_int::try_from(index).map_err(|_| invalid("web_uri", "index exceeds C int"))?;
        if needed <= 0 {
            return Err(self.error("web_uri"));
        }
        let units = u32::try_from(needed).map_err(|_| invalid("web_uri", "negative length"))?;
        let bytes = units.checked_mul(2).ok_or_else(|| invalid("web_uri", "length overflow"))?;
        if bytes > maximum {
            return Err(Error::ResourceLimit {
                limit: "max_link_bytes",
                actual: u64::from(bytes),
                maximum: u64::from(maximum),
            });
        }
        let mut buffer = try_uninit_boxed_slice::<u16>(units as usize, "web_uri")?;
        // SAFETY: initialize the full fixed buffer before the native partial-write API.
        unsafe { std::ptr::write_bytes(buffer.as_mut_ptr().cast::<u16>(), 0, units as usize) };
        let copied =
            unsafe { (self.web_link_url)(web, index, buffer.as_mut_ptr().cast::<u16>(), needed) };
        if copied <= 0 || copied > needed {
            return Err(invalid("web_uri", "native length changed or was zero"));
        }
        let copied =
            usize::try_from(copied).map_err(|_| invalid("web_uri", "length does not fit usize"))?;
        // SAFETY: the complete fixed buffer was initialized with zeroes before the FFI call.
        let mut buffer = unsafe { buffer.assume_init() }.into_vec();
        buffer.truncate(copied);
        if buffer.last() == Some(&0) {
            let _ = buffer.pop();
        }
        if buffer.contains(&0) {
            return Err(invalid("web_uri", "embedded NUL"));
        }
        decode_utf16(&buffer)
    }
}

struct WebLinksGuard {
    raw: Handle,
    close: Close,
}
impl Drop for WebLinksGuard {
    fn drop(&mut self) {
        unsafe { (self.close)(self.raw) }
    }
}

#[allow(clippy::cast_possible_truncation)]
fn finite_rect(
    operation: &'static str,
    left: impl Into<f64>,
    bottom: impl Into<f64>,
    right: impl Into<f64>,
    top: impl Into<f64>,
) -> Result<PdfRect, Error> {
    let (left, bottom, right, top) = (left.into(), bottom.into(), right.into(), top.into());
    if !left.is_finite()
        || !bottom.is_finite()
        || !right.is_finite()
        || !top.is_finite()
        || right < left
        || top < bottom
        || [left, bottom, right, top].into_iter().any(|value| value.abs() > f64::from(f32::MAX))
    {
        return Err(invalid(operation, "invalid or non-finite rectangle"));
    }
    Ok(PdfRect { left: left as f32, bottom: bottom as f32, right: right as f32, top: top as f32 })
}

fn object_bounds(
    native: &Native,
    object: Handle,
    operation: &'static str,
) -> Result<PdfRect, Error> {
    let (mut left, mut bottom, mut right, mut top) = (0.0, 0.0, 0.0, 0.0);
    if unsafe {
        (native.object_bounds)(object, &raw mut left, &raw mut bottom, &raw mut right, &raw mut top)
    } == 0
    {
        return Err(native.error(operation));
    }
    finite_rect(operation, left, bottom, right, top)
}

fn path_scan_checkpoint(
    index: u32,
    operation: &'static str,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<(), Error> {
    if index.is_multiple_of(PATH_SCAN_CHECKPOINT_OBJECTS) && !checkpoint() {
        return Err(invalid(operation, "caller interrupted PATH object scan"));
    }
    Ok(())
}

#[allow(clippy::cast_possible_truncation)]
fn f64_to_f32(value: f64) -> f32 {
    value as f32
}

#[allow(clippy::useless_conversion)] // c_ulong is 32-bit on Windows and 64-bit on LP64 targets.
fn c_ulong_to_u64(value: c_ulong) -> u64 {
    u64::from(value)
}

fn character_angle(radians: f32) -> Result<f32, Error> {
    if !radians.is_finite() || radians < 0.0 {
        return Err(invalid("character_style", "invalid character angle"));
    }
    let degrees = radians.to_degrees().rem_euclid(360.0);
    if !degrees.is_finite() {
        return Err(invalid("character_style", "invalid character angle"));
    }
    Ok(degrees)
}

fn ensure_link_capacity(
    output: &FixedOutput<Link>,
    maximum: u32,
    planned: u32,
) -> Result<(), Error> {
    if output.len() >= usize::try_from(planned).unwrap_or(usize::MAX) {
        return Err(invalid("links", "materialized link count exceeded preflight plan"));
    }
    if output.len() >= usize::try_from(maximum).unwrap_or(usize::MAX) {
        return Err(Error::ResourceLimit {
            limit: "max_links_per_page",
            actual: u64::try_from(output.len()).unwrap_or(u64::MAX).saturating_add(1),
            maximum: u64::from(maximum),
        });
    }
    Ok(())
}

fn push_annotation_link(
    output: &mut FixedOutput<Link>,
    bounds: PdfRect,
    target: Option<LinkTarget>,
    maximum: u32,
    planned: u32,
) -> Result<(), Error> {
    let Some(target) = target else { return Ok(()) };
    ensure_link_capacity(output, maximum, planned)?;
    output.push(Link { bounds, target }, "links")?;
    Ok(())
}

fn check_link_plan_bytes(
    current_target_bytes: u64,
    additional_target_bytes: u64,
    temporary_bytes: u64,
    plan: LinkAllocationPlan,
    operation: &'static str,
) -> Result<(), Error> {
    let target_bytes = current_target_bytes
        .checked_add(additional_target_bytes)
        .ok_or_else(|| invalid(operation, "planned target allocation overflow"))?;
    if target_bytes > plan.target_bytes || temporary_bytes > plan.maximum_temporary_bytes {
        return Err(invalid(operation, "native URI size exceeded preflight plan"));
    }
    Ok(())
}

fn fixed_clone_string(
    value: &str,
    capacity: usize,
    operation: &'static str,
) -> Result<String, Error> {
    if value.len() > capacity {
        return Err(invalid(operation, "string length exceeded fixed allocation plan"));
    }
    let mut output = zeroed_boxed_bytes(capacity, operation)?.into_vec();
    output[..value.len()].copy_from_slice(value.as_bytes());
    output.truncate(value.len());
    String::from_utf8(output).map_err(|_| invalid(operation, "string is not valid UTF-8"))
}

fn bounded_native_bytes<F>(
    operation: &'static str,
    needed: c_ulong,
    maximum: u32,
    copy: F,
) -> Result<String, Error>
where
    F: FnOnce(*mut u8, c_ulong) -> c_ulong,
{
    let needed_u64 = c_ulong_to_u64(needed);
    if needed == 0 {
        return Err(invalid(operation, "zero length"));
    }
    if needed_u64 > u64::from(maximum) {
        return Err(Error::ResourceLimit {
            limit: "max_link_bytes",
            actual: needed_u64,
            maximum: u64::from(maximum),
        });
    }
    let capacity =
        usize::try_from(needed).map_err(|_| invalid(operation, "length does not fit usize"))?;
    let mut buffer = zeroed_boxed_bytes(capacity, operation)?;
    let copied = copy(buffer.as_mut_ptr(), needed);
    if copied == 0 || copied > needed {
        return Err(invalid(operation, "native length changed or was zero"));
    }
    let mut buffer = buffer.into_vec();
    buffer.truncate(usize::try_from(copied).unwrap_or(capacity));
    if buffer.last() == Some(&0) {
        let _ = buffer.pop();
    }
    if buffer.contains(&0) {
        return Err(invalid(operation, "embedded NUL"));
    }
    String::from_utf8(buffer).map_err(|_| invalid(operation, "URI is not valid UTF-8"))
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
        let mut units = try_uninit_boxed_slice::<u16>(capacity, "text")?;
        // SAFETY: initialize the entire fixed-layout buffer before PDFium may
        // partially write it.
        unsafe { std::ptr::write_bytes(units.as_mut_ptr().cast::<u16>(), 0, capacity) };
        let copied = unsafe {
            (self.get_text)(
                text as Handle,
                0,
                c_int::try_from(requested).map_err(|_| invalid("text", "count exceeds C int"))?,
                units.as_mut_ptr().cast::<u16>(),
            )
        };
        if copied <= 0 {
            return Err(self.error("get_text"));
        }
        let copied = usize::try_from(copied)
            .map_err(|_| invalid("text", "negative copied count"))?
            .min(units.len());
        // SAFETY: the entire fixed buffer was zero-initialized before the FFI call.
        let units = unsafe { units.assume_init() };
        let content = if units.get(copied.saturating_sub(1)) == Some(&0) {
            &units[..copied.saturating_sub(1)]
        } else {
            &units[..copied]
        };
        decode_utf16(content)
    }
    fn character_count(&self, text: usize) -> Result<u32, Error> {
        nonnegative("text_count", unsafe { (self.text_count)(text as Handle) })
    }
    #[allow(clippy::too_many_lines)]
    fn characters(
        &self,
        text: usize,
        limits: Limits,
        plan: CharacterAllocationPlan,
    ) -> Result<Vec<Character>, Error> {
        let count = nonnegative("text_count", unsafe { (self.text_count)(text as Handle) })?;
        if count > limits.max_text_units_per_page {
            return Err(Error::ResourceLimit {
                limit: "max_text_units_per_page",
                actual: u64::from(count),
                maximum: u64::from(limits.max_text_units_per_page),
            });
        }
        if count != plan.count {
            return Err(invalid("characters", "native character count changed after preflight"));
        }
        // Revalidate every variable-sized font observation before reserving the
        // Character vector or copying any font bytes.
        let mut observed_font_bytes = 0_u64;
        for index in 0..count {
            let c_index =
                c_int::try_from(index).map_err(|_| invalid("characters", "index exceeds C int"))?;
            let mut flags = 0_i32;
            let needed = unsafe {
                (self.text_font_info)(
                    text as Handle,
                    c_index,
                    std::ptr::null_mut(),
                    0,
                    &raw mut flags,
                )
            };
            let needed = c_ulong_to_u64(needed);
            observed_font_bytes = observed_font_bytes
                .checked_add(needed)
                .ok_or_else(|| invalid("characters", "font allocation overflow"))?;
            if needed > u64::from(limits.max_font_name_bytes)
                || needed > plan.maximum_font_bytes
                || observed_font_bytes > plan.font_bytes
            {
                return Err(invalid("characters", "native font size exceeded preflight plan"));
            }
        }
        let capacity = usize::try_from(plan.count)
            .map_err(|_| invalid("characters", "count does not fit usize"))?;
        let mut output = FixedOutput::new(capacity, "characters")?;
        let mut font_bytes = 0_u64;
        for index in 0..count {
            let c_index =
                c_int::try_from(index).map_err(|_| invalid("characters", "index exceeds C int"))?;
            let raw = unsafe { (self.text_unicode)(text as Handle, c_index) };
            let value = char::from_u32(raw)
                .filter(|value| {
                    *value != '\0' && (!value.is_control() || matches!(value, '\n' | '\r' | '\t'))
                })
                .unwrap_or('\u{fffd}');
            let (mut left, mut right, mut bottom, mut top) = (0.0, 0.0, 0.0, 0.0);
            if unsafe {
                (self.text_char_box)(
                    text as Handle,
                    c_index,
                    &raw mut left,
                    &raw mut right,
                    &raw mut bottom,
                    &raw mut top,
                )
            } == 0
            {
                return Err(self.error("text_char_box"));
            }
            let bounds = finite_rect("text_char_box", left, bottom, right, top)?;
            let font_size = unsafe { (self.text_font_size)(text as Handle, c_index) };
            let angle_radians = unsafe { (self.text_char_angle)(text as Handle, c_index) };
            let angle_degrees = character_angle(angle_radians)?;
            if !font_size.is_finite() || font_size < 0.0 || font_size > f64::from(f32::MAX) {
                return Err(invalid("character_style", "non-finite font size or angle"));
            }
            let mut flags = 0_i32;
            let needed = unsafe {
                (self.text_font_info)(
                    text as Handle,
                    c_index,
                    std::ptr::null_mut(),
                    0,
                    &raw mut flags,
                )
            };
            let needed_bytes = c_ulong_to_u64(needed);
            let font_capacity = needed_bytes;
            if needed_bytes > u64::from(limits.max_font_name_bytes) {
                return Err(Error::ResourceLimit {
                    limit: "max_font_name_bytes",
                    actual: needed_bytes,
                    maximum: u64::from(limits.max_font_name_bytes),
                });
            }
            let next_font_bytes = font_bytes
                .checked_add(font_capacity)
                .ok_or_else(|| invalid("characters", "font allocation overflow"))?;
            if next_font_bytes > plan.font_bytes || font_capacity > plan.maximum_font_bytes {
                return Err(invalid("characters", "native font size exceeded preflight plan"));
            }
            font_bytes = next_font_bytes;
            let font_name = self.font_name_with_length(
                text,
                c_index,
                needed,
                limits.max_font_name_bytes,
                font_capacity,
            )?;
            output.push(
                Character {
                    index,
                    value,
                    bounds,
                    font_name,
                    font_size: f64_to_f32(font_size),
                    angle_degrees,
                },
                "characters",
            )?;
        }
        output.into_vec("characters")
    }
    fn character_allocation_bytes(
        &self,
        text: usize,
        limits: Limits,
    ) -> Result<CharacterAllocationPlan, Error> {
        let count = nonnegative("text_count", unsafe { (self.text_count)(text as Handle) })?;
        if count > limits.max_text_units_per_page {
            return Err(Error::ResourceLimit {
                limit: "max_text_units_per_page",
                actual: u64::from(count),
                maximum: u64::from(limits.max_text_units_per_page),
            });
        }
        let mut font_bytes = 0_u64;
        let mut maximum_font_bytes = 0_u64;
        for index in 0..count {
            let c_index = c_int::try_from(index)
                .map_err(|_| invalid("character_plan", "index exceeds C int"))?;
            let mut flags = 0_i32;
            let needed = unsafe {
                (self.text_font_info)(
                    text as Handle,
                    c_index,
                    std::ptr::null_mut(),
                    0,
                    &raw mut flags,
                )
            };
            let needed = c_ulong_to_u64(needed);
            if needed > u64::from(limits.max_font_name_bytes) {
                return Err(Error::ResourceLimit {
                    limit: "max_font_name_bytes",
                    actual: needed,
                    maximum: u64::from(limits.max_font_name_bytes),
                });
            }
            let capacity = needed;
            font_bytes = font_bytes
                .checked_add(capacity)
                .ok_or_else(|| invalid("character_plan", "font allocation overflow"))?;
            maximum_font_bytes = maximum_font_bytes.max(capacity);
        }
        let character_capacity = u64::from(count);
        let character_bytes = character_capacity
            .checked_mul(u64::try_from(std::mem::size_of::<Character>()).unwrap_or(u64::MAX))
            .ok_or_else(|| invalid("character_plan", "character allocation overflow"))?;
        let bytes = character_bytes
            .checked_add(font_bytes)
            .ok_or_else(|| invalid("character_plan", "allocation overflow"))?;
        Ok(CharacterAllocationPlan {
            bytes,
            count,
            font_bytes,
            maximum_font_bytes,
            retained_font_bytes: font_bytes,
        })
    }
    fn page_info(&self, page: usize) -> Result<PageInfo, Error> {
        let width = unsafe { (self.page_width)(page as Handle) };
        let height = unsafe { (self.page_height)(page as Handle) };
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(invalid("page_info", "invalid page dimensions"));
        }
        let quarter_turns = unsafe { (self.page_rotation)(page as Handle) };
        let rotation_degrees = match quarter_turns {
            0 => 0,
            1 => 90,
            2 => 180,
            3 => 270,
            _ => return Err(invalid("page_info", "invalid page rotation")),
        };
        Ok(PageInfo { width_points: width, height_points: height, rotation_degrees })
    }

    fn path_bounds(
        &self,
        page: usize,
        max_objects: u32,
        plan: PathBoundsAllocationPlan,
        checkpoint: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<PdfRect>, Error> {
        let count = nonnegative("object_count", unsafe { (self.object_count)(page as Handle) })?;
        if count > max_objects {
            return Err(Error::ResourceLimit {
                limit: "max_page_objects",
                actual: u64::from(count),
                maximum: u64::from(max_objects),
            });
        }
        let mut bounds =
            FixedOutput::new(usize::try_from(plan.count).unwrap_or(usize::MAX), "path_bounds")?;
        for index in 0..count {
            path_scan_checkpoint(index, "path_bounds_checkpoint", checkpoint)?;
            let object = unsafe {
                (self.get_object)(
                    page as Handle,
                    c_int::try_from(index)
                        .map_err(|_| invalid("path_bounds", "index exceeds C int"))?,
                )
            };
            if object.is_null() {
                return Err(self.error("get_object"));
            }
            if unsafe { (self.object_type)(object) } != 2 {
                continue;
            }
            if bounds.len() >= usize::try_from(plan.count).unwrap_or(usize::MAX) {
                return Err(invalid("path_bounds", "materialized count exceeded preflight plan"));
            }
            bounds.push(object_bounds(self, object, "path_bounds")?, "path_bounds")?;
        }
        if bounds.len() != usize::try_from(plan.count).unwrap_or(usize::MAX) {
            return Err(invalid("path_bounds", "materialized count changed after preflight"));
        }
        bounds.into_vec("path_bounds")
    }

    fn path_bounds_allocation_bytes(
        &self,
        page: usize,
        max_objects: u32,
        checkpoint: &mut dyn FnMut() -> bool,
    ) -> Result<PathBoundsAllocationPlan, Error> {
        let count = nonnegative("object_count", unsafe { (self.object_count)(page as Handle) })?;
        if count > max_objects {
            return Err(Error::ResourceLimit {
                limit: "max_page_objects",
                actual: u64::from(count),
                maximum: u64::from(max_objects),
            });
        }
        let mut paths = 0_u32;
        for index in 0..count {
            path_scan_checkpoint(index, "path_bounds_plan_checkpoint", checkpoint)?;
            let object = unsafe {
                (self.get_object)(
                    page as Handle,
                    c_int::try_from(index)
                        .map_err(|_| invalid("path_bounds_plan", "index exceeds C int"))?,
                )
            };
            if object.is_null() {
                return Err(self.error("get_object"));
            }
            if unsafe { (self.object_type)(object) } == 2 {
                // Validate the same bounds during preflight so allocation is
                // never authorized by a malformed or non-finite PATH object.
                object_bounds(self, object, "path_bounds_plan")?;
                paths = paths
                    .checked_add(1)
                    .ok_or_else(|| invalid("path_bounds_plan", "count overflow"))?;
            }
        }
        let bytes = u64::from(paths)
            .checked_mul(u64::try_from(std::mem::size_of::<PdfRect>()).unwrap_or(u64::MAX))
            .ok_or_else(|| invalid("path_bounds_plan", "allocation overflow"))?;
        Ok(PathBoundsAllocationPlan { bytes, count: paths })
    }
    fn page_object_count(&self, page: usize) -> Result<u32, Error> {
        nonnegative("object_count", unsafe { (self.object_count)(page as Handle) })
    }
    #[allow(clippy::too_many_lines)]
    fn links(
        &self,
        document: usize,
        page: usize,
        text: usize,
        limits: Limits,
        plan: LinkAllocationPlan,
    ) -> Result<Vec<Link>, Error> {
        let planned_links = plan.count;
        let mut target_bytes = 0_u64;
        let mut output = FixedOutput::new(
            usize::try_from(plan.vector_capacity)
                .map_err(|_| invalid("links", "planned count does not fit usize"))?,
            "links",
        )?;
        let mut position = 0_i32;
        let mut attempts = 0_u32;
        let maximum_attempts = limits.max_links_per_page.saturating_add(1);
        loop {
            if attempts >= maximum_attempts {
                return Err(Error::ResourceLimit {
                    limit: "max_links_per_page",
                    actual: u64::from(attempts).saturating_add(1),
                    maximum: u64::from(maximum_attempts),
                });
            }
            let previous_position = position;
            let mut link = std::ptr::null_mut();
            let found =
                unsafe { (self.enumerate_link)(page as Handle, &raw mut position, &raw mut link) };
            attempts = attempts.saturating_add(1);
            if found == 0 {
                break;
            }
            if position <= previous_position {
                return Err(invalid("enumerate_link", "enumeration position did not advance"));
            }
            if link.is_null() {
                return Err(invalid("enumerate_link", "null link handle"));
            }
            let mut rect = FsRectF::default();
            if unsafe { (self.link_rect)(link, &raw mut rect) } == 0 {
                return Err(self.error("link_rect"));
            }
            let bounds = finite_rect("link_rect", rect.left, rect.bottom, rect.right, rect.top)?;
            let action = unsafe { (self.link_action)(link) };
            let target = if !action.is_null() && unsafe { (self.action_type)(action) } == 3 {
                let needed = unsafe {
                    (self.action_uri)(document as Handle, action, std::ptr::null_mut(), 0)
                };
                let bytes = checked_link_length(needed, limits.max_link_bytes, "action_uri")?;
                check_link_plan_bytes(target_bytes, bytes, bytes, plan, "action_uri")?;
                target_bytes = target_bytes
                    .checked_add(bytes)
                    .ok_or_else(|| invalid("links", "target allocation overflow"))?;
                Some(LinkTarget::ExternalUri(self.action_uri_with_length(
                    document,
                    action,
                    needed,
                    limits.max_link_bytes,
                )?))
            } else {
                let destination = unsafe { (self.link_dest)(document as Handle, link) };
                if destination.is_null() {
                    None
                } else {
                    let index = unsafe { (self.dest_page_index)(document as Handle, destination) };
                    Some(LinkTarget::InternalPage {
                        page_index: nonnegative("dest_page_index", index)?,
                    })
                }
            };
            push_annotation_link(
                &mut output,
                bounds,
                target,
                limits.max_links_per_page,
                planned_links,
            )?;
        }
        let web = unsafe { (self.load_web_links)(text as Handle) };
        if web.is_null() {
            return Err(self.error("load_web_links"));
        }
        let web = WebLinksGuard { raw: web, close: self.close_web_links };
        let count = nonnegative("web_link_count", unsafe { (self.web_link_count)(web.raw) })?;
        if count > limits.max_links_per_page {
            return Err(Error::ResourceLimit {
                limit: "max_links_per_page",
                actual: u64::from(count),
                maximum: u64::from(limits.max_links_per_page),
            });
        }
        for index in 0..count {
            let rect_count = nonnegative("web_link_rect_count", unsafe {
                (self.web_link_rect_count)(
                    web.raw,
                    c_int::try_from(index)
                        .map_err(|_| invalid("web_links", "index exceeds C int"))?,
                )
            })?;
            if rect_count > limits.max_links_per_page {
                return Err(Error::ResourceLimit {
                    limit: "max_links_per_page",
                    actual: u64::from(rect_count),
                    maximum: u64::from(limits.max_links_per_page),
                });
            }
            let index_c =
                c_int::try_from(index).map_err(|_| invalid("web_uri", "index exceeds C int"))?;
            let needed = unsafe { (self.web_link_url)(web.raw, index_c, std::ptr::null_mut(), 0) };
            if needed <= 0 {
                return Err(self.error("web_uri"));
            }
            let units = u64::try_from(needed).map_err(|_| invalid("web_uri", "negative length"))?;
            let native_bytes =
                units.checked_mul(2).ok_or_else(|| invalid("web_uri", "length overflow"))?;
            if native_bytes > u64::from(limits.max_link_bytes) {
                return Err(Error::ResourceLimit {
                    limit: "max_link_bytes",
                    actual: native_bytes,
                    maximum: u64::from(limits.max_link_bytes),
                });
            }
            let utf8_bound =
                units.checked_mul(3).ok_or_else(|| invalid("web_uri", "UTF-8 bound overflow"))?;
            let retained = utf8_bound
                .checked_mul(u64::from(rect_count))
                .ok_or_else(|| invalid("links", "target allocation overflow"))?;
            let temporary = native_bytes
                .checked_add(utf8_bound)
                .ok_or_else(|| invalid("links", "temporary allocation overflow"))?;
            check_link_plan_bytes(target_bytes, retained, temporary, plan, "web_uri")?;
            target_bytes = target_bytes
                .checked_add(retained)
                .ok_or_else(|| invalid("links", "target allocation overflow"))?;
            let uri = self.web_uri_with_length(web.raw, index, needed, limits.max_link_bytes)?;
            for rect_index in 0..rect_count {
                ensure_link_capacity(&output, limits.max_links_per_page, planned_links)?;
                let (mut left, mut top, mut right, mut bottom) = (0.0, 0.0, 0.0, 0.0);
                let link_index = c_int::try_from(index)
                    .map_err(|_| invalid("web_link_rect", "link index exceeds C int"))?;
                let rect_index = c_int::try_from(rect_index)
                    .map_err(|_| invalid("web_link_rect", "rect index exceeds C int"))?;
                if unsafe {
                    (self.web_link_rect)(
                        web.raw,
                        link_index,
                        rect_index,
                        &raw mut left,
                        &raw mut top,
                        &raw mut right,
                        &raw mut bottom,
                    )
                } == 0
                {
                    return Err(self.error("web_link_rect"));
                }
                output.push(
                    Link {
                        bounds: finite_rect("web_link_rect", left, bottom, right, top)?,
                        target: LinkTarget::ExternalUri(fixed_clone_string(
                            &uri,
                            uri.capacity(),
                            "web_link_uri",
                        )?),
                    },
                    "links",
                )?;
            }
        }
        if output.len() != usize::try_from(planned_links).unwrap_or(usize::MAX) {
            return Err(invalid("links", "materialized link count changed after preflight"));
        }
        output.into_vec("links")
    }
    #[allow(clippy::too_many_lines)]
    fn link_allocation_bytes(
        &self,
        document: usize,
        page: usize,
        text: usize,
        limits: Limits,
    ) -> Result<LinkAllocationPlan, Error> {
        let mut links = 0_u64;
        let mut target_bytes = 0_u64;
        let mut maximum_temporary = 0_u64;
        let mut position = 0_i32;
        let mut attempts = 0_u32;
        let maximum_attempts = limits.max_links_per_page.saturating_add(1);
        loop {
            if attempts >= maximum_attempts {
                return Err(Error::ResourceLimit {
                    limit: "max_links_per_page",
                    actual: u64::from(attempts).saturating_add(1),
                    maximum: u64::from(maximum_attempts),
                });
            }
            let previous_position = position;
            let mut link = std::ptr::null_mut();
            let found =
                unsafe { (self.enumerate_link)(page as Handle, &raw mut position, &raw mut link) };
            attempts = attempts.saturating_add(1);
            if found == 0 {
                break;
            }
            if position <= previous_position || link.is_null() {
                return Err(invalid("enumerate_link", "invalid enumeration progress or handle"));
            }
            let action = unsafe { (self.link_action)(link) };
            if !action.is_null() && unsafe { (self.action_type)(action) } == 3 {
                let needed = unsafe {
                    (self.action_uri)(document as Handle, action, std::ptr::null_mut(), 0)
                };
                let bytes = checked_link_length(needed, limits.max_link_bytes, "action_uri")?;
                target_bytes = target_bytes
                    .checked_add(bytes)
                    .ok_or_else(|| invalid("link_plan", "allocation overflow"))?;
                maximum_temporary = maximum_temporary.max(bytes);
            } else {
                let destination = unsafe { (self.link_dest)(document as Handle, link) };
                if destination.is_null() {
                    continue;
                }
            }
            links = links.checked_add(1).ok_or_else(|| invalid("link_plan", "count overflow"))?;
            if links > u64::from(limits.max_links_per_page) {
                return Err(Error::ResourceLimit {
                    limit: "max_links_per_page",
                    actual: links,
                    maximum: u64::from(limits.max_links_per_page),
                });
            }
        }
        let web = unsafe { (self.load_web_links)(text as Handle) };
        if web.is_null() {
            return Err(self.error("load_web_links"));
        }
        let web = WebLinksGuard { raw: web, close: self.close_web_links };
        let count = nonnegative("web_link_count", unsafe { (self.web_link_count)(web.raw) })?;
        if count > limits.max_links_per_page {
            return Err(Error::ResourceLimit {
                limit: "max_links_per_page",
                actual: u64::from(count),
                maximum: u64::from(limits.max_links_per_page),
            });
        }
        for index in 0..count {
            let index_c =
                c_int::try_from(index).map_err(|_| invalid("web_uri", "index exceeds C int"))?;
            let needed = unsafe { (self.web_link_url)(web.raw, index_c, std::ptr::null_mut(), 0) };
            if needed <= 0 {
                return Err(self.error("web_uri"));
            }
            let units = u64::try_from(needed).map_err(|_| invalid("web_uri", "negative length"))?;
            let native_bytes =
                units.checked_mul(2).ok_or_else(|| invalid("web_uri", "length overflow"))?;
            if native_bytes > u64::from(limits.max_link_bytes) {
                return Err(Error::ResourceLimit {
                    limit: "max_link_bytes",
                    actual: native_bytes,
                    maximum: u64::from(limits.max_link_bytes),
                });
            }
            let utf8_bound =
                units.checked_mul(3).ok_or_else(|| invalid("web_uri", "UTF-8 bound overflow"))?;
            maximum_temporary = maximum_temporary.max(
                native_bytes
                    .checked_add(utf8_bound)
                    .ok_or_else(|| invalid("link_plan", "allocation overflow"))?,
            );
            let rects = nonnegative("web_link_rect_count", unsafe {
                (self.web_link_rect_count)(web.raw, index_c)
            })?;
            if rects > limits.max_links_per_page {
                return Err(Error::ResourceLimit {
                    limit: "max_links_per_page",
                    actual: u64::from(rects),
                    maximum: u64::from(limits.max_links_per_page),
                });
            }
            links = links
                .checked_add(u64::from(rects))
                .ok_or_else(|| invalid("link_plan", "count overflow"))?;
            if links > u64::from(limits.max_links_per_page) {
                return Err(Error::ResourceLimit {
                    limit: "max_links_per_page",
                    actual: links,
                    maximum: u64::from(limits.max_links_per_page),
                });
            }
            target_bytes = target_bytes
                .checked_add(
                    utf8_bound
                        .checked_mul(u64::from(rects))
                        .ok_or_else(|| invalid("link_plan", "allocation overflow"))?,
                )
                .ok_or_else(|| invalid("link_plan", "allocation overflow"))?;
        }
        let link_capacity = links;
        let bytes = link_capacity
            .checked_mul(u64::try_from(std::mem::size_of::<Link>()).unwrap_or(u64::MAX))
            .and_then(|value| value.checked_add(target_bytes))
            .and_then(|value| value.checked_add(maximum_temporary))
            .ok_or_else(|| invalid("link_plan", "allocation overflow"))?;
        Ok(LinkAllocationPlan {
            bytes,
            count: u32::try_from(links)
                .map_err(|_| invalid("link_plan", "count does not fit u32"))?,
            target_bytes,
            maximum_temporary_bytes: maximum_temporary,
            vector_capacity: link_capacity,
        })
    }
    fn image_objects(
        &self,
        page: usize,
        max_objects: u32,
        max_images: u32,
        plan: ImageAllocationPlan,
    ) -> Result<Vec<ImageObject>, Error> {
        let planned_images = plan.count;
        let count = nonnegative("object_count", unsafe { (self.object_count)(page as Handle) })?;
        if count > max_objects {
            return Err(Error::ResourceLimit {
                limit: "max_page_objects",
                actual: u64::from(count),
                maximum: u64::from(max_objects),
            });
        }
        if planned_images > max_images {
            return Err(Error::ResourceLimit {
                limit: "max_images_per_page",
                actual: u64::from(planned_images),
                maximum: u64::from(max_images),
            });
        }
        let mut images = image_output(planned_images, max_images)?;
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
                if images.len() >= usize::try_from(planned_images).unwrap_or(usize::MAX) {
                    return Err(invalid(
                        "image_objects",
                        "materialized count exceeded preflight plan",
                    ));
                }
                let (mut left, mut bottom, mut right, mut top) = (0.0, 0.0, 0.0, 0.0);
                if unsafe {
                    (self.object_bounds)(
                        object,
                        &raw mut left,
                        &raw mut bottom,
                        &raw mut right,
                        &raw mut top,
                    )
                } == 0
                {
                    return Err(self.error("image_bounds"));
                }
                images.push(
                    ImageObject {
                        raw: object as usize,
                        bounds: finite_rect("image_bounds", left, bottom, right, top)?,
                    },
                    "image_objects",
                )?;
            }
        }
        if images.len() != usize::try_from(planned_images).unwrap_or(usize::MAX) {
            return Err(invalid("image_objects", "materialized count changed after preflight"));
        }
        images.into_vec("image_objects")
    }
    fn image_object_allocation_bytes(
        &self,
        page: usize,
        max_objects: u32,
        max_images: u32,
    ) -> Result<ImageAllocationPlan, Error> {
        let count = nonnegative("object_count", unsafe { (self.object_count)(page as Handle) })?;
        if count > max_objects {
            return Err(Error::ResourceLimit {
                limit: "max_page_objects",
                actual: u64::from(count),
                maximum: u64::from(max_objects),
            });
        }
        let mut images = 0_u64;
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
                images =
                    images.checked_add(1).ok_or_else(|| invalid("image_plan", "count overflow"))?;
                if images > u64::from(max_images) {
                    return Err(Error::ResourceLimit {
                        limit: "max_images_per_page",
                        actual: images,
                        maximum: u64::from(max_images),
                    });
                }
            }
        }
        let width = u64::try_from(
            std::mem::size_of::<ImageObject>() + std::mem::size_of::<crate::Image<'_>>(),
        )
        .unwrap_or(u64::MAX);
        let capacity = images;
        Ok(ImageAllocationPlan {
            bytes: capacity
                .checked_mul(width)
                .ok_or_else(|| invalid("image_plan", "allocation overflow"))?,
            count: u32::try_from(images)
                .map_err(|_| invalid("image_plan", "count does not fit u32"))?,
        })
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
        let mut bytes = zeroed_boxed_bytes(capacity, "render")?;
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
        Ok(bytes.into_vec())
    }
    fn image_bitmap(
        &self,
        image: usize,
        limits: Limits,
        planned_bytes: u64,
    ) -> Result<ImageBitmap, Error> {
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
        let output_bound = planned_bytes / 2;
        if size > output_bound {
            return Err(invalid("image_bitmap", "decoded size exceeded preflight plan"));
        }
        let capacity = usize::try_from(size)
            .map_err(|_| invalid("image_bitmap", "buffer does not fit usize"))?;
        let source = unsafe { (self.bitmap_buffer)(bitmap.raw) }.cast::<u8>();
        if source.is_null() && capacity != 0 {
            return Err(self.error("image_bitmap_buffer"));
        }
        let mut bytes = zeroed_boxed_bytes(capacity, "image_bitmap")?;
        if capacity != 0 {
            // SAFETY: PDFium reports `stride * height` bytes owned by `bitmap`; the bitmap remains
            // alive through this copy, and all arithmetic/allocation was checked above.
            bytes.copy_from_slice(unsafe { std::slice::from_raw_parts(source, capacity) });
        }
        Ok(ImageBitmap { width, height, stride, format, bytes: bytes.into_vec() })
    }
    fn image_bitmap_allocation_bytes(&self, image: usize, limits: Limits) -> Result<u64, Error> {
        let (mut width, mut height) = (0_u32, 0_u32);
        if unsafe { (self.image_pixel_size)(image as Handle, &raw mut width, &raw mut height) } == 0
        {
            return Err(self.error("image_pixel_size"));
        }
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
            .ok_or_else(|| invalid("image_plan", "pixel count overflow"))?;
        if pixels > limits.max_render_pixels {
            return Err(Error::ResourceLimit {
                limit: "max_render_pixels",
                actual: pixels,
                maximum: limits.max_render_pixels,
            });
        }
        let bytes = pixels
            .checked_mul(4)
            .ok_or_else(|| invalid("image_plan", "bitmap byte count overflow"))?;
        if bytes > limits.max_bitmap_bytes {
            return Err(Error::ResourceLimit {
                limit: "max_bitmap_bytes",
                actual: bytes,
                maximum: limits.max_bitmap_bytes,
            });
        }
        bytes.checked_mul(2).ok_or_else(|| invalid("image_plan", "bitmap peak overflow"))
    }
}

fn checked_link_length(
    needed: c_ulong,
    maximum: u32,
    operation: &'static str,
) -> Result<u64, Error> {
    let bytes = c_ulong_to_u64(needed);
    if needed == 0 {
        return Err(invalid(operation, "zero length"));
    }
    if bytes > u64::from(maximum) {
        return Err(Error::ResourceLimit {
            limit: "max_link_bytes",
            actual: bytes,
            maximum: u64::from(maximum),
        });
    }
    Ok(bytes)
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
    let mut text = zeroed_boxed_bytes(capacity, "text_utf8")?.into_vec();
    let mut written = 0_usize;
    for character in char::decode_utf16(units.iter().copied()) {
        let character = character.map_err(|_| invalid("text", "invalid UTF-16 surrogate"))?;
        let mut encoded = [0_u8; 4];
        let value = character.encode_utf8(&mut encoded).as_bytes();
        let end = written
            .checked_add(value.len())
            .ok_or_else(|| invalid("text_utf8", "UTF-8 length overflow"))?;
        if end > text.len() {
            return Err(invalid("text_utf8", "UTF-8 output exceeded fixed allocation plan"));
        }
        text[written..end].copy_from_slice(value);
        written = end;
    }
    text.truncate(written);
    String::from_utf8(text).map_err(|_| invalid("text_utf8", "decoded text is not UTF-8"))
}

fn invalid(operation: &'static str, detail: &str) -> Error {
    Error::InvalidResult { operation, detail: detail.into() }
}
fn image_output(count: u32, maximum: u32) -> Result<FixedOutput<ImageObject>, Error> {
    if count > maximum {
        return Err(Error::ResourceLimit {
            limit: "max_images_per_page",
            actual: u64::from(count),
            maximum: u64::from(maximum),
        });
    }
    FixedOutput::new(usize::try_from(count).unwrap_or(usize::MAX), "image_objects")
}
fn nonnegative(operation: &'static str, value: c_int) -> Result<u32, Error> {
    u32::try_from(value).map_err(|_| invalid(operation, "native count was negative"))
}

struct Snapshot {
    directory: tempfile::TempDir,
    path: PathBuf,
    file: Option<File>,
    bytes: Vec<u8>,
    #[cfg(unix)]
    load_by_path: bool,
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
        if self.load_by_path {
            return self.path.clone();
        }
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

#[cfg_attr(windows, allow(clippy::permissions_set_readonly_false))]
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

#[allow(clippy::too_many_lines)] // Keep the security-sensitive open, hash, copy, and lock sequence linear.
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
    let signed_authority = if opened_metadata.len() == artifact.library_size {
        None
    } else {
        signed_derivative_authority(&canonical, artifact)
    };
    let expected_size =
        signed_authority.as_ref().map_or(artifact.library_size, |authority| authority.bytes);
    if opened_metadata.len() != expected_size {
        return Err(Error::ResourceLimit {
            limit: "pdfium_runtime_bytes",
            actual: opened_metadata.len(),
            maximum: expected_size,
        });
    }
    after_metadata();
    let bounded = expected_size
        .checked_add(1)
        .ok_or_else(|| Error::BinaryValidation("runtime size bound overflowed".into()))?;
    if bounded > MAX_RUNTIME_LIBRARY_BYTES + 1 {
        return Err(Error::ResourceLimit {
            limit: "pdfium_runtime_bytes",
            actual: expected_size,
            maximum: MAX_RUNTIME_LIBRARY_BYTES,
        });
    }
    let capacity = usize::try_from(bounded).map_err(|_| Error::ResourceLimit {
        limit: "pdfium_runtime_bytes",
        actual: bounded,
        maximum: expected_size,
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| Error::Allocation { operation: "pdfium_runtime_snapshot", bytes: bounded })?;
    std::io::Read::by_ref(&mut file)
        .take(bounded)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::InvalidPath(error.to_string()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != expected_size {
        return Err(Error::ResourceLimit {
            limit: "pdfium_runtime_bytes",
            actual: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            maximum: expected_size,
        });
    }
    let actual = format!("{:x}", Sha256::digest(&bytes));
    let digest_matches = actual == artifact.library_sha256
        || signed_authority.as_ref().is_some_and(|authority| {
            actual == authority.sha256 && verify_platform_signature(&canonical)
        });
    if !digest_matches {
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
    Ok(Snapshot {
        directory,
        path: snapshot_path,
        file: Some(snapshot_file),
        bytes,
        #[cfg(unix)]
        load_by_path: signed_authority.is_some(),
    })
}

#[cfg(target_os = "macos")]
fn signed_derivative_authority(
    path: &Path,
    artifact: &Artifact,
) -> Option<SignedDerivativeAuthority> {
    let root = path.parent()?.parent()?.parent()?;
    let manifest_path = root.join("archive-manifest.json");
    let metadata = fs::symlink_metadata(&manifest_path).ok()?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_RELEASE_PROJECTION_BYTES
    {
        return None;
    }
    let projection: ReleaseProjection =
        serde_json::from_slice(&fs::read(manifest_path).ok()?).ok()?;
    let relative = path.strip_prefix(root).ok()?.to_str()?;
    if projection.schema_version != 1 || projection.target != "aarch64-apple-darwin" {
        return None;
    }
    let mut files = projection.files.iter().filter(|file| {
        file.path == relative
            && file.kind == "component"
            && file.component_id.as_deref() == Some("pdfium")
    });
    let file = files.next()?;
    if files.next().is_some() {
        return None;
    }
    let mut transformations = projection.native_transformations.iter().filter(|item| {
        item.component_id == "pdfium"
            && item.path == relative
            && item.kind == "apple-code-sign"
            && item.source_bytes == artifact.library_size
            && item.source_sha256 == artifact.library_sha256
            && item.output_bytes == file.bytes
            && item.output_sha256 == file.sha256
    });
    let transformation = transformations.next()?;
    if transformations.next().is_some()
        || transformation.output_bytes > MAX_RUNTIME_LIBRARY_BYTES
        || transformation.output_sha256.len() != 64
        || !transformation.output_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(SignedDerivativeAuthority {
        bytes: transformation.output_bytes,
        sha256: transformation.output_sha256.clone(),
    })
}

#[cfg(not(target_os = "macos"))]
fn signed_derivative_authority(
    _path: &Path,
    _artifact: &Artifact,
) -> Option<SignedDerivativeAuthority> {
    None
}

#[cfg(target_os = "macos")]
fn verify_platform_signature(path: &Path) -> bool {
    use std::process::{Command, Stdio};
    Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", "--verbose=0"])
        .arg(path)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(target_os = "macos"))]
fn verify_platform_signature(_path: &Path) -> bool {
    false
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
    let (format, architecture) = expected_binary_identity(platform);
    if !binary_identity_matches(platform, file.format(), file.architecture(), file.is_64()) {
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
    for library in dynamic_dependencies(bytes, &file, platform)? {
        if !artifact.allowed_dependencies.iter().any(|item| library == item.to_ascii_lowercase()) {
            return Err(Error::BinaryValidation(format!(
                "unreviewed dynamic dependency {library}"
            )));
        }
    }
    Ok(())
}

fn dynamic_dependencies(
    bytes: &[u8],
    file: &object::File<'_>,
    platform: Platform,
) -> Result<BTreeSet<String>, Error> {
    if matches!(platform, Platform::LinuxX64 | Platform::LinuxArm64) {
        let object::File::Elf64(elf) = file else {
            return Err(Error::BinaryValidation("expected ELF64 dependency table".into()));
        };
        let endian = elf.endian();
        let sections = elf.elf_section_table();
        let Some((dynamic, string_index)) = sections
            .dynamic(endian, bytes)
            .map_err(|error| Error::BinaryValidation(error.to_string()))?
        else {
            return Err(Error::BinaryValidation("missing ELF dynamic dependency table".into()));
        };
        let strings = sections
            .strings(endian, bytes, string_index)
            .map_err(|error| Error::BinaryValidation(error.to_string()))?;
        let mut libraries = BTreeSet::new();
        for entry in dynamic {
            // ELF DT_NEEDED is the stable ABI tag value 1.
            if entry.tag32(endian) != Some(1) {
                continue;
            }
            let name = entry
                .string(endian, strings)
                .map_err(|error| Error::BinaryValidation(error.to_string()))?;
            let name = std::str::from_utf8(name)
                .map_err(|error| Error::BinaryValidation(error.to_string()))?;
            if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_control()) {
                return Err(Error::BinaryValidation("invalid ELF dynamic dependency name".into()));
            }
            libraries.insert(name.to_ascii_lowercase());
        }
        if libraries.is_empty() {
            return Err(Error::BinaryValidation("empty ELF dynamic dependency table".into()));
        }
        return Ok(libraries);
    }

    let imports = file.imports().map_err(|error| Error::BinaryValidation(error.to_string()))?;
    let mut libraries = BTreeSet::new();
    for import in imports {
        let import = import.map_err(|error| Error::BinaryValidation(error.to_string()))?;
        let library = std::str::from_utf8(import.library())
            .map_err(|error| Error::BinaryValidation(error.to_string()))?;
        if library.is_empty() || library.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(Error::BinaryValidation("invalid dynamic dependency name".into()));
        }
        libraries.insert(library.to_ascii_lowercase());
    }
    if libraries.is_empty() {
        return Err(Error::BinaryValidation("empty dynamic dependency table".into()));
    }
    Ok(libraries)
}

fn expected_binary_identity(platform: Platform) -> (BinaryFormat, Architecture) {
    match platform {
        Platform::MacArm64 => (BinaryFormat::MachO, Architecture::Aarch64),
        Platform::LinuxX64 => (BinaryFormat::Elf, Architecture::X86_64),
        Platform::LinuxArm64 => (BinaryFormat::Elf, Architecture::Aarch64),
        Platform::WindowsX64 => (BinaryFormat::Pe, Architecture::X86_64),
        Platform::WindowsArm64 => (BinaryFormat::Pe, Architecture::Aarch64),
    }
}

fn binary_identity_matches(
    platform: Platform,
    actual_format: BinaryFormat,
    actual_architecture: Architecture,
    is_64: bool,
) -> bool {
    let (format, architecture) = expected_binary_identity(platform);
    actual_format == format && actual_architecture == architecture && is_64
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn unicode_signature(_: Handle, _: c_int) -> c_uint {
        0
    }
    unsafe extern "C" fn action_type_signature(_: Handle) -> c_ulong {
        0
    }

    #[test]
    fn library_config_exactly_matches_chromium_7999_version_2_prefix() {
        assert_eq!(usize::BITS, 64, "all supported PDFium targets are 64-bit");
        let config = Config::version_2();
        assert_eq!(config.version, 2);
        assert_eq!(std::mem::size_of::<Config>(), 32);
        assert_eq!(std::mem::align_of::<Config>(), 8);
        assert_eq!(std::mem::size_of::<c_uint>(), 4);
        assert_eq!(std::mem::size_of::<c_ulong>(), if cfg!(windows) { 4 } else { 8 });
        let _: GetUnicode = unicode_signature;
        let _: GetActionType = action_type_signature;
        assert_eq!(std::mem::offset_of!(Config, version), 0);
        assert_eq!(std::mem::offset_of!(Config, user_font_paths), 8);
        assert_eq!(std::mem::offset_of!(Config, isolate), 16);
        assert_eq!(std::mem::offset_of!(Config, slot), 24);
    }

    #[test]
    fn manifest_is_strict_and_exactly_binds_consumed_ffi() {
        for platform in [
            Platform::MacArm64,
            Platform::LinuxX64,
            Platform::LinuxArm64,
            Platform::WindowsX64,
            Platform::WindowsArm64,
        ] {
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
    fn windows_runtime_requires_a_64_bit_x86_64_pe_image() {
        assert!(binary_identity_matches(
            Platform::WindowsX64,
            BinaryFormat::Pe,
            Architecture::X86_64,
            true
        ));
        assert!(!binary_identity_matches(
            Platform::WindowsX64,
            BinaryFormat::Coff,
            Architecture::X86_64,
            true
        ));
        assert!(!binary_identity_matches(
            Platform::WindowsX64,
            BinaryFormat::Pe,
            Architecture::I386,
            false
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
        assert_eq!(images.slots.len(), 0);
    }

    #[test]
    fn image_materialization_reserves_the_prescanned_image_count_not_all_objects() {
        let images = image_output(1, 1_000_000).unwrap();
        assert_eq!(images.slots.len(), 1);
    }

    #[test]
    fn changed_uri_length_is_rejected_before_materialization_copy() {
        let plan = LinkAllocationPlan {
            bytes: 128,
            count: 1,
            target_bytes: 8,
            maximum_temporary_bytes: 8,
            vector_capacity: 1,
        };
        let copies = std::cell::Cell::new(0_u32);
        let result = check_link_plan_bytes(0, 64, 64, plan, "action_uri").map(|()| {
            copies.set(copies.get() + 1);
            String::from("materialized")
        });
        assert!(matches!(result, Err(Error::InvalidResult { operation: "action_uri", .. })));
        assert_eq!(copies.get(), 0);

        check_link_plan_bytes(0, 8, 8, plan, "action_uri").unwrap();
    }

    #[test]
    fn null_destination_annotation_does_not_consume_planned_capacity() {
        let mut output = FixedOutput::new(0, "links").unwrap();
        push_annotation_link(&mut output, PdfRect::default(), None, 1, 0).unwrap();
        assert_eq!(output.len(), 0);
    }

    #[test]
    fn utf16_conversion_is_bounded_and_rejects_invalid_surrogates() {
        assert_eq!(decode_utf16(&[u16::from(b'O'), u16::from(b'K')]).unwrap(), "OK");
        assert!(matches!(
            decode_utf16(&[0xd800]),
            Err(Error::InvalidResult { operation: "text", .. })
        ));
    }

    #[test]
    fn character_angle_rejects_pdfium_error_sentinel_and_normalizes() {
        assert!(character_angle(-1.0).is_err());
        assert!(character_angle(f32::INFINITY).is_err());
        let angle = character_angle(std::f32::consts::TAU + 0.25).unwrap();
        assert!((0.0..360.0).contains(&angle));
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

//! Fail-closed `PDFium` boundary.
#![allow(missing_docs)]
#![allow(clippy::missing_errors_doc)]
mod native;

use std::ffi::{CStr, CString};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

pub use native::{Artifact, Platform};

const PATH_SCAN_CHECKPOINT_OBJECTS: u32 = 256;

/// Fallibly allocate an exact-layout zeroed byte buffer without allocator
/// capacity slack. This is shared with the PDF converter's asset encoder.
#[doc(hidden)]
pub fn fixed_zeroed_bytes(length: usize) -> Result<Vec<u8>, Error> {
    native::zeroed_boxed_bytes(length, "fixed_bytes").map(<[u8]>::into_vec)
}

/// Fallibly clone a UTF-8 string into exact-layout backing storage.
#[doc(hidden)]
pub fn fixed_string(value: &str) -> Result<String, Error> {
    native::fixed_string(value, "fixed_string")
}
pub fn pdfium_version() -> Result<String, Error> {
    native::version()
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("unsupported PDFium platform: {os}/{arch}")]
    UnsupportedPlatform { os: String, arch: String },
    #[error("invalid PDFium runtime path: {0}")]
    InvalidPath(String),
    #[error("PDFium artifact digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("PDFium binary validation failed: {0}")]
    BinaryValidation(String),
    #[error("failed to load PDFium runtime: {0}")]
    Load(String),
    #[error("PDFium operation {operation} failed (native error {code})")]
    Native { operation: &'static str, code: u32 },
    #[error("PDFium resource limit {limit} exceeded: {actual} > {maximum}")]
    ResourceLimit { limit: &'static str, actual: u64, maximum: u64 },
    #[error("invalid PDFium result for {operation}: {detail}")]
    InvalidResult { operation: &'static str, detail: String },
    #[error("PDFium runtime lock is poisoned")]
    Poisoned,
    #[error("PDFium allocation for {operation} failed ({bytes} bytes)")]
    Allocation { operation: &'static str, bytes: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_document_bytes: u64,
    pub max_pages: u32,
    pub max_text_units_per_page: u32,
    pub max_render_dimension: u32,
    pub max_render_pixels: u64,
    pub max_images_per_page: u32,
    pub max_page_objects: u32,
    pub max_password_bytes: u32,
    pub max_bitmap_bytes: u64,
    pub max_links_per_page: u32,
    pub max_link_bytes: u32,
    pub max_font_name_bytes: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_document_bytes: 256 * 1024 * 1024,
            max_pages: 10_000,
            max_text_units_per_page: 16 * 1024 * 1024,
            max_render_dimension: 16_384,
            max_render_pixels: 100_000_000,
            max_images_per_page: 100_000,
            max_page_objects: 1_000_000,
            max_password_bytes: 1024,
            max_bitmap_bytes: 400_000_000,
            max_links_per_page: 100_000,
            max_link_bytes: 64 * 1024,
            max_font_name_bytes: 4 * 1024,
        }
    }
}

impl Limits {
    fn validate(self) -> Result<Self, Error> {
        for (name, actual, maximum) in [
            ("max_document_bytes", self.max_document_bytes, 1024 * 1024 * 1024),
            ("max_pages", u64::from(self.max_pages), 1_000_000),
            ("max_text_units_per_page", u64::from(self.max_text_units_per_page), 64 * 1024 * 1024),
            (
                "max_render_dimension",
                u64::from(self.max_render_dimension),
                u64::try_from(i32::MAX).unwrap_or(u64::MAX),
            ),
            ("max_render_pixels", self.max_render_pixels, 400_000_000),
            ("max_images_per_page", u64::from(self.max_images_per_page), 1_000_000),
            ("max_page_objects", u64::from(self.max_page_objects), 10_000_000),
            ("max_password_bytes", u64::from(self.max_password_bytes), 64 * 1024),
            ("max_bitmap_bytes", self.max_bitmap_bytes, 1_600_000_000),
            ("max_links_per_page", u64::from(self.max_links_per_page), 1_000_000),
            ("max_link_bytes", u64::from(self.max_link_bytes), 1024 * 1024),
            ("max_font_name_bytes", u64::from(self.max_font_name_bytes), 64 * 1024),
        ] {
            if actual > maximum {
                return Err(Error::ResourceLimit { limit: name, actual, maximum });
            }
        }
        Ok(self)
    }
}

trait Backend: Send + Sync {
    fn load_document(&self, bytes: &[u8], password: Option<&CStr>) -> Result<usize, Error>;
    fn close_document(&self, document: usize);
    fn page_count(&self, document: usize) -> Result<u32, Error>;
    fn load_page(&self, document: usize, index: u32) -> Result<usize, Error>;
    fn close_page(&self, page: usize);
    fn load_text_page(&self, page: usize) -> Result<usize, Error>;
    fn close_text_page(&self, text: usize);
    fn text(&self, text: usize, max_units: u32) -> Result<String, Error>;
    fn character_count(&self, text: usize) -> Result<u32, Error>;
    fn characters(
        &self,
        text: usize,
        limits: Limits,
        plan: CharacterAllocationPlan,
    ) -> Result<Vec<Character>, Error>;
    fn character_allocation_bytes(
        &self,
        text: usize,
        limits: Limits,
    ) -> Result<CharacterAllocationPlan, Error>;
    fn page_info(&self, page: usize) -> Result<PageInfo, Error>;
    fn page_object_count(&self, page: usize) -> Result<u32, Error>;
    fn path_bounds(
        &self,
        page: usize,
        max_objects: u32,
        plan: PathBoundsAllocationPlan,
        checkpoint: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<PdfRect>, Error>;
    fn path_bounds_allocation_bytes(
        &self,
        page: usize,
        max_objects: u32,
        checkpoint: &mut dyn FnMut() -> bool,
    ) -> Result<PathBoundsAllocationPlan, Error>;
    fn links(
        &self,
        document: usize,
        page: usize,
        text: usize,
        limits: Limits,
        plan: LinkAllocationPlan,
    ) -> Result<Vec<Link>, Error>;
    fn link_allocation_bytes(
        &self,
        document: usize,
        page: usize,
        text: usize,
        limits: Limits,
    ) -> Result<LinkAllocationPlan, Error>;
    fn image_objects(
        &self,
        page: usize,
        max_objects: u32,
        max_images: u32,
        plan: ImageAllocationPlan,
    ) -> Result<Vec<ImageObject>, Error>;
    fn image_object_allocation_bytes(
        &self,
        page: usize,
        max_objects: u32,
        max_images: u32,
    ) -> Result<ImageAllocationPlan, Error>;
    fn render(&self, page: usize, width: u32, height: u32) -> Result<Vec<u8>, Error>;
    fn image_bitmap(
        &self,
        image: usize,
        limits: Limits,
        planned_bytes: u64,
    ) -> Result<ImageBitmap, Error>;
    fn image_bitmap_allocation_bytes(&self, image: usize, limits: Limits) -> Result<u64, Error>;
}

struct Inner {
    backend: Box<dyn Backend>,
    limits: Limits,
    gate: Mutex<()>,
}
impl Inner {
    fn lock(&self) -> Result<MutexGuard<'_, ()>, Error> {
        self.gate.lock().map_err(|_| Error::Poisoned)
    }
}

#[derive(Clone)]
pub struct Pdfium(Arc<Inner>);
impl std::fmt::Debug for Pdfium {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pdfium").field("limits", &self.0.limits).finish_non_exhaustive()
    }
}

impl Pdfium {
    pub fn load_pinned(path: &Path, limits: Limits) -> Result<Self, Error> {
        let limits = limits.validate()?;
        let backend = native::Native::load(path)?;
        Ok(Self(Arc::new(Inner { backend: Box::new(backend), limits, gate: Mutex::new(()) })))
    }
    pub fn open<'runtime>(
        &'runtime self,
        bytes: Arc<[u8]>,
        password: Option<&str>,
    ) -> Result<Document<'runtime>, Error> {
        self.plan_open(bytes, password)?.materialize()
    }

    /// Validate a document-open request without allocating its password copy.
    ///
    /// Callers with an `ExecutionContext` can reserve
    /// [`PlannedDocument::allocation_bytes`] before consuming the plan. The
    /// bound covers the exact Rust-owned NUL-terminated password buffer; the
    /// input is retained by `Arc` and is not copied. `PDFium`'s own in-process
    /// parser allocations remain native residual risk and require an OS memory
    /// limit for hostile deployments.
    pub fn plan_open<'runtime, 'password>(
        &'runtime self,
        bytes: Arc<[u8]>,
        password: Option<&'password str>,
    ) -> Result<PlannedDocument<'runtime, 'password>, Error> {
        limit("max_document_bytes", bytes.len(), self.0.limits.max_document_bytes)?;
        let password_bytes = password_allocation_bytes(password, self.0.limits.max_password_bytes)?;
        Ok(PlannedDocument { runtime: self, bytes, password, password_bytes })
    }

    fn materialize_document<'runtime>(
        &'runtime self,
        bytes: Arc<[u8]>,
        password: Option<&str>,
    ) -> Result<Document<'runtime>, Error> {
        let password = bounded_password(password, self.0.limits.max_password_bytes)?;
        let _guard = self.0.lock()?;
        let raw = self.0.backend.load_document(&bytes, password.as_deref())?;
        let count = match self.0.backend.page_count(raw) {
            Ok(count) if count <= self.0.limits.max_pages => count,
            Ok(count) => {
                self.0.backend.close_document(raw);
                return Err(Error::ResourceLimit {
                    limit: "max_pages",
                    actual: u64::from(count),
                    maximum: u64::from(self.0.limits.max_pages),
                });
            }
            Err(error) => {
                self.0.backend.close_document(raw);
                return Err(error);
            }
        };
        Ok(Document { runtime: self, raw, bytes, page_count: count })
    }
}

/// Validated, allocation-free description of a PDF document open operation.
#[must_use = "reserve allocation_bytes before materializing the document"]
pub struct PlannedDocument<'runtime, 'password> {
    runtime: &'runtime Pdfium,
    bytes: Arc<[u8]>,
    password: Option<&'password str>,
    password_bytes: u64,
}

impl<'runtime> PlannedDocument<'runtime, '_> {
    /// Exact Rust allocation required for the temporary password buffer.
    #[must_use]
    pub const fn allocation_bytes(&self) -> u64 {
        self.password_bytes
    }

    /// Consume the validated plan and open the document.
    pub fn materialize(self) -> Result<Document<'runtime>, Error> {
        self.runtime.materialize_document(self.bytes, self.password)
    }
}

pub struct Document<'runtime> {
    runtime: &'runtime Pdfium,
    raw: usize,
    #[allow(dead_code)]
    bytes: Arc<[u8]>,
    page_count: u32,
}
impl Document<'_> {
    #[must_use]
    pub const fn page_count(&self) -> u32 {
        self.page_count
    }
    pub fn page(&self, index: u32) -> Result<Page<'_>, Error> {
        if index >= self.page_count {
            return Err(Error::InvalidResult {
                operation: "load_page",
                detail: format!("page index {index} is outside 0..{}", self.page_count),
            });
        }
        let _guard = self.runtime.0.lock()?;
        Ok(Page { document: self, raw: self.runtime.0.backend.load_page(self.raw, index)? })
    }
}
impl Drop for Document<'_> {
    fn drop(&mut self) {
        if let Ok(_guard) = self.runtime.0.gate.lock() {
            self.runtime.0.backend.close_document(self.raw);
        }
    }
}

pub struct Page<'document> {
    document: &'document Document<'document>,
    raw: usize,
}
impl Page<'_> {
    pub fn info(&self) -> Result<PageInfo, Error> {
        let _guard = self.document.runtime.0.lock()?;
        self.document.runtime.0.backend.page_info(self.raw)
    }

    pub fn object_count(&self) -> Result<u32, Error> {
        let _guard = self.document.runtime.0.lock()?;
        self.document.runtime.0.backend.page_object_count(self.raw)
    }

    pub fn text_page(&self) -> Result<TextPage<'_>, Error> {
        let _guard = self.document.runtime.0.lock()?;
        Ok(TextPage { page: self, raw: self.document.runtime.0.backend.load_text_page(self.raw)? })
    }

    /// Plan a bounded snapshot of finite PDF PATH object bounds on this page.
    pub fn plan_path_bounds(&self) -> Result<PlannedPathBounds<'_, '_>, Error> {
        self.plan_path_bounds_with_checkpoint(&mut || true)
    }

    /// Plan PATH bounds while invoking a caller-owned checkpoint once per
    /// fixed object batch. Returning `false` interrupts the scan.
    pub fn plan_path_bounds_with_checkpoint(
        &self,
        checkpoint: &mut dyn FnMut() -> bool,
    ) -> Result<PlannedPathBounds<'_, '_>, Error> {
        let limits = self.document.runtime.0.limits;
        let _guard = self.document.runtime.0.lock()?;
        let plan = self.document.runtime.0.backend.path_bounds_allocation_bytes(
            self.raw,
            limits.max_page_objects,
            checkpoint,
        )?;
        Ok(PlannedPathBounds { page: self, plan })
    }

    fn materialize_path_bounds(
        &self,
        plan: PathBoundsAllocationPlan,
        checkpoint: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<PdfRect>, Error> {
        let limits = self.document.runtime.0.limits;
        let _guard = self.document.runtime.0.lock()?;
        let bounds = self.document.runtime.0.backend.path_bounds(
            self.raw,
            limits.max_page_objects,
            plan,
            checkpoint,
        )?;
        if bounds.len() != usize::try_from(plan.count).unwrap_or(usize::MAX) {
            return Err(Error::InvalidResult {
                operation: "path_bounds",
                detail: "materialized count changed after preflight".into(),
            });
        }
        Ok(bounds)
    }
    pub fn images(&self) -> Result<Vec<Image<'_>>, Error> {
        self.plan_images()?.materialize()
    }

    pub fn plan_images(&self) -> Result<PlannedImages<'_, '_>, Error> {
        let limits = self.document.runtime.0.limits;
        let _guard = self.document.runtime.0.lock()?;
        let plan = self.document.runtime.0.backend.image_object_allocation_bytes(
            self.raw,
            limits.max_page_objects,
            limits.max_images_per_page,
        )?;
        Ok(PlannedImages { page: self, plan })
    }

    fn materialize_images(&self, plan: ImageAllocationPlan) -> Result<Vec<Image<'_>>, Error> {
        let limits = self.document.runtime.0.limits;
        let _guard = self.document.runtime.0.lock()?;
        let objects = self.document.runtime.0.backend.image_objects(
            self.raw,
            limits.max_page_objects,
            limits.max_images_per_page,
            plan,
        )?;
        if objects.len() != usize::try_from(plan.count).unwrap_or(usize::MAX) {
            return Err(Error::InvalidResult {
                operation: "image_handles",
                detail: "materialized count changed after preflight".into(),
            });
        }
        let mut slots = try_uninit_boxed_slice::<Image<'_>>(objects.len(), "image_handles")?;
        for (index, (slot, object)) in slots.iter_mut().zip(objects).enumerate() {
            slot.write(Image {
                raw: object.raw,
                bounds: object.bounds,
                index: u32::try_from(index).unwrap_or(u32::MAX),
                page: self,
            });
        }
        let raw = Box::into_raw(slots);
        // SAFETY: the exact number of slots was initialized in the loop above.
        let images = unsafe { Box::from_raw(raw as *mut [Image<'_>]) };
        Ok(images.into_vec())
    }

    /// Tight Rust allocation bound for [`Self::images`], computed without
    /// constructing the image metadata vectors.
    pub fn image_allocation_bytes(&self) -> Result<u64, Error> {
        self.plan_images().map(|plan| plan.allocation_bytes())
    }
    pub fn render_bgra(&self, width: u32, height: u32) -> Result<Bitmap, Error> {
        let limits = self.document.runtime.0.limits;
        for dimension in [width, height] {
            if dimension > limits.max_render_dimension {
                return Err(Error::ResourceLimit {
                    limit: "max_render_dimension",
                    actual: u64::from(dimension),
                    maximum: u64::from(limits.max_render_dimension),
                });
            }
        }
        let pixels = u64::from(width).checked_mul(u64::from(height)).ok_or_else(|| {
            Error::InvalidResult { operation: "render", detail: "pixel count overflow".into() }
        })?;
        if pixels > limits.max_render_pixels {
            return Err(Error::ResourceLimit {
                limit: "max_render_pixels",
                actual: pixels,
                maximum: limits.max_render_pixels,
            });
        }
        let bitmap_bytes = pixels.checked_mul(4).ok_or_else(|| Error::InvalidResult {
            operation: "render",
            detail: "bitmap byte count overflow".into(),
        })?;
        if bitmap_bytes > limits.max_bitmap_bytes {
            return Err(Error::ResourceLimit {
                limit: "max_bitmap_bytes",
                actual: bitmap_bytes,
                maximum: limits.max_bitmap_bytes,
            });
        }
        let _guard = self.document.runtime.0.lock()?;
        let bytes = self.document.runtime.0.backend.render(self.raw, width, height)?;
        Ok(Bitmap { width, height, stride: width.saturating_mul(4), bytes })
    }
}
impl Drop for Page<'_> {
    fn drop(&mut self) {
        if let Ok(_guard) = self.document.runtime.0.gate.lock() {
            self.document.runtime.0.backend.close_page(self.raw);
        }
    }
}

pub struct TextPage<'page> {
    page: &'page Page<'page>,
    raw: usize,
}
impl TextPage<'_> {
    pub fn character_count(&self) -> Result<u32, Error> {
        let _guard = self.page.document.runtime.0.lock()?;
        self.page.document.runtime.0.backend.character_count(self.raw)
    }
    pub fn text(&self) -> Result<String, Error> {
        let _guard = self.page.document.runtime.0.lock()?;
        self.page
            .document
            .runtime
            .0
            .backend
            .text(self.raw, self.page.document.runtime.0.limits.max_text_units_per_page)
    }

    pub fn characters(&self) -> Result<Vec<Character>, Error> {
        self.plan_characters()?.materialize()
    }

    pub fn plan_characters(&self) -> Result<PlannedCharacters<'_, '_>, Error> {
        let _guard = self.page.document.runtime.0.lock()?;
        let plan = self
            .page
            .document
            .runtime
            .0
            .backend
            .character_allocation_bytes(self.raw, self.page.document.runtime.0.limits)?;
        Ok(PlannedCharacters { text: self, plan })
    }

    fn materialize_characters(
        &self,
        plan: CharacterAllocationPlan,
    ) -> Result<Vec<Character>, Error> {
        let _guard = self.page.document.runtime.0.lock()?;
        self.page.document.runtime.0.backend.characters(
            self.raw,
            self.page.document.runtime.0.limits,
            plan,
        )
    }

    pub fn links(&self) -> Result<Vec<Link>, Error> {
        self.plan_links()?.materialize()
    }

    pub fn plan_links(&self) -> Result<PlannedLinks<'_, '_>, Error> {
        let _guard = self.page.document.runtime.0.lock()?;
        let plan = self.page.document.runtime.0.backend.link_allocation_bytes(
            self.page.document.raw,
            self.page.raw,
            self.raw,
            self.page.document.runtime.0.limits,
        )?;
        Ok(PlannedLinks { text: self, plan })
    }

    fn materialize_links(&self, plan: LinkAllocationPlan) -> Result<Vec<Link>, Error> {
        let _guard = self.page.document.runtime.0.lock()?;
        self.page.document.runtime.0.backend.links(
            self.page.document.raw,
            self.page.raw,
            self.raw,
            self.page.document.runtime.0.limits,
            plan,
        )
    }

    /// Tight allocation bound for [`Self::links`], computed before URI strings
    /// or link vectors are materialized.
    pub fn link_allocation_bytes(&self) -> Result<u64, Error> {
        self.plan_links().map(|plan| plan.allocation_bytes())
    }
}

pub struct PlannedCharacters<'plan, 'page> {
    text: &'plan TextPage<'page>,
    plan: CharacterAllocationPlan,
}
impl PlannedCharacters<'_, '_> {
    /// Peak needed to materialize the native character vector and its owned
    /// font-name strings.
    #[must_use]
    pub const fn allocation_bytes(&self) -> u64 {
        self.plan.bytes
    }
    /// Additional retained font-name capacity needed while a caller builds
    /// one independently-owned provenance copy per character.
    #[must_use]
    pub const fn retained_font_bytes(&self) -> u64 {
        self.plan.retained_font_bytes
    }
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.plan.count
    }
    pub fn materialize(self) -> Result<Vec<Character>, Error> {
        self.text.materialize_characters(self.plan)
    }
}
impl Drop for TextPage<'_> {
    fn drop(&mut self) {
        if let Ok(_guard) = self.page.document.runtime.0.gate.lock() {
            self.page.document.runtime.0.backend.close_text_page(self.raw);
        }
    }
}

#[derive(Clone, Copy)]
pub struct Image<'page> {
    raw: usize,
    index: u32,
    bounds: PdfRect,
    page: &'page Page<'page>,
}
impl std::fmt::Debug for Image<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Image").field("index", &self.index).finish_non_exhaustive()
    }
}
impl Image<'_> {
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    #[must_use]
    pub const fn bounds(self) -> PdfRect {
        self.bounds
    }

    pub fn bitmap(&self) -> Result<ImageBitmap, Error> {
        self.plan_bitmap()?.materialize()
    }

    pub fn plan_bitmap(&self) -> Result<PlannedBitmap<'_, '_>, Error> {
        let _guard = self.page.document.runtime.0.lock()?;
        let allocation_bytes = self
            .page
            .document
            .runtime
            .0
            .backend
            .image_bitmap_allocation_bytes(self.raw, self.page.document.runtime.0.limits)?;
        Ok(PlannedBitmap { image: self, allocation_bytes })
    }

    /// Tight decoded bitmap allocation bound obtained without decoding or
    /// copying image pixels.
    pub fn bitmap_allocation_bytes(&self) -> Result<u64, Error> {
        self.plan_bitmap().map(|plan| plan.allocation_bytes())
    }
}

pub struct PlannedLinks<'plan, 'page> {
    text: &'plan TextPage<'page>,
    plan: LinkAllocationPlan,
}
impl PlannedLinks<'_, '_> {
    #[must_use]
    pub const fn allocation_bytes(&self) -> u64 {
        self.plan.bytes
    }
    pub fn materialize(self) -> Result<Vec<Link>, Error> {
        self.text.materialize_links(self.plan)
    }
}

pub struct PlannedImages<'plan, 'document> {
    page: &'plan Page<'document>,
    plan: ImageAllocationPlan,
}

pub struct PlannedPathBounds<'plan, 'document> {
    page: &'plan Page<'document>,
    plan: PathBoundsAllocationPlan,
}
impl PlannedPathBounds<'_, '_> {
    #[must_use]
    pub const fn allocation_bytes(&self) -> u64 {
        self.plan.bytes
    }
    pub fn materialize(self) -> Result<Vec<PdfRect>, Error> {
        self.materialize_with_checkpoint(&mut || true)
    }

    /// Materialize PATH bounds while invoking a caller-owned checkpoint once
    /// per fixed object batch. Returning `false` interrupts the scan.
    pub fn materialize_with_checkpoint(
        self,
        checkpoint: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<PdfRect>, Error> {
        self.page.materialize_path_bounds(self.plan, checkpoint)
    }
}
impl<'plan> PlannedImages<'plan, '_> {
    #[must_use]
    pub const fn allocation_bytes(&self) -> u64 {
        self.plan.bytes
    }
    pub fn materialize(self) -> Result<Vec<Image<'plan>>, Error> {
        self.page.materialize_images(self.plan)
    }
}

pub struct PlannedBitmap<'plan, 'page> {
    image: &'plan Image<'page>,
    allocation_bytes: u64,
}
impl PlannedBitmap<'_, '_> {
    #[must_use]
    pub const fn allocation_bytes(&self) -> u64 {
        self.allocation_bytes
    }
    pub fn materialize(self) -> Result<ImageBitmap, Error> {
        let _guard = self.image.page.document.runtime.0.lock()?;
        let bitmap = self.image.page.document.runtime.0.backend.image_bitmap(
            self.image.raw,
            self.image.page.document.runtime.0.limits,
            self.allocation_bytes,
        )?;
        if u64::try_from(bitmap.bytes.capacity()).unwrap_or(u64::MAX) > self.allocation_bytes {
            return Err(Error::InvalidResult {
                operation: "image_bitmap",
                detail: "materialized bitmap exceeded preflight plan".into(),
            });
        }
        Ok(bitmap)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Gray,
    Bgr,
    Bgrx,
    Bgra,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBitmap {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub bytes: Vec<u8>,
}

/// PDF-native rectangle in points, with a bottom-left origin.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PdfRect {
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageInfo {
    /// Displayed page width in PDF points; `PDFium` has already applied page rotation.
    pub width_points: f32,
    /// Displayed page height in PDF points; `PDFium` has already applied page rotation.
    pub height_points: f32,
    /// Clockwise rotation in degrees, one of 0, 90, 180, or 270.
    pub rotation_degrees: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Character {
    pub index: u32,
    pub value: char,
    pub bounds: PdfRect,
    pub font_name: Option<String>,
    pub font_size: f32,
    pub angle_degrees: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub bounds: PdfRect,
    pub target: LinkTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    ExternalUri(String),
    InternalPage { page_index: u32 },
}

#[derive(Debug, Clone, Copy)]
struct ImageObject {
    raw: usize,
    bounds: PdfRect,
}

#[derive(Clone, Copy)]
struct LinkAllocationPlan {
    bytes: u64,
    count: u32,
    target_bytes: u64,
    maximum_temporary_bytes: u64,
    vector_capacity: u64,
}

#[derive(Clone, Copy)]
struct CharacterAllocationPlan {
    bytes: u64,
    count: u32,
    font_bytes: u64,
    maximum_font_bytes: u64,
    retained_font_bytes: u64,
}

#[derive(Clone, Copy)]
struct ImageAllocationPlan {
    bytes: u64,
    count: u32,
}

#[derive(Clone, Copy)]
struct PathBoundsAllocationPlan {
    bytes: u64,
    count: u32,
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
    // SAFETY: the valid non-zero layout is transferred into the returned Box.
    let raw = unsafe { std::alloc::alloc(layout) }.cast::<std::mem::MaybeUninit<T>>();
    if raw.is_null() {
        return Err(Error::Allocation {
            operation,
            bytes: u64::try_from(layout.size()).unwrap_or(u64::MAX),
        });
    }
    let slice = std::ptr::slice_from_raw_parts_mut(raw, length);
    // SAFETY: `slice` names exactly the allocation obtained above.
    Ok(unsafe { Box::from_raw(slice) })
}

fn limit(name: &'static str, actual: usize, maximum: u64) -> Result<(), Error> {
    let actual = u64::try_from(actual).unwrap_or(u64::MAX);
    if actual > maximum {
        Err(Error::ResourceLimit { limit: name, actual, maximum })
    } else {
        Ok(())
    }
}

fn bounded_password(value: Option<&str>, maximum: u32) -> Result<Option<CString>, Error> {
    let capacity = usize::try_from(password_allocation_bytes(value, maximum)?)
        .map_err(|_| Error::Allocation { operation: "password", bytes: u64::MAX })?;
    let Some(value) = value else { return Ok(None) };
    let mut bytes = try_uninit_boxed_slice::<u8>(capacity, "password")?;
    for (slot, byte) in bytes.iter_mut().zip(value.bytes().chain(std::iter::once(0))) {
        slot.write(byte);
    }
    // SAFETY: the zip writes exactly `value.len() + 1 == capacity` bytes.
    let bytes = unsafe { bytes.assume_init() }.into_vec();
    CString::from_vec_with_nul(bytes).map(Some).map_err(|error| Error::InvalidResult {
        operation: "load_document",
        detail: format!("invalid password: {error}"),
    })
}

fn password_allocation_bytes(value: Option<&str>, maximum: u32) -> Result<u64, Error> {
    let Some(value) = value else { return Ok(0) };
    let actual = u64::try_from(value.len()).unwrap_or(u64::MAX);
    if actual > u64::from(maximum) {
        return Err(Error::ResourceLimit {
            limit: "max_password_bytes",
            actual,
            maximum: u64::from(maximum),
        });
    }
    if value.as_bytes().contains(&0) {
        return Err(Error::InvalidResult {
            operation: "load_document",
            detail: "password contains NUL".into(),
        });
    }
    actual.checked_add(1).ok_or(Error::Allocation { operation: "password", bytes: actual })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Mock {
        document_opens: AtomicUsize,
        closes: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        image_plan_count: AtomicUsize,
        image_allocations: AtomicUsize,
        path_plan_count: AtomicUsize,
        path_materialized_count: AtomicUsize,
        path_allocations: AtomicUsize,
        path_plan_scanned: AtomicUsize,
        path_materialized_scanned: AtomicUsize,
        character_plan_count: AtomicUsize,
        character_materialized_count: AtomicUsize,
        character_plan_font_bytes: AtomicUsize,
        character_materialized_font_bytes: AtomicUsize,
        character_allocations: AtomicUsize,
        bitmap_plan_bytes: AtomicUsize,
        bitmap_copies: AtomicUsize,
    }
    impl Mock {
        fn enter(&self) {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            std::thread::yield_now();
            self.active.fetch_sub(1, Ordering::SeqCst);
        }

        fn scan_paths(
            count: u32,
            scanned: &AtomicUsize,
            operation: &'static str,
            checkpoint: &mut dyn FnMut() -> bool,
        ) -> Result<(), Error> {
            for index in 0..count {
                if index.is_multiple_of(PATH_SCAN_CHECKPOINT_OBJECTS) && !checkpoint() {
                    return Err(Error::InvalidResult {
                        operation,
                        detail: "caller interrupted PATH object scan".into(),
                    });
                }
                scanned.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }
    impl Backend for Arc<Mock> {
        fn load_document(&self, _: &[u8], _: Option<&CStr>) -> Result<usize, Error> {
            self.document_opens.fetch_add(1, Ordering::SeqCst);
            self.enter();
            Ok(1)
        }
        fn close_document(&self, _: usize) {
            self.closes.fetch_add(1, Ordering::SeqCst);
        }
        fn page_count(&self, _: usize) -> Result<u32, Error> {
            self.enter();
            Ok(2)
        }
        fn load_page(&self, _: usize, _: u32) -> Result<usize, Error> {
            self.enter();
            Ok(2)
        }
        fn close_page(&self, _: usize) {
            self.closes.fetch_add(1, Ordering::SeqCst);
        }
        fn load_text_page(&self, _: usize) -> Result<usize, Error> {
            self.enter();
            Ok(3)
        }
        fn close_text_page(&self, _: usize) {
            self.closes.fetch_add(1, Ordering::SeqCst);
        }
        fn text(&self, _: usize, _: u32) -> Result<String, Error> {
            self.enter();
            Ok("ok".into())
        }
        fn character_count(&self, _: usize) -> Result<u32, Error> {
            Ok(1)
        }
        fn characters(
            &self,
            _: usize,
            _: Limits,
            plan: CharacterAllocationPlan,
        ) -> Result<Vec<Character>, Error> {
            let configured = self.character_materialized_count.load(Ordering::SeqCst);
            let actual = if configured == 0 { 1 } else { u32::try_from(configured).unwrap() };
            if plan.count != actual {
                return Err(Error::InvalidResult {
                    operation: "characters",
                    detail: "materialized count exceeded preflight plan".into(),
                });
            }
            let actual_font =
                u64::try_from(self.character_materialized_font_bytes.load(Ordering::SeqCst))
                    .unwrap();
            if actual_font > plan.font_bytes || actual_font > plan.maximum_font_bytes {
                return Err(Error::InvalidResult {
                    operation: "characters",
                    detail: "materialized font exceeded preflight plan".into(),
                });
            }
            self.character_allocations.fetch_add(1, Ordering::SeqCst);
            Ok(vec![Character {
                index: 0,
                value: 'o',
                bounds: PdfRect { left: 1.0, bottom: 2.0, right: 3.0, top: 4.0 },
                font_name: Some("Mock".into()),
                font_size: 12.0,
                angle_degrees: 0.0,
            }])
        }
        fn character_allocation_bytes(
            &self,
            _: usize,
            _: Limits,
        ) -> Result<CharacterAllocationPlan, Error> {
            let configured = self.character_plan_count.load(Ordering::SeqCst);
            let count = if configured == 0 { 1 } else { u32::try_from(configured).unwrap() };
            let font_bytes =
                u64::try_from(self.character_plan_font_bytes.load(Ordering::SeqCst)).unwrap();
            let character_capacity = u64::from(count);
            Ok(CharacterAllocationPlan {
                bytes: character_capacity
                    * u64::try_from(std::mem::size_of::<Character>()).unwrap()
                    + font_bytes,
                count,
                font_bytes,
                maximum_font_bytes: font_bytes,
                retained_font_bytes: font_bytes,
            })
        }
        fn page_info(&self, _: usize) -> Result<PageInfo, Error> {
            Ok(PageInfo { width_points: 100.0, height_points: 100.0, rotation_degrees: 0 })
        }
        fn page_object_count(&self, _: usize) -> Result<u32, Error> {
            Ok(2)
        }
        fn path_bounds(
            &self,
            _: usize,
            _: u32,
            plan: PathBoundsAllocationPlan,
            checkpoint: &mut dyn FnMut() -> bool,
        ) -> Result<Vec<PdfRect>, Error> {
            let actual = u32::try_from(self.path_materialized_count.load(Ordering::SeqCst))
                .unwrap_or(u32::MAX);
            if plan.count != actual {
                return Err(Error::InvalidResult {
                    operation: "path_bounds",
                    detail: "materialized count exceeded preflight plan".into(),
                });
            }
            Mock::scan_paths(
                actual,
                &self.path_materialized_scanned,
                "path_bounds_checkpoint",
                checkpoint,
            )?;
            self.path_allocations.fetch_add(1, Ordering::SeqCst);
            Ok(vec![PdfRect::default(); usize::try_from(actual).unwrap_or(usize::MAX)])
        }
        fn path_bounds_allocation_bytes(
            &self,
            _: usize,
            _: u32,
            checkpoint: &mut dyn FnMut() -> bool,
        ) -> Result<PathBoundsAllocationPlan, Error> {
            let count =
                u32::try_from(self.path_plan_count.load(Ordering::SeqCst)).unwrap_or(u32::MAX);
            Mock::scan_paths(
                count,
                &self.path_plan_scanned,
                "path_bounds_plan_checkpoint",
                checkpoint,
            )?;
            Ok(PathBoundsAllocationPlan {
                bytes: u64::from(count) * u64::try_from(std::mem::size_of::<PdfRect>()).unwrap(),
                count,
            })
        }
        fn links(
            &self,
            _: usize,
            _: usize,
            _: usize,
            _: Limits,
            _: LinkAllocationPlan,
        ) -> Result<Vec<Link>, Error> {
            Ok(Vec::new())
        }
        fn link_allocation_bytes(
            &self,
            _: usize,
            _: usize,
            _: usize,
            _: Limits,
        ) -> Result<LinkAllocationPlan, Error> {
            Ok(LinkAllocationPlan {
                bytes: 0,
                count: 0,
                target_bytes: 0,
                maximum_temporary_bytes: 0,
                vector_capacity: 0,
            })
        }
        fn image_objects(
            &self,
            _: usize,
            _: u32,
            _: u32,
            plan: ImageAllocationPlan,
        ) -> Result<Vec<ImageObject>, Error> {
            self.enter();
            if plan.count != 2 {
                return Err(Error::InvalidResult {
                    operation: "image_objects",
                    detail: "materialized count exceeded preflight plan".into(),
                });
            }
            self.image_allocations.fetch_add(1, Ordering::SeqCst);
            Ok(vec![
                ImageObject { raw: 5, bounds: PdfRect::default() },
                ImageObject { raw: 6, bounds: PdfRect::default() },
            ])
        }
        fn image_object_allocation_bytes(
            &self,
            _: usize,
            _: u32,
            _: u32,
        ) -> Result<ImageAllocationPlan, Error> {
            let configured = self.image_plan_count.load(Ordering::SeqCst);
            let count = if configured == 0 { 2 } else { u32::try_from(configured).unwrap() };
            let vector_capacity = u64::from(count);
            Ok(ImageAllocationPlan {
                bytes: vector_capacity
                    * u64::try_from(
                        std::mem::size_of::<ImageObject>() + std::mem::size_of::<Image<'_>>(),
                    )
                    .unwrap(),
                count,
            })
        }
        fn render(&self, _: usize, width: u32, height: u32) -> Result<Vec<u8>, Error> {
            self.enter();
            Ok(vec![0; usize::try_from(width * height * 4).unwrap()])
        }
        fn image_bitmap(
            &self,
            _: usize,
            _: Limits,
            planned_bytes: u64,
        ) -> Result<ImageBitmap, Error> {
            self.enter();
            if planned_bytes < 3 {
                return Err(Error::InvalidResult {
                    operation: "image_bitmap",
                    detail: "materialized bitmap exceeded preflight plan".into(),
                });
            }
            self.bitmap_copies.fetch_add(1, Ordering::SeqCst);
            Ok(ImageBitmap {
                width: 1,
                height: 1,
                stride: 3,
                format: PixelFormat::Bgr,
                bytes: vec![0, 0, 255],
            })
        }
        fn image_bitmap_allocation_bytes(&self, _: usize, _: Limits) -> Result<u64, Error> {
            let configured = self.bitmap_plan_bytes.load(Ordering::SeqCst);
            Ok(if configured == 0 { 3 } else { u64::try_from(configured).unwrap() })
        }
    }
    fn runtime(mock: Arc<Mock>, limits: Limits) -> Pdfium {
        Pdfium(Arc::new(Inner { backend: Box::new(mock), limits, gate: Mutex::new(()) }))
    }

    #[test]
    fn handles_close_child_before_parent() {
        let mock = Arc::new(Mock::default());
        let runtime = runtime(Arc::clone(&mock), Limits::default());
        {
            let document = runtime.open(Arc::from(b"pdf".as_slice()), None).unwrap();
            let page = document.page(0).unwrap();
            {
                let text = page.text_page().unwrap();
                assert_eq!(text.text().unwrap(), "ok");
            }
            let images = page.images().unwrap();
            assert_eq!(images.len(), 2);
            assert_eq!(images[0].bitmap().unwrap().bytes, [0, 0, 255]);
        }
        assert_eq!(mock.closes.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn limits_fail_before_backend_allocation() {
        let mock = Arc::new(Mock::default());
        let limits = Limits {
            max_document_bytes: 2,
            max_render_dimension: 4,
            max_render_pixels: 8,
            ..Limits::default()
        };
        let bounded_runtime = runtime(mock, limits);
        assert!(matches!(
            bounded_runtime.open(Arc::from(b"big".as_slice()), None),
            Err(Error::ResourceLimit { limit: "max_document_bytes", .. })
        ));
        let document = bounded_runtime.open(Arc::from(b"ok".as_slice()), None).unwrap();
        let page = document.page(0).unwrap();
        assert!(matches!(
            page.render_bgra(4, 4),
            Err(Error::ResourceLimit { limit: "max_render_pixels", .. })
        ));
        assert!(matches!(
            document.page(2),
            Err(Error::InvalidResult { operation: "load_page", .. })
        ));
        assert!(matches!(
            Limits { max_images_per_page: u32::MAX, ..Limits::default() }.validate(),
            Err(Error::ResourceLimit { limit: "max_images_per_page", .. })
        ));
        assert!(matches!(
            Pdfium::load_pinned(
                Path::new("/missing/libpdfium.dylib"),
                Limits { max_images_per_page: u32::MAX, ..Limits::default() }
            ),
            Err(Error::ResourceLimit { limit: "max_images_per_page", .. })
        ));
        assert!(matches!(
            Limits { max_render_dimension: u32::MAX, ..Limits::default() }.validate(),
            Err(Error::ResourceLimit { limit: "max_render_dimension", .. })
        ));
        let password_limited_runtime = runtime(
            Arc::new(Mock::default()),
            Limits { max_password_bytes: 1, ..Limits::default() },
        );
        assert!(matches!(
            password_limited_runtime.open(Arc::from(b"ok".as_slice()), Some("xx")),
            Err(Error::ResourceLimit { limit: "max_password_bytes", .. })
        ));
        let password = bounded_password(Some("secret"), 6).unwrap().unwrap().into_bytes_with_nul();
        assert_eq!(password, b"secret\0");
        assert_eq!(password.capacity(), password.len());
        assert!(bounded_password(Some("nul\0inside"), 32).is_err());

        let mock = Arc::new(Mock::default());
        let planned_runtime = runtime(Arc::clone(&mock), Limits::default());
        let plan = planned_runtime.plan_open(Arc::from(b"ok".as_slice()), Some("secret")).unwrap();
        assert_eq!(plan.allocation_bytes(), 7);
        assert_eq!(mock.document_opens.load(Ordering::SeqCst), 0);
        drop(plan.materialize().unwrap());
        assert_eq!(mock.document_opens.load(Ordering::SeqCst), 1);
        let no_password = planned_runtime.plan_open(Arc::from(b"ok".as_slice()), None).unwrap();
        assert_eq!(no_password.allocation_bytes(), 0);
    }

    #[test]
    fn opaque_plans_reject_changed_counts_and_sizes_before_backend_copy() {
        let mock = Arc::new(Mock::default());
        mock.image_plan_count.store(1, Ordering::SeqCst);
        let runtime = runtime(Arc::clone(&mock), Limits::default());
        let document = runtime.open(Arc::from(b"ok".as_slice()), None).unwrap();
        let page = document.page(0).unwrap();
        let plan = page.plan_images().unwrap();
        assert!(matches!(
            plan.materialize(),
            Err(Error::InvalidResult { operation: "image_objects", .. })
        ));
        assert_eq!(mock.image_allocations.load(Ordering::SeqCst), 0);

        mock.path_plan_count.store(1, Ordering::SeqCst);
        let path_plan = page.plan_path_bounds().unwrap();
        assert_eq!(
            path_plan.allocation_bytes(),
            u64::try_from(std::mem::size_of::<PdfRect>()).unwrap()
        );
        mock.path_materialized_count.store(2, Ordering::SeqCst);
        assert!(matches!(
            path_plan.materialize(),
            Err(Error::InvalidResult { operation: "path_bounds", .. })
        ));
        assert_eq!(mock.path_allocations.load(Ordering::SeqCst), 0);
        mock.path_materialized_count.store(1, Ordering::SeqCst);
        assert_eq!(page.plan_path_bounds().unwrap().materialize().unwrap().len(), 1);

        mock.image_plan_count.store(2, Ordering::SeqCst);
        let images = page.plan_images().unwrap().materialize().unwrap();
        mock.bitmap_plan_bytes.store(2, Ordering::SeqCst);
        let plan = images[0].plan_bitmap().unwrap();
        assert!(matches!(
            plan.materialize(),
            Err(Error::InvalidResult { operation: "image_bitmap", .. })
        ));
        assert_eq!(mock.bitmap_copies.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn path_plan_and_materialization_stop_at_fixed_checkpoint_batches() {
        let mock = Arc::new(Mock::default());
        mock.path_plan_count.store(600, Ordering::SeqCst);
        mock.path_materialized_count.store(600, Ordering::SeqCst);
        let runtime = runtime(Arc::clone(&mock), Limits::default());
        let document = runtime.open(Arc::from(b"ok".as_slice()), None).unwrap();
        let page = document.page(0).unwrap();

        let mut plan_checkpoints = 0_usize;
        let Err(error) = page.plan_path_bounds_with_checkpoint(&mut || {
            plan_checkpoints += 1;
            plan_checkpoints < 2
        }) else {
            panic!("plan scan should stop at its second checkpoint")
        };
        assert!(matches!(
            error,
            Error::InvalidResult { operation: "path_bounds_plan_checkpoint", .. }
        ));
        assert_eq!(plan_checkpoints, 2);
        assert_eq!(
            mock.path_plan_scanned.load(Ordering::SeqCst),
            usize::try_from(PATH_SCAN_CHECKPOINT_OBJECTS).unwrap()
        );
        assert_eq!(mock.path_allocations.load(Ordering::SeqCst), 0);

        mock.path_plan_scanned.store(0, Ordering::SeqCst);
        let plan = page.plan_path_bounds().unwrap();
        assert_eq!(mock.path_plan_scanned.load(Ordering::SeqCst), 600);
        let mut materialize_checkpoints = 0_usize;
        let error = plan
            .materialize_with_checkpoint(&mut || {
                materialize_checkpoints += 1;
                materialize_checkpoints < 2
            })
            .unwrap_err();
        assert!(matches!(error, Error::InvalidResult { operation: "path_bounds_checkpoint", .. }));
        assert_eq!(materialize_checkpoints, 2);
        assert_eq!(
            mock.path_materialized_scanned.load(Ordering::SeqCst),
            usize::try_from(PATH_SCAN_CHECKPOINT_OBJECTS).unwrap()
        );
        assert_eq!(mock.path_allocations.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn character_plan_rejects_count_growth_before_backend_allocation() {
        let mock = Arc::new(Mock::default());
        mock.character_plan_count.store(1, Ordering::SeqCst);
        let runtime = runtime(Arc::clone(&mock), Limits::default());
        let document = runtime.open(Arc::from(b"ok".as_slice()), None).unwrap();
        let page = document.page(0).unwrap();
        let text = page.text_page().unwrap();
        let plan = text.plan_characters().unwrap();
        mock.character_materialized_count.store(2, Ordering::SeqCst);
        assert!(matches!(
            plan.materialize(),
            Err(Error::InvalidResult { operation: "characters", .. })
        ));
        assert_eq!(mock.character_allocations.load(Ordering::SeqCst), 0);

        mock.character_materialized_count.store(1, Ordering::SeqCst);
        mock.character_plan_font_bytes.store(1, Ordering::SeqCst);
        let plan = text.plan_characters().unwrap();
        assert_eq!(plan.retained_font_bytes(), 1);
        mock.character_materialized_font_bytes.store(2, Ordering::SeqCst);
        assert!(matches!(
            plan.materialize(),
            Err(Error::InvalidResult { operation: "characters", .. })
        ));
        assert_eq!(mock.character_allocations.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn concurrent_calls_are_serialized() {
        let mock = Arc::new(Mock::default());
        let runtime = runtime(Arc::clone(&mock), Limits::default());
        std::thread::scope(|scope| {
            for _ in 0..12 {
                let runtime = runtime.clone();
                scope.spawn(move || {
                    let document = runtime.open(Arc::from(b"x".as_slice()), None).unwrap();
                    assert_eq!(document.page_count(), 2);
                });
            }
        });
        assert_eq!(mock.max_active.load(Ordering::SeqCst), 1);
    }

    #[test]
    #[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
    #[allow(clippy::too_many_lines)]
    fn native_smoke() {
        let path = std::env::var_os("PDFIUM_LIBRARY").expect("PDFIUM_LIBRARY is required");
        {
            let runtime = Pdfium::load_pinned(Path::new(&path), Limits::default()).unwrap();
            assert!(matches!(
                Pdfium::load_pinned(Path::new(&path), Limits::default()),
                Err(Error::Load(message)) if message.contains("already active")
            ));
            assert!(matches!(
                runtime.open(Arc::from(b"not a PDF".as_slice()), None),
                Err(Error::Native { operation: "load_document", code }) if code != 0
            ));
            let document = runtime.open(Arc::from(minimal_pdf()), None).unwrap();
            assert_eq!(document.page_count(), 1);
            let page = document.page(0).unwrap();
            assert_eq!(
                page.info().unwrap(),
                PageInfo { width_points: 100.0, height_points: 100.0, rotation_degrees: 0 }
            );
            let text = page.text_page().unwrap();
            assert_eq!(text.text().unwrap(), "Hello PDFium https://example.test/");
            let characters = text.characters().unwrap();
            assert_eq!(characters[0].value, 'H');
            assert_eq!(characters[0].index, 0);
            assert!(characters[0].bounds.right > characters[0].bounds.left);
            assert!(characters[0].font_size > 0.0);
            assert!(characters[0].font_name.as_ref().is_some_and(|name| !name.is_empty()));
            let link_plan = text.link_allocation_bytes().unwrap();
            let links = text.links().unwrap();
            assert!(link_plan >= u64::try_from(links.len() * std::mem::size_of::<Link>()).unwrap());
            assert!(links.iter().any(|link| matches!(&link.target, LinkTarget::ExternalUri(uri) if uri.contains("example.test"))));
            drop(text);
            let image_plan = page.image_allocation_bytes().unwrap();
            let images = page.images().unwrap();
            assert_eq!(images.len(), 1);
            assert!(
                image_plan
                    >= u64::try_from(
                        std::mem::size_of::<ImageObject>() + std::mem::size_of::<Image<'_>>()
                    )
                    .unwrap()
            );
            assert!(images[0].bounds().right > images[0].bounds().left);
            let bitmap_plan = images[0].bitmap_allocation_bytes().unwrap();
            let image = images[0].bitmap().unwrap();
            assert!(bitmap_plan >= u64::try_from(image.bytes.capacity()).unwrap());
            assert_eq!((image.width, image.height), (1, 1));
            assert!(matches!(
                image.format,
                PixelFormat::Bgr | PixelFormat::Bgrx | PixelFormat::Bgra
            ));
            assert!(image.bytes.windows(3).any(|pixel| pixel == [0, 0, 255]));
            assert_eq!(page.render_bgra(8, 8).unwrap().bytes.len(), 256);

            drop(page);
            drop(document);
            let rotated = runtime.open(Arc::from(rotated_pdf()), None).unwrap();
            assert_eq!(rotated.page_count(), 4);
            for (index, (rotation, width, height)) in
                [(0, 100.0, 200.0), (90, 200.0, 100.0), (180, 100.0, 200.0), (270, 200.0, 100.0)]
                    .into_iter()
                    .enumerate()
            {
                let page = rotated.page(u32::try_from(index).unwrap()).unwrap();
                assert_eq!(
                    page.info().unwrap(),
                    PageInfo {
                        width_points: width,
                        height_points: height,
                        rotation_degrees: rotation,
                    }
                );
                let text = page.text_page().unwrap();
                let character = &text.characters().unwrap()[0];
                assert!(character.bounds.right > character.bounds.left);
                assert!((0.0..360.0).contains(&character.angle_degrees));
                let links = text.links().unwrap();
                assert!(links.iter().any(|link| {
                    link.bounds == PdfRect { left: 10.0, bottom: 150.0, right: 40.0, top: 170.0 }
                        && matches!(&link.target, LinkTarget::ExternalUri(uri) if uri == "https://example.test/rotated")
                }));
                assert!(links.iter().any(|link| {
                    link.bounds == PdfRect { left: 50.0, bottom: 100.0, right: 90.0, top: 120.0 }
                        && matches!(link.target, LinkTarget::InternalPage { page_index: 1 })
                }));
                drop(text);
                let image = &page.images().unwrap()[0];
                assert!(image.bounds().right > image.bounds().left);
            }
        }

        let limits =
            Limits { max_text_units_per_page: 1, max_bitmap_bytes: 2, ..Limits::default() };
        let runtime = Pdfium::load_pinned(Path::new(&path), limits).unwrap();
        let document = runtime.open(Arc::from(minimal_pdf()), None).unwrap();
        let page = document.page(0).unwrap();
        assert!(matches!(
            page.text_page().unwrap().text(),
            Err(Error::ResourceLimit { limit: "max_text_units_per_page", .. })
        ));
        assert!(matches!(
            page.images().unwrap()[0].bitmap(),
            Err(Error::ResourceLimit { limit: "max_bitmap_bytes", .. })
        ));
    }

    fn minimal_pdf() -> Vec<u8> {
        let content = b"BT /F1 12 Tf 10 60 Td (Hello PDFium https://example.test/) Tj ET\nq 10 0 0 10 10 10 cm /Im1 Do Q\n";
        let objects = [
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 5 0 R >> /XObject << /Im1 6 0 R >> >> /Contents 4 0 R /Annots [7 0 R] >>".to_vec(),
            stream_object("", content),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            stream_object("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8", &[255, 0, 0]),
            b"<< /Type /Annot /Subtype /Link /Rect [10 50 80 70] /A << /S /URI /URI (https://example.test/) >> >>".to_vec(),
        ];
        let mut pdf = b"%PDF-1.4\n%\x80\x80\x80\x80\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn rotated_pdf() -> Vec<u8> {
        let content = b"BT /F1 12 Tf 10 160 Td (R) Tj ET\nq 20 0 0 30 10 20 cm /Im1 Do Q\n";
        let mut objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 6 0 R] /Count 4 >>".to_vec(),
        ];
        for rotation in [0, 90, 180, 270] {
            objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Rotate {rotation} /Resources << /Font << /F1 8 0 R >> /XObject << /Im1 9 0 R >> >> /Contents 7 0 R /Annots [10 0 R 11 0 R] >>").into_bytes());
        }
        objects.extend([
            stream_object("", content),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            stream_object("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8", &[255, 0, 0]),
            b"<< /Type /Annot /Subtype /Link /Rect [10 150 40 170] /A << /S /URI /URI (https://example.test/rotated) >> >>".to_vec(),
            b"<< /Type /Annot /Subtype /Link /Rect [50 100 90 120] /Dest [4 0 R /Fit] >>".to_vec(),
        ]);
        assemble_pdf(&objects)
    }

    fn assemble_pdf(objects: &[Vec<u8>]) -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n%\x80\x80\x80\x80\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn stream_object(dictionary: &str, bytes: &[u8]) -> Vec<u8> {
        let mut object =
            format!("<< {dictionary} /Length {} >>\nstream\n", bytes.len()).into_bytes();
        object.extend_from_slice(bytes);
        object.extend_from_slice(b"\nendstream");
        object
    }
}

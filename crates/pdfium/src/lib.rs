//! Fail-closed `PDFium` boundary.
#![allow(missing_docs)]
#![allow(clippy::missing_errors_doc)]
mod native;

use std::ffi::{CStr, CString};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

pub use native::{Artifact, Platform};
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
    fn image_objects(
        &self,
        page: usize,
        max_objects: u32,
        max_images: u32,
    ) -> Result<Vec<usize>, Error>;
    fn render(&self, page: usize, width: u32, height: u32) -> Result<Vec<u8>, Error>;
    fn image_bitmap(&self, image: usize, limits: Limits) -> Result<ImageBitmap, Error>;
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
        limit("max_document_bytes", bytes.len(), self.0.limits.max_document_bytes)?;
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
    pub fn text_page(&self) -> Result<TextPage<'_>, Error> {
        let _guard = self.document.runtime.0.lock()?;
        Ok(TextPage { page: self, raw: self.document.runtime.0.backend.load_text_page(self.raw)? })
    }
    pub fn images(&self) -> Result<Vec<Image<'_>>, Error> {
        let limits = self.document.runtime.0.limits;
        let _guard = self.document.runtime.0.lock()?;
        let objects = self.document.runtime.0.backend.image_objects(
            self.raw,
            limits.max_page_objects,
            limits.max_images_per_page,
        )?;
        let mut images = Vec::new();
        images.try_reserve_exact(objects.len()).map_err(|_| Error::Allocation {
            operation: "image_handles",
            bytes: u64::try_from(objects.len()).unwrap_or(u64::MAX)
                * u64::try_from(std::mem::size_of::<Image<'_>>()).unwrap_or(u64::MAX),
        })?;
        images.extend(objects.into_iter().enumerate().map(|(index, raw)| Image {
            raw,
            index: u32::try_from(index).unwrap_or(u32::MAX),
            page: self,
        }));
        Ok(images)
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
    pub fn text(&self) -> Result<String, Error> {
        let _guard = self.page.document.runtime.0.lock()?;
        self.page
            .document
            .runtime
            .0
            .backend
            .text(self.raw, self.page.document.runtime.0.limits.max_text_units_per_page)
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

    pub fn bitmap(&self) -> Result<ImageBitmap, Error> {
        let _guard = self.page.document.runtime.0.lock()?;
        self.page
            .document
            .runtime
            .0
            .backend
            .image_bitmap(self.raw, self.page.document.runtime.0.limits)
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

fn limit(name: &'static str, actual: usize, maximum: u64) -> Result<(), Error> {
    let actual = u64::try_from(actual).unwrap_or(u64::MAX);
    if actual > maximum {
        Err(Error::ResourceLimit { limit: name, actual, maximum })
    } else {
        Ok(())
    }
}

fn bounded_password(value: Option<&str>, maximum: u32) -> Result<Option<CString>, Error> {
    let Some(value) = value else { return Ok(None) };
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
    let capacity = value
        .len()
        .checked_add(1)
        .ok_or(Error::Allocation { operation: "password", bytes: actual })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| Error::Allocation {
        operation: "password",
        bytes: u64::try_from(capacity).unwrap_or(u64::MAX),
    })?;
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    CString::from_vec_with_nul(bytes).map(Some).map_err(|error| Error::InvalidResult {
        operation: "load_document",
        detail: format!("invalid password: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Mock {
        closes: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
    }
    impl Mock {
        fn enter(&self) {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            std::thread::yield_now();
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
    impl Backend for Arc<Mock> {
        fn load_document(&self, _: &[u8], _: Option<&CStr>) -> Result<usize, Error> {
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
        fn image_objects(&self, _: usize, _: u32, _: u32) -> Result<Vec<usize>, Error> {
            self.enter();
            Ok(vec![5, 6])
        }
        fn render(&self, _: usize, width: u32, height: u32) -> Result<Vec<u8>, Error> {
            self.enter();
            Ok(vec![0; usize::try_from(width * height * 4).unwrap()])
        }
        fn image_bitmap(&self, _: usize, _: Limits) -> Result<ImageBitmap, Error> {
            self.enter();
            Ok(ImageBitmap {
                width: 1,
                height: 1,
                stride: 3,
                format: PixelFormat::Bgr,
                bytes: vec![0, 0, 255],
            })
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
        let runtime = runtime(
            Arc::new(Mock::default()),
            Limits { max_password_bytes: 1, ..Limits::default() },
        );
        assert!(matches!(
            runtime.open(Arc::from(b"ok".as_slice()), Some("xx")),
            Err(Error::ResourceLimit { limit: "max_password_bytes", .. })
        ));
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
            let text = page.text_page().unwrap();
            assert_eq!(text.text().unwrap(), "Hello PDFium");
            drop(text);
            let images = page.images().unwrap();
            assert_eq!(images.len(), 1);
            let image = images[0].bitmap().unwrap();
            assert_eq!((image.width, image.height), (1, 1));
            assert!(matches!(
                image.format,
                PixelFormat::Bgr | PixelFormat::Bgrx | PixelFormat::Bgra
            ));
            assert!(image.bytes.windows(3).any(|pixel| pixel == [0, 0, 255]));
            assert_eq!(page.render_bgra(8, 8).unwrap().bytes.len(), 256);
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
        let content =
            b"BT /F1 12 Tf 10 60 Td (Hello PDFium) Tj ET\nq 10 0 0 10 10 10 cm /Im1 Do Q\n";
        let objects = [
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 5 0 R >> /XObject << /Im1 6 0 R >> >> /Contents 4 0 R >>".to_vec(),
            stream_object("", content),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            stream_object("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8", &[255, 0, 0]),
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

    fn stream_object(dictionary: &str, bytes: &[u8]) -> Vec<u8> {
        let mut object =
            format!("<< {dictionary} /Length {} >>\nstream\n", bytes.len()).into_bytes();
        object.extend_from_slice(bytes);
        object.extend_from_slice(b"\nendstream");
        object
    }
}

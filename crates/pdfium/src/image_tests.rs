use super::*;
use crate::tests::{assemble_pdf, stream_object};

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn native_soft_mask_preserves_pixels_and_page_placement() {
    let path = std::env::var_os("PDFIUM_LIBRARY").expect("PDFIUM_LIBRARY is required");
    let runtime = Pdfium::load_pinned(Path::new(&path), Limits::default()).unwrap();
    let source = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /Im1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream_object("", b"q 20 0 0 30 10 15 cm /Im1 Do Q"),
        stream_object("/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /SMask 6 0 R", &[255, 0, 0, 0, 0, 0, 255, 0, 0, 0, 0, 0]),
        stream_object("/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8", &[255, 0, 128, 255]),
    ]);
    let document = runtime.open(Arc::from(source), None).unwrap();
    let page = document.page(0).unwrap();
    let before = page.render_bgra(100, 100).unwrap().bytes;
    let images = page.images().unwrap();
    for _ in 0..2 {
        let bitmap = images[0].bitmap().unwrap();
        assert_eq!((bitmap.width, bitmap.height, bitmap.stride), (2, 2, 8));
        assert_eq!(bitmap.format, PixelFormat::Bgra);
        let alpha: Vec<_> = bitmap.bytes.chunks_exact(4).map(|pixel| pixel[3]).collect();
        assert_eq!(alpha, [255, 0, 128, 255]);
        assert_eq!(&bitmap.bytes[8..12], &[0, 0, 255, 128]);
        assert_eq!(page.render_bgra(100, 100).unwrap().bytes, before);
    }
    assert_eq!(page.images().unwrap()[0].bounds(), images[0].bounds());
}

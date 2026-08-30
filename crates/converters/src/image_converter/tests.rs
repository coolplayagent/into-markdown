use super::*;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use into_markdown_core::{
    AiCapability, AiInput, AiOutput, AiProvider, AiRequest, BoundOcrResult, BoxFuture,
    ConversionOutcome, ExecutionOptions, Inline, OcrEngine, OcrEvidenceStage, OcrEvidenceStep,
    OcrOutputPlan, OcrPolicy, OcrRecognition, OcrRegion, OcrRequest, OcrResult, ResourceLimits,
    SourceMetadata, conversion_outcome,
};
use std::collections::BTreeSet;
use std::future::Future;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use tiff::encoder::{Rational, TiffEncoder, colortype};
use tiff::tags::{ResolutionUnit, Tag};

fn context(options: &ConversionOptions) -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), options.limits.clone())
}

fn options() -> ConversionOptions {
    let mut options = ConversionOptions::default();
    options.ocr.policy = OcrPolicy::Off;
    options
}

fn input(bytes: Vec<u8>, name: &str) -> ResolvedInput {
    ResolvedInput {
        bytes: Arc::from(bytes),
        metadata: SourceMetadata { name: Some(name.into()), ..SourceMetadata::default() },
    }
}

fn pixels() -> RgbaImage {
    RgbaImage::from_fn(3, 2, |x, y| {
        Rgba([
            u8::try_from(x * 70).unwrap(),
            u8::try_from(y * 120).unwrap(),
            40,
            if x == 1 { 100 } else { 255 },
        ])
    })
}

fn encoded(format: ImageFormat) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(pixels()).write_to(&mut cursor, format).unwrap();
    cursor.into_inner()
}

fn png_with_reserved_bit_set() -> Vec<u8> {
    let mut png = encoded(ImageFormat::Png);
    let ihdr_end = 8 + 4 + 4 + 13 + 4;
    let kind = b"aaac";
    let crc = png_crc(kind);
    let mut chunk = Vec::with_capacity(12);
    chunk.extend_from_slice(&0_u32.to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(&crc.to_be_bytes());
    png.splice(ihdr_end..ihdr_end, chunk);
    png
}

fn png_crc(bytes: &[u8]) -> u32 {
    let mut value = u32::MAX;
    for byte in bytes {
        value ^= u32::from(*byte);
        for _ in 0..8 {
            value = if value & 1 == 0 { value >> 1 } else { (value >> 1) ^ 0xedb8_8320 };
        }
    }
    !value
}

fn multi_tiff() -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
        encoder.write_image::<colortype::Gray8>(2, 2, &[0, 64, 128, 255]).unwrap();
        encoder.write_image::<colortype::RGB8>(1, 2, &[255, 0, 0, 0, 0, 255]).unwrap();
    }
    bytes.into_inner()
}

fn oriented_tiff() -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
        let mut image = encoder.new_image::<colortype::RGB8>(2, 3).unwrap();
        image.encoder().write_tag(Tag::Orientation, 6_u16).unwrap();
        image.resolution(ResolutionUnit::Inch, Rational { n: 300, d: 1 });
        image
            .write_data(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0, 0, 255, 255, 255, 0, 255])
            .unwrap();
    }
    let mut bytes = bytes.into_inner();
    // The encoder preserves the superseded default 1-DPI rationals as an
    // unreferenced allocation. Remove that allocation so this positive
    // fixture has an exact, fully owned TIFF envelope.
    let ifd = usize::try_from(u32::from_le_bytes(bytes[4..8].try_into().unwrap())).unwrap();
    let count = usize::from(u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()));
    for index in 0..count {
        let entry = ifd + 2 + index * 12;
        let tag = u16::from_le_bytes(bytes[entry..entry + 2].try_into().unwrap());
        if matches!(tag, 273 | 282 | 283) {
            let value = u32::from_le_bytes(bytes[entry + 8..entry + 12].try_into().unwrap());
            bytes[entry + 8..entry + 12].copy_from_slice(&(value - 16).to_le_bytes());
        }
    }
    bytes[4..8].copy_from_slice(&(u32::try_from(ifd - 16).unwrap()).to_le_bytes());
    bytes.drain(20..36);
    bytes
}

fn big_tiff() -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new_big(&mut bytes).unwrap();
        encoder.write_image::<colortype::Gray8>(2, 1, &[0, 255]).unwrap();
    }
    bytes.into_inner()
}

fn tiff_with_unowned_gap(length: usize) -> Vec<u8> {
    let ifd = length - 30;
    let mut bytes = vec![0_u8; length];
    bytes[..4].copy_from_slice(b"II\x2a\0");
    bytes[4..8].copy_from_slice(&u32::try_from(ifd).unwrap().to_le_bytes());
    bytes[8] = 0x7f;
    bytes[ifd..ifd + 2].copy_from_slice(&2_u16.to_le_bytes());
    let strip_offset = ifd + 2;
    bytes[strip_offset..strip_offset + 2].copy_from_slice(&273_u16.to_le_bytes());
    bytes[strip_offset + 2..strip_offset + 4].copy_from_slice(&4_u16.to_le_bytes());
    bytes[strip_offset + 4..strip_offset + 8].copy_from_slice(&1_u32.to_le_bytes());
    bytes[strip_offset + 8..strip_offset + 12].copy_from_slice(&8_u32.to_le_bytes());
    let strip_count = strip_offset + 12;
    bytes[strip_count..strip_count + 2].copy_from_slice(&279_u16.to_le_bytes());
    bytes[strip_count + 2..strip_count + 4].copy_from_slice(&4_u16.to_le_bytes());
    bytes[strip_count + 4..strip_count + 8].copy_from_slice(&1_u32.to_le_bytes());
    bytes[strip_count + 8..strip_count + 12].copy_from_slice(&1_u32.to_le_bytes());
    bytes
}

fn tiff_with_pixel_ifd_overlap(length: usize) -> Vec<u8> {
    let mut bytes = tiff_with_unowned_gap(length);
    let ifd = length - 30;
    let strip_offset = ifd + 2;
    bytes[strip_offset + 8..strip_offset + 12]
        .copy_from_slice(&u32::try_from(ifd + 1).unwrap().to_le_bytes());
    bytes
}

fn bmp_with_header_pixel_gap() -> Vec<u8> {
    let mut bytes = vec![0_u8; 66];
    bytes[..2].copy_from_slice(b"BM");
    bytes[2..6].copy_from_slice(&66_u32.to_le_bytes());
    bytes[10..14].copy_from_slice(&62_u32.to_le_bytes());
    bytes[14..18].copy_from_slice(&40_u32.to_le_bytes());
    bytes[18..22].copy_from_slice(&1_i32.to_le_bytes());
    bytes[22..26].copy_from_slice(&1_i32.to_le_bytes());
    bytes[26..28].copy_from_slice(&1_u16.to_le_bytes());
    bytes[28..30].copy_from_slice(&24_u16.to_le_bytes());
    bytes[34..38].copy_from_slice(&4_u32.to_le_bytes());
    bytes
}

fn tiff_with_oversized_resolution_count() -> Vec<u8> {
    let mut bytes = oriented_tiff();
    let ifd = usize::try_from(u32::from_le_bytes(bytes[4..8].try_into().unwrap())).unwrap();
    let count = usize::from(u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()));
    for index in 0..count {
        let entry = ifd + 2 + index * 12;
        let tag = u16::from_le_bytes(bytes[entry..entry + 2].try_into().unwrap());
        if tag == 282 {
            bytes[entry + 4..entry + 8].copy_from_slice(&u32::MAX.to_le_bytes());
            return bytes;
        }
    }
    panic!("oriented TIFF fixture has no XResolution tag")
}

fn animated_webp_envelope(frame_count: usize) -> Vec<u8> {
    fn chunk(bytes: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
        bytes.extend_from_slice(&kind);
        bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(data);
        if data.len() % 2 == 1 {
            bytes.push(0);
        }
    }
    let mut body = Vec::new();
    chunk(&mut body, *b"VP8X", &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    chunk(&mut body, *b"ANIM", &[0; 6]);
    for _ in 0..frame_count {
        let mut frame = vec![0; 16];
        chunk(&mut frame, *b"VP8 ", &[]);
        chunk(&mut body, *b"ANMF", &frame);
    }
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
    bytes.extend_from_slice(b"WEBP");
    bytes.extend_from_slice(&body);
    bytes
}

fn animated_webp() -> Vec<u8> {
    fn chunk(bytes: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
        bytes.extend_from_slice(&kind);
        bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(data);
        if data.len() % 2 == 1 {
            bytes.push(0);
        }
    }
    let static_webp = encoded(ImageFormat::WebP);
    let primary = &static_webp[12..];
    let mut body = Vec::new();
    chunk(&mut body, *b"VP8X", &[0x12, 0, 0, 0, 2, 0, 0, 1, 0, 0]);
    chunk(&mut body, *b"ANIM", &[0; 6]);
    for duration in [100_u32, 200] {
        let mut frame = vec![0; 12];
        frame.extend_from_slice(&duration.to_le_bytes()[..3]);
        frame.push(0);
        frame[6] = 2;
        frame[9] = 1;
        frame.extend_from_slice(primary);
        chunk(&mut body, *b"ANMF", &frame);
    }
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
    bytes.extend_from_slice(b"WEBP");
    bytes.extend_from_slice(&body);
    bytes
}

#[test]
fn real_ocr_fixture_converts_offline_as_image_only() {
    let bytes = include_bytes!("../../../../fixtures/small/ocr/ocr-mixed-clear-1.png").to_vec();
    let options = options();
    let output = block_on(convert_image(
        &input(bytes.clone(), "mixed.png"),
        &options,
        &Services::default(),
        &context(&options),
    ))
    .unwrap();
    assert_eq!(output.document.blocks.len(), 1);
    assert_eq!(output.assets.len(), 1);
    assert_eq!(output.assets[0].bytes, bytes);
    assert_eq!(output.assets[0].media_type, "image/png");
    assert!(output.diagnostics.is_empty());
}

#[test]
fn jpeg_webp_and_bmp_round_trip_through_strict_converter() {
    for (format, name, media_type) in [
        (ImageFormat::Jpeg, "sample.jpg", "image/jpeg"),
        (ImageFormat::WebP, "sample.webp", "image/webp"),
        (ImageFormat::Bmp, "sample.bmp", "image/bmp"),
    ] {
        let options = options();
        let bytes = encoded(format);
        let output = block_on(convert_image(
            &input(bytes, name),
            &options,
            &Services::default(),
            &context(&options),
        ))
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(output.assets[0].media_type, media_type);
        assert_eq!(output.document.blocks.len(), 1);
    }
}

#[test]
fn multipage_tiff_preserves_original_and_emits_every_page() {
    let options = options();
    let output = block_on(convert_image(
        &input(multi_tiff(), "scan.tiff"),
        &options,
        &Services::default(),
        &context(&options),
    ))
    .unwrap();
    assert_eq!(output.document.blocks.len(), 2);
    assert_eq!(output.assets.len(), 3);
    assert_eq!(output.assets[0].media_type, "image/tiff");
    assert!(output.assets[1..].iter().all(|asset| asset.media_type == "image/png"));
}

#[test]
fn real_animated_webp_decodes_and_emits_every_frame() {
    let options = options();
    let output = block_on(convert_image(
        &input(animated_webp(), "animated.webp"),
        &options,
        &Services::default(),
        &context(&options),
    ))
    .unwrap();
    assert_eq!(output.document.blocks.len(), 2);
    assert_eq!(output.assets.len(), 3);
    assert_eq!(output.document.metadata.properties["image.animated"], "true");
}

#[test]
fn big_tiff_orientation_density_and_normalized_dimensions_are_preserved() {
    let options = options();
    let big = block_on(convert_image(
        &input(big_tiff(), "scan-big.tiff"),
        &options,
        &Services::default(),
        &context(&options),
    ))
    .unwrap();
    assert_eq!(big.document.blocks.len(), 1);

    let output = block_on(convert_image(
        &input(oriented_tiff(), "oriented.tiff"),
        &options,
        &Services::default(),
        &context(&options),
    ))
    .unwrap();
    assert_eq!(output.assets.len(), 2);
    let normalized = image::load_from_memory_with_format(&output.assets[1].bytes, ImageFormat::Png)
        .unwrap()
        .into_rgba8();
    assert_eq!(normalized.dimensions(), (3, 2));
    assert_eq!(output.document.metadata.properties["image.orientationApplied"], "6");
    assert_eq!(output.document.metadata.properties["image.dpiX"], "300.0000");
    assert_eq!(output.document.metadata.properties["image.dpiY"], "300.0000");
}

#[test]
fn structural_work_and_animated_frame_limits_fail_before_codec_entry() {
    for (bytes, name) in [
        (encoded(ImageFormat::Png), "chunks.png"),
        (encoded(ImageFormat::Jpeg), "markers.jpg"),
        (encoded(ImageFormat::WebP), "chunks.webp"),
    ] {
        let mut options = options();
        options.limits.max_archive_entries = 0;
        let context = context(&options);
        let error =
            block_on(convert_image(&input(bytes, name), &options, &Services::default(), &context))
                .unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit, "{name}: {error}");
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    let mut options = options();
    options.limits.max_pages = 1;
    let context = context(&options);
    let error = block_on(convert_image(
        &input(animated_webp_envelope(2), "animated.webp"),
        &options,
        &Services::default(),
        &context,
    ))
    .unwrap_err();
    assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn exact_envelopes_reject_trailing_bytes_and_invalid_png_structure() {
    for (format, name) in [
        (ImageFormat::Png, "trailing.png"),
        (ImageFormat::Jpeg, "trailing.jpg"),
        (ImageFormat::WebP, "trailing.webp"),
        (ImageFormat::Bmp, "trailing.bmp"),
    ] {
        let options = options();
        let mut bytes = encoded(format);
        bytes.push(0);
        let error = block_on(convert_image(
            &input(bytes, name),
            &options,
            &Services::default(),
            &context(&options),
        ))
        .unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Malformed, "{name}: {error}");
    }
    let options = options();
    let mut png = encoded(ImageFormat::Png);
    png[29] ^= 1;
    let error = block_on(convert_image(
        &input(png, "crc.png"),
        &options,
        &Services::default(),
        &context(&options),
    ))
    .unwrap_err();
    assert_eq!(error.code(), into_markdown_core::ErrorCode::Malformed);

    let reserved_bit_context = context(&options);
    let error = block_on(convert_image(
        &input(png_with_reserved_bit_set(), "reserved-bit.png"),
        &options,
        &Services::default(),
        &reserved_bit_context,
    ))
    .unwrap_err();
    assert_eq!(error.code(), into_markdown_core::ErrorCode::Malformed);
    assert_eq!(reserved_bit_context.reserved_memory_bytes(), 0);

    let mut tiff = multi_tiff();
    tiff.push(0);
    let error = block_on(convert_image(
        &input(tiff, "trailing.tiff"),
        &options,
        &Services::default(),
        &context(&options),
    ))
    .unwrap_err();
    assert_eq!(error.code(), into_markdown_core::ErrorCode::Malformed);
}

#[test]
fn reviewer_exact_tiff_and_bmp_gap_overlap_fixtures_fail_closed() {
    let fixtures = [
        (tiff_with_unowned_gap(131), "gap-131.tiff"),
        (tiff_with_pixel_ifd_overlap(135), "overlap-135.tiff"),
        (bmp_with_header_pixel_gap(), "gap-66.bmp"),
    ];
    assert_eq!(fixtures[0].0.len(), 131);
    assert_eq!(fixtures[1].0.len(), 135);
    assert_eq!(fixtures[2].0.len(), 66);
    for (bytes, name) in fixtures {
        let options = options();
        let context = context(&options);
        let error =
            block_on(convert_image(&input(bytes, name), &options, &Services::default(), &context))
                .unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Malformed, "{name}: {error}");
        assert_eq!(context.reserved_memory_bytes(), 0, "{name}");
    }
}

#[test]
fn tiff_density_failures_precede_decoder_entry_and_release_every_lease() {
    let cases = [
        (
            tiff_with_oversized_resolution_count(),
            options(),
            ExecutionOptions::default(),
            into_markdown_core::ErrorCode::Malformed,
        ),
        {
            let mut low = options();
            low.limits.max_memory_bytes = 0;
            (
                oriented_tiff(),
                low,
                ExecutionOptions::default(),
                into_markdown_core::ErrorCode::ResourceLimit,
            )
        },
        {
            let token = into_markdown_core::CancellationToken::new();
            token.cancel();
            (
                oriented_tiff(),
                options(),
                ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
                into_markdown_core::ErrorCode::Cancelled,
            )
        },
        (
            oriented_tiff(),
            options(),
            ExecutionOptions { timeout: Some(Duration::ZERO), ..ExecutionOptions::default() },
            into_markdown_core::ErrorCode::Timeout,
        ),
    ];
    for (bytes, options, execution, expected) in cases {
        decode::reset_decoder_entries();
        let context = ExecutionContext::new(execution, options.limits.clone());
        let error = block_on(convert_image(
            &input(bytes, "density.tiff"),
            &options,
            &Services::default(),
            &context,
        ))
        .unwrap_err();
        assert_eq!(error.code(), expected, "{error}");
        assert_eq!(decode::decoder_entries(), 0);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn frame_and_pixel_budgets_fail_before_materialization() {
    let bytes = multi_tiff();
    let mut page_options = options();
    page_options.limits.max_pages = 1;
    let error = block_on(convert_image(
        &input(bytes.clone(), "pages.tiff"),
        &page_options,
        &Services::default(),
        &context(&page_options),
    ))
    .unwrap_err();
    assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);

    let mut pixel_options = options();
    pixel_options.limits.max_decompressed_bytes = 1;
    let error = block_on(convert_image(
        &input(bytes, "pixels.tiff"),
        &pixel_options,
        &Services::default(),
        &context(&pixel_options),
    ))
    .unwrap_err();
    assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
}

#[test]
fn cancelled_large_envelope_returns_cancelled_without_a_lease() {
    let mut bytes = encoded(ImageFormat::Png);
    let iend = bytes.len() - 12;
    let mut text = Vec::with_capacity(8 * 1024 * 1024 + 12);
    text.extend_from_slice(&(8_u32 * 1024 * 1024).to_be_bytes());
    text.extend_from_slice(b"tEXt");
    text.resize(8 * 1024 * 1024 + 8, b'x');
    text.extend_from_slice(&crc32(&text[4..]).to_be_bytes());
    bytes.splice(iend..iend, text);
    let options = options();
    let token = into_markdown_core::CancellationToken::new();
    token.cancel();
    let context = ExecutionContext::new(
        ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let error = block_on(convert_image(
        &input(bytes, "large.png"),
        &options,
        &Services::default(),
        &context,
    ))
    .unwrap_err();
    assert_eq!(error.code(), into_markdown_core::ErrorCode::Cancelled);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

struct BoundOcr;

struct WorkingSetOcr;

struct RemoteOcr;

impl OcrEngine for RemoteOcr {
    fn id(&self) -> &'static str {
        "provider.fixture.vision-ocr"
    }

    fn provenance_kind(&self) -> ProvenanceKind {
        ProvenanceKind::AiProvider
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async {
            Ok(OcrResult {
                regions: vec![OcrRegion {
                    text: "remote text".into(),
                    polygon: [(0.0, 0.0); 4],
                    confidence: 0.0,
                }],
                provider: "provider.fixture.vision-ocr".into(),
            })
        })
    }

    fn planned_bound_output(
        &self,
        _: OcrRequest<'_>,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        OcrOutputPlan::try_new_with_working(16 * 1024, 4 * 1024, 1, 4096)
    }

    fn recognize_bound<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move { self.recognize(request, context).await.map(OcrRecognition::Remote) })
    }
}

#[test]
fn remote_vision_ocr_runs_when_local_ocr_is_off_without_fabricating_local_evidence() {
    let mut options = options();
    options.ai.vision_ocr = AiMode::Only;
    let services = Services { ocr: Some(Arc::new(RemoteOcr)), ..Services::default() };
    let context = context(&options);
    let output = block_on(convert_image(
        &input(encoded(ImageFormat::Png), "remote.png"),
        &options,
        &services,
        &context,
    ))
    .unwrap();
    let Block::Page { blocks, .. } = &output.document.blocks[0].block else {
        panic!("page expected")
    };
    let remote = blocks
        .iter()
        .find(|node| node.provenance.provider == "provider.fixture.vision-ocr")
        .expect("remote OCR paragraph");
    assert_eq!(remote.provenance.kind, ProvenanceKind::AiProvider);
    let Block::Paragraph(inlines) = &remote.block else { panic!("remote paragraph expected") };
    assert!(matches!(inlines.as_slice(), [Inline::Text { value, .. }] if value == "remote text"));
    assert_eq!(context.resource_usage().ocr_recognized_regions, 1);
    assert_eq!(context.resource_usage().ocr_recognized_chars, 11);
}

impl OcrEngine for WorkingSetOcr {
    fn id(&self) -> &'static str {
        "test.ocr.working-set"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async { unreachable!("working-set provider must use recognize_bound") })
    }

    fn planned_bound_output(
        &self,
        _: OcrRequest<'_>,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        OcrOutputPlan::try_new_with_working(4096, 8192, 1, 32)
    }

    fn recognize_bound<'a>(
        &'a self,
        _: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move {
            let _working = context.reserve_memory(8192)?;
            let bound = BoundOcrResult::try_new(
                OcrResult { regions: vec![], provider: "test.ocr.recognizer".into() },
                vec![],
                vec![
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Detection,
                        provider: "test.ocr.detector".into(),
                        model: Some("detector-sha256".into()),
                    },
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Recognition,
                        provider: "test.ocr.recognizer".into(),
                        model: Some("recognizer-sha256".into()),
                    },
                ],
            )?;
            Ok(OcrRecognition::Bound(bound))
        })
    }
}

#[test]
fn enclosing_credit_does_not_double_charge_provider_working_memory() {
    let mut options = options();
    options.ocr.policy = OcrPolicy::Always;
    options.limits.max_memory_bytes = 12_288;
    let root = context(&options);
    let mut parent = root.reserve_memory(12_288).unwrap();
    let credit = root.with_memory_credit(&mut parent).unwrap();
    let services = Services { ocr: Some(Arc::new(WorkingSetOcr)), ..Services::default() };

    let contribution =
        block_on(ocr::recognize(b"opaque-png", 1, 3, 2, &options, &services, &credit)).unwrap();

    assert!(contribution.memory.is_some());
    drop(contribution);
    assert_eq!(credit.reserved_memory_bytes(), 0);
}

impl OcrEngine for BoundOcr {
    fn id(&self) -> &'static str {
        "test.ocr.pipeline"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async { unreachable!("bound provider must use recognize_bound") })
    }

    fn planned_bound_output(
        &self,
        _: OcrRequest<'_>,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        OcrOutputPlan::try_new(16 * 1024, 16, 4096)
    }

    fn recognize_bound<'a>(
        &'a self,
        _: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            let result = OcrResult {
                regions: vec![OcrRegion {
                    text: "简体 Chinese and English".into(),
                    polygon: [(0.0, 0.0), (3.0, 0.0), (3.0, 2.0), (0.0, 2.0)],
                    confidence: 0.96,
                }],
                provider: "test.ocr.recognizer".into(),
            };
            let bound = BoundOcrResult::try_new(
                result,
                vec![0.98],
                vec![
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Detection,
                        provider: "test.ocr.detector".into(),
                        model: Some("detector-sha256".into()),
                    },
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Recognition,
                        provider: "test.ocr.recognizer".into(),
                        model: Some("recognizer-sha256".into()),
                    },
                ],
            )?;
            Ok(OcrRecognition::Bound(bound))
        })
    }
}

#[test]
fn bound_ocr_emits_exact_geometry_confidence_and_chain() {
    let mut options = options();
    options.ocr.policy = OcrPolicy::Always;
    let services = Services { ocr: Some(Arc::new(BoundOcr)), ..Services::default() };
    let context = context(&options);
    let output = block_on(convert_image(
        &input(encoded(ImageFormat::Png), "bound.png"),
        &options,
        &services,
        &context,
    ))
    .unwrap();
    let Block::Page { blocks, .. } = &output.document.blocks[0].block else {
        panic!("page expected")
    };
    let Block::Paragraph(inlines) = &blocks[1].block else { panic!("OCR paragraph expected") };
    let Inline::OcrText { value, evidence, .. } = &inlines[0] else {
        panic!("bound OCR inline expected")
    };
    assert!(value.contains("简体"));
    assert!((evidence.regions[0].detection_confidence - 0.98).abs() < f32::EPSILON);
    assert_eq!(evidence.chain.len(), 3);
    assert_eq!(evidence.chain[2].stage, OcrEvidenceStage::Merge);
    assert_eq!(context.resource_usage().ocr_recognized_regions, 1);
    assert_eq!(context.resource_usage().ocr_recognized_chars, 22);
}

#[test]
fn low_confidence_ocr_omission_is_degraded() {
    let mut options = options();
    options.ocr.policy = OcrPolicy::Always;
    options.ocr.minimum_confidence = 0.99;
    let services = Services { ocr: Some(Arc::new(BoundOcr)), ..Services::default() };
    let context = context(&options);
    let output = block_on(convert_image(
        &input(encoded(ImageFormat::Png), "low-confidence.png"),
        &options,
        &services,
        &context,
    ))
    .unwrap();
    let diagnostic = output
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "ocr.lowConfidence")
        .unwrap();
    assert_eq!(diagnostic.severity, DiagnosticSeverity::Warning);
    assert_eq!(conversion_outcome(&output.diagnostics), ConversionOutcome::Degraded);
    assert_eq!(context.resource_usage().ocr_recognized_regions, 0);
    assert_eq!(context.resource_usage().ocr_recognized_chars, 0);
}

struct LegacyOcr;

impl OcrEngine for LegacyOcr {
    fn id(&self) -> &'static str {
        "test.ocr.legacy"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async { Ok(OcrResult::default()) })
    }
}

#[test]
fn legacy_ocr_cannot_forge_structured_evidence() {
    let mut options = options();
    options.ocr.policy = OcrPolicy::Always;
    let services = Services { ocr: Some(Arc::new(LegacyOcr)), ..Services::default() };
    let error = block_on(convert_image(
        &input(encoded(ImageFormat::Png), "legacy.png"),
        &options,
        &services,
        &context(&options),
    ))
    .unwrap_err();
    assert_eq!(error.code(), into_markdown_core::ErrorCode::ComponentUnavailable);
}

#[derive(Clone, Copy)]
enum TestOcrPlan {
    Zero,
    Valid,
}

struct PlannedOcr {
    calls: Arc<AtomicUsize>,
    plan: TestOcrPlan,
    regions: usize,
    text_capacity: usize,
}

#[derive(Clone, Copy)]
enum ProviderExecutionFailure {
    Cancelled,
    Timeout,
    ResourceLimit,
    Ocr,
    ComponentUnavailable,
}

struct FailingOcr {
    failure: ProviderExecutionFailure,
}

impl OcrEngine for FailingOcr {
    fn id(&self) -> &'static str {
        "test.ocr.failing"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async { unreachable!("failing provider must use recognize_bound") })
    }

    fn planned_bound_output(
        &self,
        _: OcrRequest<'_>,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        OcrOutputPlan::try_new(4096, 1, 32)
    }

    fn recognize_bound<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move {
            Err(match self.failure {
                ProviderExecutionFailure::Cancelled => ConversionError::Cancelled,
                ProviderExecutionFailure::Timeout => ConversionError::Timeout,
                ProviderExecutionFailure::ResourceLimit => ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: "provider execution exceeded its budget".into(),
                },
                ProviderExecutionFailure::Ocr => ConversionError::Ocr {
                    provider: self.id().into(),
                    detail: "provider execution failed".into(),
                },
                ProviderExecutionFailure::ComponentUnavailable => {
                    ConversionError::ComponentUnavailable {
                        component: self.id().into(),
                        detail: "provider disappeared".into(),
                    }
                }
            })
        })
    }
}

#[test]
fn auto_ocr_degrades_only_provider_component_unavailability() {
    let mut options = options();
    options.ocr.policy = OcrPolicy::Auto;
    for (failure, expected) in [
        (ProviderExecutionFailure::Cancelled, into_markdown_core::ErrorCode::Cancelled),
        (ProviderExecutionFailure::Timeout, into_markdown_core::ErrorCode::Timeout),
        (ProviderExecutionFailure::ResourceLimit, into_markdown_core::ErrorCode::ResourceLimit),
        (ProviderExecutionFailure::Ocr, into_markdown_core::ErrorCode::Ocr),
    ] {
        let context = context(&options);
        let services =
            Services { ocr: Some(Arc::new(FailingOcr { failure })), ..Services::default() };
        let error = block_on(ocr::recognize(b"opaque-png", 1, 3, 2, &options, &services, &context))
            .unwrap_err();
        assert_eq!(error.code(), expected);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    let context = context(&options);
    let services = Services {
        ocr: Some(Arc::new(FailingOcr { failure: ProviderExecutionFailure::ComponentUnavailable })),
        ..Services::default()
    };
    let contribution =
        block_on(ocr::recognize(b"opaque-png", 1, 3, 2, &options, &services, &context)).unwrap();
    assert_eq!(contribution.diagnostics[0].code, "image.ocrUnavailable");
    assert!(!contribution.diagnostics[0].message.contains("setup ocr"));
    assert_eq!(context.reserved_memory_bytes(), 0);
}

struct PreflightFailingOcr {
    unavailable: bool,
}

#[test]
fn always_ocr_preserves_runtime_errors_without_installation_advice() {
    let mut options = options();
    options.ocr.policy = OcrPolicy::Always;
    for (failure, expected) in [
        (ProviderExecutionFailure::ResourceLimit, into_markdown_core::ErrorCode::ResourceLimit),
        (ProviderExecutionFailure::Ocr, into_markdown_core::ErrorCode::Ocr),
        (
            ProviderExecutionFailure::ComponentUnavailable,
            into_markdown_core::ErrorCode::ComponentUnavailable,
        ),
    ] {
        let context = context(&options);
        let services =
            Services { ocr: Some(Arc::new(FailingOcr { failure })), ..Services::default() };
        let error = block_on(ocr::recognize(b"opaque-png", 1, 3, 2, &options, &services, &context))
            .unwrap_err();
        assert_eq!(error.code(), expected);
        assert!(!error.to_string().contains("setup ocr"));
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

impl OcrEngine for PreflightFailingOcr {
    fn id(&self) -> &'static str {
        "test.ocr.preflight-failing"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async { unreachable!("preflight failure must prevent provider execution") })
    }

    fn planned_bound_output(
        &self,
        _: OcrRequest<'_>,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        if self.unavailable {
            Err(ConversionError::ComponentUnavailable {
                component: self.id().into(),
                detail: "provider disappeared during preflight".into(),
            })
        } else {
            Err(ConversionError::Ocr {
                provider: self.id().into(),
                detail: "provider preflight failed".into(),
            })
        }
    }
}

#[test]
fn auto_ocr_preflight_degrades_only_component_unavailability() {
    let mut options = options();
    options.ocr.policy = OcrPolicy::Auto;

    let failure_context = context(&options);
    let services = Services {
        ocr: Some(Arc::new(PreflightFailingOcr { unavailable: false })),
        ..Services::default()
    };
    let error =
        block_on(ocr::recognize(b"opaque-png", 1, 3, 2, &options, &services, &failure_context))
            .unwrap_err();
    assert_eq!(error.code(), into_markdown_core::ErrorCode::Ocr);
    assert_eq!(failure_context.reserved_memory_bytes(), 0);

    let context = context(&options);
    let services = Services {
        ocr: Some(Arc::new(PreflightFailingOcr { unavailable: true })),
        ..Services::default()
    };
    let contribution =
        block_on(ocr::recognize(b"opaque-png", 1, 3, 2, &options, &services, &context)).unwrap();
    assert_eq!(contribution.diagnostics[0].code, "image.ocrUnavailable");
    assert_eq!(context.reserved_memory_bytes(), 0);
}

impl OcrEngine for PlannedOcr {
    fn id(&self) -> &'static str {
        "test.ocr.planned"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async { unreachable!("planned provider must use recognize_bound") })
    }

    fn planned_bound_output(
        &self,
        _: OcrRequest<'_>,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        match self.plan {
            TestOcrPlan::Zero => OcrOutputPlan::try_new(0, 0, 0),
            TestOcrPlan::Valid => OcrOutputPlan::try_new(4096, 1, 32),
        }
    }

    fn recognize_bound<'a>(
        &'a self,
        _: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            context.checkpoint()?;
            let mut regions = Vec::with_capacity(self.regions);
            for _ in 0..self.regions {
                let mut text = String::with_capacity(self.text_capacity);
                text.push('x');
                regions.push(OcrRegion {
                    text,
                    polygon: [(0.0, 0.0), (3.0, 0.0), (3.0, 2.0), (0.0, 2.0)],
                    confidence: 0.9,
                });
            }
            let bound = BoundOcrResult::try_new(
                OcrResult { regions, provider: "test.ocr.recognizer".into() },
                vec![0.9; self.regions],
                vec![
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Detection,
                        provider: "test.ocr.detector".into(),
                        model: Some("detector-sha256".into()),
                    },
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Recognition,
                        provider: "test.ocr.recognizer".into(),
                        model: Some("recognizer-sha256".into()),
                    },
                ],
            )?;
            Ok(OcrRecognition::Bound(bound))
        })
    }
}

fn planned_ocr_services(engine: PlannedOcr) -> Services {
    Services { ocr: Some(Arc::new(engine)), ..Services::default() }
}

#[test]
fn ocr_preflight_rejects_zero_huge_and_low_memory_plans_without_leaks() {
    let mut options = options();
    options.ocr.policy = OcrPolicy::Always;
    for (plan, regions, text_capacity, memory, expected_calls) in [
        (TestOcrPlan::Zero, 1, 1, 1024 * 1024, 0),
        (TestOcrPlan::Valid, 2, 1, 1024 * 1024, 1),
        (TestOcrPlan::Valid, 1, 8192, 1024 * 1024, 1),
        (TestOcrPlan::Valid, 1, 1, 2048, 0),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let services = planned_ocr_services(PlannedOcr {
            calls: Arc::clone(&calls),
            plan,
            regions,
            text_capacity,
        });
        let mut limits = options.limits.clone();
        limits.max_memory_bytes = memory;
        let context = ExecutionContext::new(ExecutionOptions::default(), limits);
        let error = block_on(ocr::recognize(b"opaque-png", 1, 3, 2, &options, &services, &context))
            .unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit, "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn ocr_preflight_observes_precancel_and_deadline_before_provider_execution() {
    let mut options = options();
    options.ocr.policy = OcrPolicy::Always;
    let token = into_markdown_core::CancellationToken::new();
    token.cancel();
    let executions = [
        (
            ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
            into_markdown_core::ErrorCode::Cancelled,
        ),
        (
            ExecutionOptions { timeout: Some(Duration::ZERO), ..ExecutionOptions::default() },
            into_markdown_core::ErrorCode::Timeout,
        ),
    ];
    for (execution, expected) in executions {
        let calls = Arc::new(AtomicUsize::new(0));
        let services = planned_ocr_services(PlannedOcr {
            calls: Arc::clone(&calls),
            plan: TestOcrPlan::Valid,
            regions: 1,
            text_capacity: 1,
        });
        let context = ExecutionContext::new(execution, options.limits.clone());
        let error = block_on(ocr::recognize(b"opaque-png", 1, 3, 2, &options, &services, &context))
            .unwrap_err();
        assert_eq!(error.code(), expected);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

struct OpaqueInputOcr;

impl OcrEngine for OpaqueInputOcr {
    fn id(&self) -> &'static str {
        "test.ocr.opaque-input"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async { unreachable!("bound provider must use recognize_bound") })
    }

    fn planned_bound_output(
        &self,
        _: OcrRequest<'_>,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        OcrOutputPlan::try_new(16 * 1024, 16, 4096)
    }

    fn recognize_bound<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            let pixels = image::load_from_memory_with_format(request.image, ImageFormat::Png)
                .unwrap()
                .into_rgba8();
            assert!(pixels.pixels().all(|pixel| pixel.0[3] == 255));
            let bound = BoundOcrResult::try_new(
                OcrResult { regions: vec![], provider: "test.ocr.recognizer".into() },
                vec![],
                vec![
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Detection,
                        provider: "test.ocr.detector".into(),
                        model: Some("detector-sha256".into()),
                    },
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Recognition,
                        provider: "test.ocr.recognizer".into(),
                        model: Some("recognizer-sha256".into()),
                    },
                ],
            )?;
            Ok(OcrRecognition::Bound(bound))
        })
    }
}

#[test]
fn alpha_is_composited_for_ocr_and_invalid_confidence_is_rejected() {
    let mut options = options();
    options.ocr.policy = OcrPolicy::Always;
    let services = Services { ocr: Some(Arc::new(OpaqueInputOcr)), ..Services::default() };
    block_on(convert_image(
        &input(encoded(ImageFormat::Png), "alpha.png"),
        &options,
        &services,
        &context(&options),
    ))
    .unwrap();

    options.ocr.minimum_confidence = f32::NAN;
    let context = context(&options);
    let error = block_on(convert_image(
        &input(encoded(ImageFormat::Png), "nan.png"),
        &options,
        &services,
        &context,
    ))
    .unwrap_err();
    assert_eq!(error.code(), into_markdown_core::ErrorCode::Ocr);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

struct DescriptionAi {
    calls: Arc<AtomicUsize>,
    malicious: bool,
}

impl AiProvider for DescriptionAi {
    fn id(&self) -> &'static str {
        "test.ai.description"
    }

    fn capabilities(&self) -> BTreeSet<AiCapability> {
        BTreeSet::from([AiCapability::ImageDescription])
    }

    fn planned_output_bytes(
        &self,
        request: AiRequest<'_>,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        assert_eq!(request.capability, AiCapability::ImageDescription);
        assert!(matches!(
            request.input,
            AiInput::PageImage { media_type: "image/png", page: 1, .. }
        ));
        assert!(request.prompt.is_none());
        assert!(!options.network.enabled);
        context.checkpoint()?;
        Ok(8 * 1024)
    }

    fn execute_with_options<'a>(
        &'a self,
        request: AiRequest<'a>,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let malicious = self.malicious;
        Box::pin(async move {
            context.checkpoint()?;
            assert!(request.prompt.is_none());
            assert!(!options.network.enabled);
            let provenance = Provenance {
                kind: if malicious {
                    ProvenanceKind::NativeParser
                } else {
                    ProvenanceKind::AiProvider
                },
                provider: "test.ai.description".into(),
                locator: SourceLocator { page: Some(1), ..SourceLocator::default() },
                confidence: Some(0.8),
            };
            Ok(AiOutput {
                nodes: vec![BlockNode {
                    id: NodeId("image-page-1-ai-description".into()),
                    block: Block::Paragraph(vec![Inline::Text {
                        value: "A controlled description.".into(),
                        marks: vec![],
                    }]),
                    provenance,
                }],
                patch: None,
                diagnostics: vec![],
            })
        })
    }

    fn execute<'a>(
        &'a self,
        _: AiRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
        Box::pin(async {
            panic!("policy-bound image conversion must never call legacy AiProvider::execute")
        })
    }
}

#[test]
fn ai_modes_are_capability_bound_and_off_is_zero_call() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(DescriptionAi { calls: Arc::clone(&calls), malicious: false });
    let services = Services { ai: Some(provider), ..Services::default() };
    let bytes = encoded(ImageFormat::Png);

    let off_options = options();
    block_on(convert_image(
        &input(bytes.clone(), "off.png"),
        &off_options,
        &services,
        &context(&off_options),
    ))
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let mut fallback = options();
    fallback.ai.image_description = AiMode::Fallback;
    let output = block_on(convert_image(
        &input(bytes, "fallback.png"),
        &fallback,
        &services,
        &context(&fallback),
    ))
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let Block::Page { blocks, .. } = &output.document.blocks[0].block else { unreachable!() };
    assert!(blocks.iter().any(|node| node.id.0 == "image-page-1-ai-description"));

    let mut fallback_with_text = options();
    fallback_with_text.ocr.policy = OcrPolicy::Always;
    fallback_with_text.ai.image_description = AiMode::Fallback;
    let services =
        Services { ai: services.ai.clone(), ocr: Some(Arc::new(BoundOcr)), ..Services::default() };
    block_on(convert_image(
        &input(encoded(ImageFormat::Png), "text.png"),
        &fallback_with_text,
        &services,
        &context(&fallback_with_text),
    ))
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let mut prefer = options();
    prefer.ai.image_description = AiMode::Prefer;
    let services = Services {
        ai: Some(Arc::new(DescriptionAi { calls: Arc::clone(&calls), malicious: true })),
        ..Services::default()
    };
    let output = block_on(convert_image(
        &input(encoded(ImageFormat::Png), "prefer.png"),
        &prefer,
        &services,
        &context(&prefer),
    ))
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(output.diagnostics.iter().any(|value| value.code == "image.aiDescriptionFallback"));
}

#[test]
fn ai_only_rejects_absence_and_malicious_identity_transactionally() {
    let bytes = encoded(ImageFormat::Png);
    let mut options = options();
    options.ai.image_description = AiMode::Only;
    let error = block_on(convert_image(
        &input(bytes.clone(), "absent.png"),
        &options,
        &Services::default(),
        &context(&options),
    ))
    .unwrap_err();
    assert_eq!(error.code(), into_markdown_core::ErrorCode::ComponentUnavailable);

    let calls = Arc::new(AtomicUsize::new(0));
    let services = Services {
        ai: Some(Arc::new(DescriptionAi { calls, malicious: true })),
        ..Services::default()
    };
    let context = context(&options);
    let error =
        block_on(convert_image(&input(bytes, "malicious.png"), &options, &services, &context))
            .unwrap_err();
    assert_eq!(error.code(), into_markdown_core::ErrorCode::Ai);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut value = u32::MAX;
    for byte in bytes {
        value ^= u32::from(*byte);
        for _ in 0..8 {
            value = if value & 1 == 0 { value >> 1 } else { (value >> 1) ^ 0xedb8_8320 };
        }
    }
    !value
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

use super::*;
use into_markdown_core::SourceMetadata;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn backend_materialization_only_runs_after_exact_memory_permit() {
    let calls = AtomicUsize::new(0);
    let low = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits {
            max_memory_bytes: 31,
            ..into_markdown_core::ResourceLimits::default()
        },
    );
    assert!(matches!(
        materialize_after_reserve(&low, 32, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let exact = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits {
            max_memory_bytes: 32,
            ..into_markdown_core::ResourceLimits::default()
        },
    );
    let ((), permit) = materialize_after_reserve(&exact, 32, || {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(permit);
    assert_eq!(exact.reserved_memory_bytes(), 0);
}

#[test]
fn character_native_and_ir_peak_has_exact_permit_boundary_before_construction() {
    let count = 64_u32;
    let long_font_capacity = 4_096_usize;
    let font_bytes = long_font_capacity * usize::try_from(count).unwrap();
    let character_slots = usize::try_from(count).unwrap();
    let materialization = character_slots * std::mem::size_of::<Character>() + font_bytes;
    let required = character_working_set_bytes(
        u64::try_from(materialization).unwrap(),
        u64::try_from(font_bytes).unwrap(),
        count,
    )
    .unwrap();
    let info = PageInfo { width_points: 100.0, height_points: 200.0, rotation_degrees: 0 };
    let native_calls = AtomicUsize::new(0);
    let ir_calls = AtomicUsize::new(0);
    let materialize = || {
        native_calls.fetch_add(1, Ordering::SeqCst);
        let long_font = "F".repeat(4_096);
        Ok::<Vec<Character>, ConversionError>(
            (0..count)
                .map(|index| Character {
                    index,
                    value: 'x',
                    bounds: PdfRect { left: 1.0, bottom: 1.0, right: 2.0, top: 2.0 },
                    font_name: Some(long_font.clone()),
                    font_size: 12.0,
                    angle_degrees: 0.0,
                })
                .collect::<Vec<_>>(),
        )
    };
    let low = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits { max_memory_bytes: required - 1, ..Default::default() },
    );
    assert!(
        materialize_after_reserve(&low, required, || {
            let characters = materialize()?;
            ir_calls.fetch_add(1, Ordering::SeqCst);
            text_block(1, &info, &characters)
        })
        .is_err()
    );
    assert_eq!(native_calls.load(Ordering::SeqCst), 0);
    assert_eq!(ir_calls.load(Ordering::SeqCst), 0);

    let exact = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits { max_memory_bytes: required, ..Default::default() },
    );
    let (block, permit) = materialize_after_reserve(&exact, required, || {
        let characters = materialize()?;
        ir_calls.fetch_add(1, Ordering::SeqCst);
        text_block(1, &info, &characters)
    })
    .unwrap();
    assert_eq!(native_calls.load(Ordering::SeqCst), 1);
    assert_eq!(ir_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(block.block, Block::Paragraph(ref values) if values.len() == 64));
    drop(permit);
    assert_eq!(exact.reserved_memory_bytes(), 0);

    assert!(character_ir_allocation_bytes(0).unwrap() < character_ir_allocation_bytes(1).unwrap());
    assert!(character_ir_allocation_bytes(4).unwrap() < character_ir_allocation_bytes(5).unwrap());
}

#[test]
fn incremental_output_inventory_has_combined_exact_boundary_and_precedes_allocation() {
    fn account_fixture(
        context: &ExecutionContext,
        calls: &AtomicUsize,
    ) -> Result<Vec<ResourceReservation>, ConversionError> {
        let mut retained = Vec::new();
        for bytes in [
            output_block_overhead(2)?,
            output_block_overhead(0)?,
            asset_record_overhead()?,
            asset_record_overhead()?, // duplicate processing is still charged
            diagnostic_overhead()?,
            output_block_overhead(0)?,
        ] {
            retain_output_bytes(context, &mut retained, bytes)?;
            calls.fetch_add(1, Ordering::SeqCst);
        }
        Ok(retained)
    }

    let measuring = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits::default(),
    );
    let calls = AtomicUsize::new(0);
    let retained = account_fixture(&measuring, &calls).unwrap();
    let required = measuring.reserved_memory_bytes();
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    drop(retained);

    let exact = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits { max_memory_bytes: required, ..Default::default() },
    );
    calls.store(0, Ordering::SeqCst);
    let retained = account_fixture(&exact, &calls).unwrap();
    assert_eq!(exact.reserved_memory_bytes(), required);
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    let output = ConverterOutput::new_with_memory_reservations(
        Document::default(),
        Vec::new(),
        Vec::new(),
        retained,
    )
    .account_retained(&exact)
    .unwrap();
    assert_eq!(exact.reserved_memory_bytes(), required);
    drop(output);
    assert_eq!(exact.reserved_memory_bytes(), 0);

    let low = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits { max_memory_bytes: required - 1, ..Default::default() },
    );
    calls.store(0, Ordering::SeqCst);
    assert!(matches!(
        account_fixture(&low, &calls),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    assert_eq!(low.reserved_memory_bytes(), 0);
}

#[test]
fn coordinates_are_top_left_and_rotation_aware() {
    let rect = PdfRect { left: 10.0, bottom: 20.0, right: 30.0, top: 40.0 };
    let plain = PageInfo { width_points: 100.0, height_points: 200.0, rotation_degrees: 0 };
    assert_eq!(
        normalize_rect(rect, &plain).unwrap(),
        Rect { x: 10.0, y: 160.0, width: 20.0, height: 20.0 }
    );
    let rotated = PageInfo { width_points: 200.0, height_points: 100.0, rotation_degrees: 90 };
    assert_eq!(
        normalize_rect(rect, &rotated).unwrap(),
        Rect { x: 20.0, y: 10.0, width: 20.0, height: 20.0 }
    );
    let upside_down = PageInfo { rotation_degrees: 180, ..plain };
    assert_eq!(normalize_point(10.0, 20.0, &upside_down).unwrap(), (90.0, 20.0));
    let counter_clockwise =
        PageInfo { width_points: 200.0, height_points: 100.0, rotation_degrees: 270 };
    assert_eq!(normalize_point(10.0, 20.0, &counter_clockwise).unwrap(), (180.0, 90.0));
    assert_eq!(displayed_dimensions(&rotated), (200.0, 100.0));
    let extreme =
        PageInfo { width_points: f32::MAX, height_points: f32::MAX, rotation_degrees: 180 };
    assert!(
        normalize_rect(PdfRect { left: -f32::MAX, bottom: 0.0, right: 0.0, top: 1.0 }, &extreme)
            .is_err()
    );
}

#[test]
fn character_orientation_is_expressed_in_displayed_page_coordinates() {
    let character = Character {
        index: 0,
        value: 'x',
        bounds: PdfRect { left: 10.0, bottom: 20.0, right: 20.0, top: 30.0 },
        font_name: None,
        font_size: 12.0,
        angle_degrees: 0.0,
    };
    for rotation in [0_u16, 90, 180, 270] {
        let info = PageInfo {
            width_points: if matches!(rotation, 90 | 270) { 200.0 } else { 100.0 },
            height_points: if matches!(rotation, 90 | 270) { 100.0 } else { 200.0 },
            rotation_degrees: rotation,
        };
        assert_eq!(
            provenance(1, None, Some(&character), &info).unwrap().locator.rotation_degrees,
            Some(f32::from(rotation))
        );
    }

    let rotated_character = Character { angle_degrees: 270.0, ..character };
    let page = PageInfo { width_points: 200.0, height_points: 100.0, rotation_degrees: 90 };
    assert_eq!(
        provenance(1, None, Some(&rotated_character), &page).unwrap().locator.rotation_degrees,
        Some(0.0)
    );
}

#[test]
fn links_reject_dangerous_and_unrepresentable_targets() {
    assert!(safe_link_target(LinkTarget::ExternalUri("javascript:alert(1)".into()), 3).is_err());
    assert!(safe_link_target(LinkTarget::ExternalUri("https://example.test/a".into()), 3).is_ok());
    assert_eq!(
        safe_link_target(LinkTarget::InternalPage { page_index: 2 }, 3).unwrap(),
        "#pdf-page-3"
    );
    assert!(safe_link_target(LinkTarget::InternalPage { page_index: 3 }, 3).is_err());
}

#[test]
fn bitmap_validation_is_fail_closed_before_indexing() {
    let context = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits::default(),
    );
    let bitmap = ImageBitmap {
        width: 2,
        height: 2,
        stride: 8,
        format: PixelFormat::Bgra,
        bytes: vec![0; 15],
    };
    assert!(image_bitmap_to_bmp(&bitmap, &ConversionOptions::default(), &context).is_err());

    let short_stride =
        ImageBitmap { width: 2, height: 2, stride: 4, format: PixelFormat::Bgr, bytes: vec![0; 8] };
    assert!(image_bitmap_to_bmp(&short_stride, &ConversionOptions::default(), &context).is_err());
}

#[test]
fn bmp_output_memory_has_an_exact_boundary() {
    let bitmap = ImageBitmap {
        width: 2,
        height: 2,
        stride: 8,
        format: PixelFormat::Bgra,
        bytes: vec![0; 16],
    };
    for (limit, succeeds) in [(70, true), (69, false)] {
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: limit,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        assert_eq!(
            image_bitmap_to_bmp(&bitmap, &ConversionOptions::default(), &context).is_ok(),
            succeeds
        );
    }
}

#[test]
fn scan_coverage_is_union_based_and_blank_or_overlapping_small_images_do_not_qualify() {
    let info = PageInfo { width_points: 100.0, height_points: 100.0, rotation_degrees: 0 };
    let mut blank = PageCoverage::default();
    assert!(blank.ratio().abs() < f64::EPSILON);
    let small = Rect { x: 0.0, y: 0.0, width: 30.0, height: 30.0 };
    for _ in 0..100 {
        blank.add(small, &info);
    }
    assert!(blank.ratio() < MIN_SCAN_IMAGE_COVERAGE);
    blank.add(Rect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 }, &info);
    assert!((blank.ratio() - 1.0).abs() < f64::EPSILON);

    let mut below = PageCoverage::default();
    below.add(Rect { x: 0.0, y: 0.0, width: 49.9, height: 100.0 }, &info);
    assert!(below.ratio() < 0.5);
    let mut boundary = PageCoverage::default();
    boundary.add(Rect { x: 0.0, y: 0.0, width: 50.0, height: 100.0 }, &info);
    assert!((boundary.ratio() - 0.5).abs() < f64::EPSILON);
}

#[test]
fn conversion_gate_wait_honors_cancellation() {
    let held = PDF_CONVERSION_GATE.get_or_init(|| Mutex::new(())).lock().unwrap();
    let cancellation = into_markdown_core::CancellationToken::new();
    cancellation.cancel();
    let context = ExecutionContext::new(
        into_markdown_core::ExecutionOptions {
            cancellation,
            ..into_markdown_core::ExecutionOptions::default()
        },
        into_markdown_core::ResourceLimits::default(),
    );
    assert!(matches!(lock_pdf_conversion(&context), Err(ConversionError::Cancelled)));
    drop(held);
}

#[test]
fn total_asset_budget_counts_bytes_before_deduplication() {
    let mut options = ConversionOptions::default();
    options.limits.max_total_asset_bytes = 7;
    let mut total = 0;
    account_asset(b"same", &mut total, &options).unwrap();
    assert!(matches!(
        account_asset(b"same", &mut total, &options),
        Err(ConversionError::ResourceLimit { limit: "max_total_asset_bytes", .. })
    ));
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
#[allow(clippy::too_many_lines)]
fn native_production_converter_is_serialized_and_emits_unified_ir() {
    let path = PathBuf::from(std::env::var_os("PDFIUM_LIBRARY").expect("PDFIUM_LIBRARY"));
    let bytes: Arc<[u8]> = Arc::from(rotated_pdf());
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let bytes = Arc::clone(&bytes);
            handles.push(scope.spawn(move || {
                let input = ResolvedInput { bytes, metadata: SourceMetadata::default() };
                let context = ExecutionContext::new(
                    into_markdown_core::ExecutionOptions::default(),
                    into_markdown_core::ResourceLimits::default(),
                );
                let output = convert_pdf(&path, &input, &ConversionOptions::default(), &context)
                    .expect("both serialized conversions succeed");
                assert_eq!(output.document.blocks.len(), 4);
                assert!(!output.assets.is_empty());
                assert!(output.document.to_json().is_ok());
                let dimensions = output
                    .document
                    .blocks
                    .iter()
                    .map(|page| {
                        (
                            page.provenance.locator.page,
                            page.provenance.locator.page_width,
                            page.provenance.locator.page_height,
                            page.provenance.locator.rotation_degrees,
                        )
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    dimensions,
                    vec![
                        (Some(1), Some(100.0), Some(200.0), Some(0.0)),
                        (Some(2), Some(200.0), Some(100.0), Some(90.0)),
                        (Some(3), Some(100.0), Some(200.0), Some(180.0)),
                        (Some(4), Some(200.0), Some(100.0), Some(270.0)),
                    ]
                );
                let image_bounds = [
                    Rect { x: 10.0, y: 150.0, width: 20.0, height: 30.0 },
                    Rect { x: 20.0, y: 10.0, width: 30.0, height: 20.0 },
                    Rect { x: 70.0, y: 20.0, width: 20.0, height: 30.0 },
                    Rect { x: 150.0, y: 70.0, width: 30.0, height: 20.0 },
                ];
                let external_bounds = [
                    Rect { x: 10.0, y: 30.0, width: 30.0, height: 20.0 },
                    Rect { x: 150.0, y: 10.0, width: 20.0, height: 30.0 },
                    Rect { x: 60.0, y: 150.0, width: 30.0, height: 20.0 },
                    Rect { x: 30.0, y: 60.0, width: 20.0, height: 30.0 },
                ];
                let internal_bounds = [
                    Rect { x: 50.0, y: 80.0, width: 40.0, height: 20.0 },
                    Rect { x: 100.0, y: 50.0, width: 20.0, height: 40.0 },
                    Rect { x: 10.0, y: 100.0, width: 40.0, height: 20.0 },
                    Rect { x: 80.0, y: 10.0, width: 20.0, height: 40.0 },
                ];
                let Block::Page { blocks: first_blocks, .. } = &output.document.blocks[0].block
                else {
                    panic!("page")
                };
                let Block::Paragraph(first_inlines) = &first_blocks[0].block else {
                    panic!("text")
                };
                let Inline::SourceText { provenance, .. } = &first_inlines[0] else {
                    panic!("character")
                };
                let first_character_bounds = provenance.locator.bounds.unwrap();
                let raw_character = PdfRect {
                    left: first_character_bounds.x,
                    right: first_character_bounds.x + first_character_bounds.width,
                    top: 200.0 - first_character_bounds.y,
                    bottom: 200.0 - first_character_bounds.y - first_character_bounds.height,
                };
                for (index, page) in output.document.blocks.iter().enumerate() {
                    let Block::Page { blocks, .. } = &page.block else { panic!("page") };
                    let image = blocks
                        .iter()
                        .find(|block| matches!(block.block, Block::Image { .. }))
                        .unwrap();
                    assert_eq!(image.provenance.locator.bounds, Some(image_bounds[index]));
                    let Block::Paragraph(inlines) = &blocks[0].block else { panic!("text") };
                    let Inline::SourceText { provenance, .. } = &inlines[0] else {
                        panic!("character")
                    };
                    let character = provenance.locator.bounds.unwrap();
                    let info = PageInfo {
                        width_points: page.provenance.locator.page_width.unwrap(),
                        height_points: page.provenance.locator.page_height.unwrap(),
                        rotation_degrees: u16::try_from(index * 90).unwrap(),
                    };
                    assert_eq!(character, normalize_rect(raw_character, &info).unwrap());
                    let mut found_external = false;
                    let mut found_internal = false;
                    for block in blocks {
                        let Block::Paragraph(inlines) = &block.block else { continue };
                        let Some(Inline::Link { target, .. }) = inlines.first() else {
                            continue;
                        };
                        if target == "https://example.test/rotated" {
                            assert_eq!(
                                block.provenance.locator.bounds,
                                Some(external_bounds[index])
                            );
                            found_external = true;
                        } else if target == "#pdf-page-2" {
                            assert_eq!(
                                block.provenance.locator.bounds,
                                Some(internal_bounds[index])
                            );
                            found_internal = true;
                        }
                    }
                    assert!(found_external && found_internal);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
    });

    let input =
        ResolvedInput { bytes: Arc::from(rotated_pdf()), metadata: SourceMetadata::default() };
    let context = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits::default(),
    );
    let mut always = ConversionOptions::default();
    always.ocr.policy = OcrPolicy::Always;
    let output = convert_pdf(&path, &input, &always, &context).unwrap();
    assert!(output.assets.iter().any(|asset| asset.id.0.starts_with("pdf-page-render-")));

    let mut off = ConversionOptions::default();
    off.ocr.policy = OcrPolicy::Off;
    let output = convert_pdf(&path, &input, &off, &context).unwrap();
    assert!(!output.assets.iter().any(|asset| asset.id.0.starts_with("pdf-page-render-")));
    let markdown =
        into_markdown_render_markdown::render(&output.document, &output.assets, &off).unwrap();
    assert!(markdown.contains("https://example.test/rotated"));
    assert!(markdown.contains("#pdf-page-2"));
    assert!(markdown.contains("<a id=\"pdf-page-2\"></a>"));

    let auto = ConversionOptions::default();
    for (fixture, rendered) in
        [(text_only_pdf(), false), (mixed_pdf(), false), (scanned_pdf(), true)]
    {
        let input =
            ResolvedInput { bytes: Arc::from(fixture), metadata: SourceMetadata::default() };
        let output = convert_pdf(&path, &input, &auto, &context).unwrap();
        assert_eq!(
            output.assets.iter().any(|asset| asset.id.0.starts_with("pdf-page-render-")),
            rendered
        );
    }

    let modest = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits {
            max_memory_bytes: 64 * 1024 * 1024,
            ..into_markdown_core::ResourceLimits::default()
        },
    );
    assert!(convert_pdf(&path, &input, &auto, &modest).is_ok());

    let mut page_limited = ConversionOptions::default();
    page_limited.limits.max_pages = 3;
    assert!(matches!(
        convert_pdf(&path, &input, &page_limited, &context),
        Err(ConversionError::ResourceLimit { limit: "max_pages", .. })
    ));

    let low = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits {
            max_memory_bytes: 1,
            ..into_markdown_core::ResourceLimits::default()
        },
    );
    assert!(matches!(
        convert_pdf(&path, &input, &ConversionOptions::default(), &low),
        Err(ConversionError::ResourceLimit { .. })
    ));
    let damaged = ResolvedInput {
        bytes: Arc::from(b"%PDF-1.4\nbroken".as_slice()),
        metadata: SourceMetadata::default(),
    };
    assert!(matches!(
        convert_pdf(&path, &damaged, &ConversionOptions::default(), &context),
        Err(ConversionError::Malformed { .. })
    ));
    let encrypted =
        ResolvedInput { bytes: Arc::from(encrypted_pdf()), metadata: SourceMetadata::default() };
    assert!(matches!(
        convert_pdf(&path, &encrypted, &ConversionOptions::default(), &context),
        Err(ConversionError::Encrypted)
    ));
}

fn rotated_pdf() -> Vec<u8> {
    let content = b"BT /F1 12 Tf 10 160 Td (Rotated) Tj ET\nq 20 0 0 30 10 20 cm /Im1 Do Q\n";
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

fn text_only_pdf() -> Vec<u8> {
    one_page_fixture(b"BT /F1 12 Tf 10 160 Td (Text only page) Tj ET\n", false)
}

fn mixed_pdf() -> Vec<u8> {
    one_page_fixture(
        b"BT /F1 12 Tf 10 160 Td (Mixed page text) Tj ET\nq 20 0 0 30 10 20 cm /Im1 Do Q\n",
        true,
    )
}

fn scanned_pdf() -> Vec<u8> {
    one_page_fixture(b"q 100 0 0 200 0 0 cm /Im1 Do Q\n", true)
}

fn encrypted_pdf() -> Vec<u8> {
    let content = b"BT /F1 12 Tf 10 160 Td (Encrypted) Tj ET\n";
    let objects = [
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            stream_object("", content),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            b"<< /Filter /Standard /V 1 /R 2 /O <0000000000000000000000000000000000000000000000000000000000000000> /U <0000000000000000000000000000000000000000000000000000000000000000> /P -4 >>".to_vec(),
        ];
    assemble_pdf_with_trailer(
        &objects,
        "/Encrypt 6 0 R /ID [<00112233445566778899aabbccddeeff><00112233445566778899aabbccddeeff>]",
    )
}

fn one_page_fixture(content: &[u8], image: bool) -> Vec<u8> {
    let resources = if image {
        "<< /Font << /F1 5 0 R >> /XObject << /Im1 6 0 R >> >>"
    } else {
        "<< /Font << /F1 5 0 R >> >>"
    };
    let mut objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Resources {resources} /Contents 4 0 R >>").into_bytes(),
            stream_object("", content),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ];
    if image {
        objects.push(stream_object("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8", &[255, 0, 0]));
    }
    assemble_pdf(&objects)
}

fn stream_object(dictionary: &str, bytes: &[u8]) -> Vec<u8> {
    let mut object = format!("<< {dictionary} /Length {} >>\nstream\n", bytes.len()).into_bytes();
    object.extend_from_slice(bytes);
    object.extend_from_slice(b"\nendstream");
    object
}

fn assemble_pdf(objects: &[Vec<u8>]) -> Vec<u8> {
    assemble_pdf_with_trailer(objects, "")
}

fn assemble_pdf_with_trailer(objects: &[Vec<u8>], extra: &str) -> Vec<u8> {
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
            "trailer\n<< /Size {} /Root 1 0 R {extra} >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1,
        )
        .as_bytes(),
    );
    pdf
}

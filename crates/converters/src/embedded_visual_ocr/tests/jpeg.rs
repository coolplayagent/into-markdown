use super::*;
use into_markdown_core::ErrorPolicy;

fn asset(tail: &[u8]) -> Asset {
    let pixels = image::RgbImage::from_fn(3, 2, |x, y| {
        image::Rgb([u8::try_from(x * 70).unwrap(), u8::try_from(y * 110).unwrap(), 40])
    });
    let mut buffer = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(pixels).write_to(&mut buffer, ImageFormat::Jpeg).unwrap();
    let mut bytes = buffer.into_inner();
    bytes.extend_from_slice(tail);
    Asset {
        id: AssetId("jpeg".into()),
        filename: None,
        media_type: "image/jpeg".into(),
        bytes,
        external_uri: None,
    }
}

fn source(asset: &Asset) -> ConverterOutput {
    let mut result = output();
    for target in &mut result.assets {
        target.bytes.clone_from(&asset.bytes);
        target.media_type.clone_from(&asset.media_type);
        target.filename = None;
    }
    result
}

fn context(options: &ConversionOptions) -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), options.limits.clone())
}

#[test]
fn jpeg_trailing_bytes_preserve_pixels_and_assets_with_located_diagnostics() {
    let options = ConversionOptions::default();
    let plain = asset(&[]);
    let expected = normalize(&plain, &options, &context(&options)).unwrap();
    for tail in [vec![0], vec![0xa5; 17], vec![0xff; 4097], plain.bytes.clone()] {
        let original = asset(&tail);
        let normalized = normalize(&original, &options, &context(&options)).unwrap();
        assert_eq!(normalized.bytes, expected.bytes);
        assert_eq!((normalized.width, normalized.height), (3, 2));
        assert_eq!(normalized.trailing_bytes, tail.len());
    }
    for policy in [OcrPolicy::Auto, OcrPolicy::Always] {
        let original = asset(&[0xa5; 17]);
        let before = source(&original);
        let expected_assets = before.assets.clone();
        let ocr = source_bound_ocr(false);
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut options = options.clone();
        options.ocr.policy = policy;
        let context = context(&options);
        assert!(matches!(
            plan_enrichment(&before, InputFormat::Odp, &options, &services, &context).unwrap(),
            EnrichmentPlan::Reserve(_)
        ));
        let after =
            block_on(enrich(before, InputFormat::Odp, &options, &services, &context)).unwrap();
        assert_eq!(after.assets, expected_assets);
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 1);
        let Block::Page { blocks, .. } = &after.document.blocks[0].block else {
            panic!("expected the original page")
        };
        assert_eq!(blocks.len(), 4);
        let warnings: Vec<_> = after
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "embeddedVisualOcr.jpegTrailingData")
            .collect();
        assert_eq!(warnings.len(), 2);
        for warning in warnings {
            assert!(warning.message.contains("17 trailing byte(s)"));
            assert_eq!(warning.locator.as_ref().unwrap().page, Some(2));
        }
    }
}

#[test]
fn jpeg_strict_and_standalone_keep_exact_eof_contract() {
    let original = asset(&[0xa5; 17]);
    let mut options = ConversionOptions::default();
    options.ocr.policy = OcrPolicy::Auto;
    for policy in [ErrorPolicy::BestEffort, ErrorPolicy::Strict] {
        options.error_policy = policy;
        assert!(matches!(
            envelope::validate(
                format::RasterFormat::Jpeg,
                &original.bytes,
                &options.limits,
                &context(&options)
            ),
            Err(ConversionError::Malformed { .. })
        ));
    }
    let ocr = source_bound_ocr(false);
    let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
    assert!(matches!(
        plan_enrichment(
            &source(&original),
            InputFormat::Odp,
            &options,
            &services,
            &context(&options)
        ),
        Err(ConversionError::Malformed { .. })
    ));
    assert!(matches!(
        normalize(&original, &options, &context(&options)),
        Err(ConversionError::Malformed { .. })
    ));
    assert_eq!(ocr.plans.load(Ordering::SeqCst), 0);
    assert_eq!(ocr.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn jpeg_marker_payload_is_not_a_terminator_and_bad_pixels_are_not_recovered() {
    let options = ConversionOptions::default();
    let mut original = asset(&[0xa5; 17]);
    // APP data contains marker-like bytes; only the actual marker stream delimits JPEG.
    original.bytes.splice(2..2, [0xff, 0xe2, 0, 6, 0xff, 0xd9, 0xff, 0xd8]);
    assert_eq!(normalize(&original, &options, &context(&options)).unwrap().trailing_bytes, 17);
    let mut truncated = asset(&[]);
    truncated.bytes.truncate(truncated.bytes.len() - 2);
    let mut bad_pixels = asset(&[0xa5; 17]);
    let quantization = bad_pixels.bytes.windows(2).position(|pair| pair == [0xff, 0xdb]).unwrap();
    bad_pixels.bytes[quantization + 4] = 0xf0; // Impossible quantization precision, intact envelope.
    assert!(jpeg_input::envelope(&bad_pixels.bytes, &options, &context(&options)).is_ok());
    let ocr = source_bound_ocr(false);
    let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
    for original in [truncated, bad_pixels] {
        for policy in [OcrPolicy::Auto, OcrPolicy::Always] {
            let mut options = options.clone();
            options.ocr.policy = policy;
            assert!(matches!(
                block_on(enrich(
                    source(&original),
                    InputFormat::Odp,
                    &options,
                    &services,
                    &context(&options)
                )),
                Err(ConversionError::Malformed { .. })
            ));
        }
    }
    assert_eq!(ocr.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn jpeg_compatibility_preserves_limits_cancellation_and_timeout() {
    let original = asset(&[0xa5; 17]);
    for limit in ["max_pages", "image_chunks", "max_decompressed_bytes", "max_memory_bytes"] {
        let mut options = ConversionOptions::default();
        match limit {
            "max_pages" => options.limits.max_pages = 0,
            "image_chunks" => options.limits.max_archive_entries = 0,
            "max_decompressed_bytes" => options.limits.max_decompressed_bytes = 1,
            _ => options.limits.max_memory_bytes = 1,
        }
        assert!(
            matches!(
                normalize(&original, &options, &context(&options)),
                Err(ConversionError::ResourceLimit { .. })
            ),
            "{limit}"
        );
    }
    let options = ConversionOptions::default();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = ExecutionContext::new(
        ExecutionOptions { cancellation, ..ExecutionOptions::default() },
        options.limits.clone(),
    );
    assert!(matches!(normalize(&original, &options, &cancelled), Err(ConversionError::Cancelled)));
    let timed_out = ExecutionContext::new(
        ExecutionOptions {
            timeout: Some(std::time::Duration::ZERO),
            ..ExecutionOptions::default()
        },
        options.limits.clone(),
    );
    assert!(matches!(normalize(&original, &options, &timed_out), Err(ConversionError::Timeout)));
}

#[test]
fn jpeg_compatibility_diagnostic_survives_auto_provider_unavailability() {
    let original = asset(&[0xa5; 17]);
    let mut options = ConversionOptions::default();
    options.ocr.policy = OcrPolicy::Auto;
    let after = block_on(enrich(
        source(&original),
        InputFormat::Odp,
        &options,
        &Services::default(),
        &context(&options),
    ))
    .unwrap();
    assert_eq!(
        after
            .diagnostics
            .iter()
            .filter(|diagnostic| { diagnostic.code == "embeddedVisualOcr.jpegTrailingData" })
            .count(),
        2
    );
}

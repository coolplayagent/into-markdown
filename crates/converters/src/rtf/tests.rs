use super::{RtfConverter, convert_rtf_bytes, strict_header};
use into_markdown_core::*;
use std::fmt::Write as _;
use std::mem::size_of;
use std::sync::Arc;

fn convert(bytes: &[u8]) -> Result<ConverterOutput, ConversionError> {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    convert_rtf_bytes(bytes, &options, &context)
}

fn paragraph_text(output: &ConverterOutput) -> String {
    output
        .document
        .blocks
        .iter()
        .filter_map(|node| match &node.block {
            Block::Paragraph(inlines) => Some(inlines),
            _ => None,
        })
        .flatten()
        .filter_map(|inline| match inline {
            Inline::Text { value, .. } => Some(value.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn strict_probe_rejects_plain_text_prefixes() {
    assert!(strict_header(b"{\\rtf1\\ansi ok}").is_some());
    assert!(strict_header(b"{\\rtf\\ansi no version}").is_none());
    assert!(strict_header(b"prefix {\\rtf1 no}").is_none());
    assert!(strict_header(b"{\\rtf1x no delimiter}").is_none());
}

#[test]
fn styles_unicode_hex_and_spans_are_preserved() {
    let output = convert(b"{\\rtf1\\ansi A {\\b bold} \\u20013? \\'e9\\par}").unwrap();
    assert_eq!(paragraph_text(&output), "A bold 中 é");
    let node = &output.document.blocks[0];
    assert_eq!(node.provenance.locator.byte_start, Some(12));
    assert!(
        node.provenance.locator.byte_end.unwrap() > node.provenance.locator.byte_start.unwrap()
    );
    let Block::Paragraph(inlines) = &node.block else { panic!("paragraph") };
    assert!(inlines.iter().any(
        |inline| matches!(inline, Inline::Text { marks, .. } if marks.contains(&InlineMark::Bold))
    ));
}

#[test]
fn codepages_font_charset_surrogates_and_unicode_fallback_are_deterministic() {
    let chinese = convert(
            b"{\\rtf1\\ansi\\ansicpg1252{\\fonttbl{\\f0\\fcharset134 SimSun;}}\\f0 \\'d6\\'d0\\'ce\\'c4 \\uc2\\u-10179a\\~\\u-8704cd\\par}",
        )
        .unwrap();
    assert_eq!(paragraph_text(&chinese), "中文 😀");
}

#[test]
fn active_destinations_are_skipped() {
    let output =
        convert(b"{\\rtf1\\ansi before{\\object{\\*\\objdata 0102}{\\result BAD}}after\\par}")
            .unwrap();
    assert_eq!(paragraph_text(&output), "beforeafter");
    assert!(output.assets.is_empty());
}

#[test]
fn malformed_and_limits_are_stable() {
    assert_eq!(convert(b"{\\rtf1\\ansi no close").unwrap_err().code(), ErrorCode::Malformed);
    assert_eq!(convert(b"{\\rtf1\\u99999999999?}").unwrap_err().code(), ErrorCode::ResourceLimit);
    let mut options = ConversionOptions::default();
    options.limits.max_nesting_depth = 2;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert_eq!(
        convert_rtf_bytes(b"{\\rtf1{{x}}}", &options, &context).unwrap_err().code(),
        ErrorCode::ResourceLimit
    );
}

#[test]
fn table_list_and_safe_field_map_to_structured_ir() {
    let table =
        convert(b"{\\rtf1\\ansi\\trowd\\intbl A\\cell B\\cell\\row\\pard after\\par}").unwrap();
    assert!(matches!(table.document.blocks[0].block, Block::Table { .. }));
    assert!(matches!(table.document.blocks[1].block, Block::Paragraph(_)));

    let span = convert(
        b"{\\rtf1\\ansi\\trowd\\clmgf\\cellx1000\\clmrg\\cellx2000\\intbl merged\\cell\\cell\\row}",
    )
    .unwrap();
    let Block::Table { rows, .. } = &span.document.blocks[0].block else { panic!("table") };
    assert_eq!(rows[0].cells.len(), 1);
    assert_eq!(rows[0].cells[0].column_span, 2);

    let list = convert(b"{\\rtf1\\ansi{\\listtext\\bullet\\tab}Item\\par}").unwrap();
    assert!(matches!(list.document.blocks[0].block, Block::List { kind: ListKind::Bullet, .. }));

    let link = convert(b"{\\rtf1\\ansi before{\\field{\\*\\fldinst HYPERLINK \\\"https://example.invalid/path\\\"}{\\fldrslt safe}}after\\par}").unwrap();
    let Block::Paragraph(inlines) = &link.document.blocks[0].block else {
        panic!("field result paragraph")
    };
    assert!(matches!(
        &inlines[1],
        Inline::Link { target, .. } if target == "https://example.invalid/path"
    ));

    let secret = convert(b"{\\rtf1\\ansi{\\field{\\*\\fldinst HYPERLINK \\\"https://user:secret@example.invalid/path?token=x\\\"}{\\fldrslt label}}}").unwrap();
    assert!(
        secret.diagnostics.iter().any(|diagnostic| diagnostic.code == "rtf.unsafeHyperlinkSkipped")
    );
    let Block::Paragraph(inlines) = &secret.document.blocks[0].block else {
        panic!("unsafe link fallback paragraph")
    };
    assert!(matches!(&inlines[0], Inline::Text { value, .. } if value == "label"));
}

#[test]
fn png_picture_is_decoded_and_retained_but_vector_is_not() {
    let png = "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c6360606060000000050001a5f645400000000049454e44ae426082";
    let source = format!("{{\\rtf1\\ansi{{\\pict\\pngblip {png}}}}}");
    let output = convert(source.as_bytes()).unwrap();
    assert_eq!(output.assets.len(), 1);
    assert_eq!(output.assets[0].media_type, "image/png");
    assert!(matches!(output.document.blocks[0].block, Block::Image { .. }));

    let vector = convert(b"{\\rtf1\\ansi{\\pict\\emfblip 0102}}").unwrap();
    assert!(vector.assets.is_empty());
    assert!(
        vector.diagnostics.iter().any(|diagnostic| diagnostic.code == "rtf.unsupportedVectorImage")
    );
}

#[test]
fn cancellation_and_trailing_payload_are_controlled_errors() {
    let mut options = ConversionOptions::default();
    let cancellation = into_markdown_core::CancellationToken::new();
    cancellation.cancel();
    let context = ExecutionContext::new(
        ExecutionOptions { cancellation, ..ExecutionOptions::default() },
        options.limits.clone(),
    );
    let error = convert_rtf_bytes(b"{\\rtf1 text}", &options, &context).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Cancelled);
    options.limits.max_input_bytes = 1024;
    assert_eq!(convert(b"{\\rtf1 text}payload").unwrap_err().code(), ErrorCode::Malformed);
}

#[test]
fn probe_is_not_extension_only() {
    let converter = RtfConverter;
    let input =
        ResolvedInput { metadata: SourceMetadata::default(), bytes: Arc::from(&b"not rtf"[..]) };
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits);
    let result = futures_lite_for_test(converter.probe(
        &input,
        &FormatCandidate::new(InputFormat::Rtf, 0.55, "extension"),
        &context,
    ));
    assert_eq!(result.unwrap(), ProbeOutcome::NotApplicable);
}

#[test]
fn font_table_is_bounded_deduplicated_and_binary_searched() {
    let duplicate = convert(
            b"{\\rtf1\\ansi{\\fonttbl{\\f7\\fcharset0 Arial;}{\\f7\\fcharset134 SimSun;}}\\f7 \\'d6\\'d0\\'ce\\'c4\\par}",
        )
        .unwrap();
    assert_eq!(paragraph_text(&duplicate), "中文");

    let mut source = String::from("{\\rtf1\\ansi{\\fonttbl");
    for font in 0..=super::parser::MAX_RTF_FONTS {
        write!(&mut source, "{{\\f{font}\\fcharset0 F;}}").unwrap();
    }
    source.push_str("}x}");
    let error = convert(source.as_bytes()).unwrap_err();
    assert!(matches!(error, ConversionError::ResourceLimit { limit: "rtf_font_count", .. }));
}

#[test]
fn font_vector_fails_under_low_memory_and_releases_same_context_lease() {
    let mut source = String::from("{\\rtf1\\ansi{\\fonttbl");
    for font in 0..256 {
        write!(&mut source, "{{\\f{font}\\fcharset0 F;}}").unwrap();
    }
    source.push_str("}x}");
    let mut options = ConversionOptions::default();
    options.limits.max_memory_bytes = 1024;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let error = convert_rtf_bytes(source.as_bytes(), &options, &context).unwrap_err();
    assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
    assert_eq!(context.reserved_memory_bytes(), 0);

    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let output = convert_rtf_bytes(b"{\\rtf1 same context}", &options, &context).unwrap();
    assert!(output.leased_memory_for(&context) > 0);
    drop(output);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn font_vector_capacity_is_charged_exactly_and_fails_before_growth() {
    use super::budget::reserve_vec;
    use super::parser::FontCharset;

    let limits = ResourceLimits {
        max_memory_bytes: u64::try_from(size_of::<FontCharset>() - 1).unwrap(),
        ..ResourceLimits::default()
    };
    let context = ExecutionContext::new(ExecutionOptions::default(), limits);
    let mut memory = context.reserve_memory(0).unwrap();
    let mut fonts = Vec::<FontCharset>::new();
    let error = reserve_vec(&mut fonts, 1, &mut memory).unwrap_err();
    assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
    assert_eq!(fonts.capacity(), 0);
    assert_eq!(context.reserved_memory_bytes(), 0);

    let limits = ResourceLimits { max_memory_bytes: 1024, ..ResourceLimits::default() };
    let context = ExecutionContext::new(ExecutionOptions::default(), limits);
    let mut memory = context.reserve_memory(0).unwrap();
    let mut fonts = Vec::<FontCharset>::new();
    reserve_vec(&mut fonts, 1, &mut memory).unwrap();
    let actual =
        u64::try_from(fonts.capacity().checked_mul(size_of::<FontCharset>()).unwrap()).unwrap();
    assert_eq!(context.reserved_memory_bytes(), actual);
}

fn futures_lite_for_test<T>(mut future: BoxFuture<'_, T>) -> T {
    use std::task::{Context, Poll, Waker};
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test future unexpectedly pending"),
    }
}

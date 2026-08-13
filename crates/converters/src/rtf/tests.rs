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
fn safe_destination_containers_allow_only_their_documented_children() {
    let output = convert(
        b"{\\rtf1\\ansi{\\info{\\title Good}{\\author Writer}{\\object{\\title BAD}}{\\*\\unknown{\\author BAD}}}body\\par}",
    )
    .unwrap();
    assert_eq!(output.document.metadata.title.as_deref(), Some("Good"));
    assert_eq!(output.document.metadata.authors, ["Writer"]);
    assert_eq!(paragraph_text(&output), "body");

    let wrong_scope = convert(
        b"{\\rtf1\\ansi{\\title BAD}{\\info{\\title before{\\shppict{\\pict\\pngblip 00}}after}}safe}",
    )
    .unwrap();
    assert_eq!(wrong_scope.document.metadata.title.as_deref(), Some("beforeafter"));
    assert!(wrong_scope.assets.is_empty());

    let dangerous = convert(
        b"{\\rtf1\\ansi{\\object{\\info{\\title BAD}}{\\shppict{\\pict\\pngblip 00}}}safe\\par}",
    )
    .unwrap();
    assert!(dangerous.document.metadata.title.is_none());
    assert!(dangerous.assets.is_empty());
    assert_eq!(paragraph_text(&dangerous), "safe");
}

#[test]
fn skipped_descendants_stay_skipped_and_bin_bytes_are_structurally_opaque() {
    let output = convert(
        b"{\\rtf1\\ansi before{\\object{\\title BAD}{\\pict\\pngblip 00}{\\fldrslt BAD}\\bin3 {}\\}after\\par}",
    )
    .unwrap();
    assert_eq!(paragraph_text(&output), "beforeafter");
    assert!(output.document.metadata.title.is_none());
    assert!(output.assets.is_empty());

    let metadata = convert(b"{\\rtf1\\ansi{\\info{\\title hi\\bin3 {}\\there}}body\\par}").unwrap();
    assert_eq!(metadata.document.metadata.title.as_deref(), Some("hithere"));
    assert_eq!(paragraph_text(&metadata), "body");

    let body = convert(b"{\\rtf1\\ansi before\\bin3 {}\\after\\par}").unwrap();
    assert_eq!(paragraph_text(&body), "beforeafter");
    for malformed in
        [b"{\\rtf1\\ansi \\bin-1 x}".as_slice(), b"{\\rtf1\\ansi \\bin4 xx}".as_slice()]
    {
        assert!(matches!(
            convert(malformed).unwrap_err(),
            ConversionError::ResourceLimit { limit: "rtf_binary_bytes", .. }
                | ConversionError::Malformed { .. }
        ));
    }
}

#[test]
fn dbcs_trails_that_resemble_rtf_syntax_remain_atomic() {
    for (codepage, pair, encoding) in [
        (932_u16, [0x81_u8, b'\\'], encoding_rs::SHIFT_JIS),
        (936_u16, [0x81_u8, b'{'], encoding_rs::GBK),
        (950_u16, [0xa4_u8, b'}'], encoding_rs::BIG5),
    ] {
        let mut source = format!("{{\\rtf1\\ansi\\ansicpg{codepage} ").into_bytes();
        source.extend_from_slice(&pair);
        source.extend_from_slice(b"\\par}");
        let output = convert(&source).unwrap();
        let (expected, _, malformed) = encoding.decode(&pair);
        assert!(!malformed);
        assert_eq!(paragraph_text(&output), expected);
    }

    let mut fallback = b"{\\rtf1\\ansi\\ansicpg932\\uc1\\u65 ".to_vec();
    fallback.extend_from_slice(&[0x81, b'\\']);
    fallback.extend_from_slice(b"X}");
    assert_eq!(paragraph_text(&convert(&fallback).unwrap()), "AX");
}

#[test]
fn text_destinations_reject_controls_but_tab_and_line_are_structured() {
    for mut source in [
        b"{\\rtf1\\ansi raw".to_vec(),
        b"{\\rtf1\\ansi\\ansicpg1252 raw".to_vec(),
        b"{\\rtf1\\ansi raw".to_vec(),
    ]
    .into_iter()
    .zip([0x00_u8, 0x81, 0x7f])
    .map(|(mut source, byte)| {
        source.push(byte);
        source.extend_from_slice(b"text}");
        source
    }) {
        assert_eq!(convert(&source).unwrap_err().code(), ErrorCode::Malformed);
        source.clear();
    }
    for source in [
        b"{\\rtf1\\ansi \\'00}".as_slice(),
        b"{\\rtf1\\ansi \\'81}".as_slice(),
        b"{\\rtf1\\ansi \\'7f}".as_slice(),
        b"{\\rtf1\\ansi \\u0?}".as_slice(),
        b"{\\rtf1\\ansi \\u127?}".as_slice(),
        b"{\\rtf1\\ansi \\u128?}".as_slice(),
        b"{\\rtf1\\ansi{\\info{\\title safe\\'00bad}}}".as_slice(),
        b"{\\rtf1\\ansi{\\field{\\*\\fldinst HYPERLINK \\u0?}{\\fldrslt x}}}".as_slice(),
        b"{\\rtf1\\ansi{\\field{\\*\\fldinst HYPERLINK \\\"https://example.invalid\\\"}{\\fldrslt x\\'00}}}".as_slice(),
    ] {
        assert_eq!(convert(source).unwrap_err().code(), ErrorCode::Malformed);
    }

    let allowed = convert(b"{\\rtf1\\ansi a\tb\\tab c\\line d}").unwrap();
    let Block::Paragraph(inlines) = &allowed.document.blocks[0].block else { panic!("paragraph") };
    assert!(inlines.iter().any(|inline| matches!(inline, Inline::LineBreak)));
    assert_eq!(paragraph_text(&allowed), "a\tb\tcd");
}

#[test]
fn single_byte_codepages_validate_decoded_unicode_scalars() {
    let printable = convert(b"{\\rtf1\\ansi\\ansicpg1252 \\'80 \\'91 \\'92}").unwrap();
    assert_eq!(paragraph_text(&printable), "€ ‘ ’");

    let mut raw = b"{\\rtf1\\ansi\\ansicpg1252 ".to_vec();
    raw.extend_from_slice(&[0x80, b'}']);
    assert_eq!(paragraph_text(&convert(&raw).unwrap()), "€");

    for source in [
        b"{\\rtf1\\ansi\\ansicpg1252 \\'81}".as_slice(),
        b"{\\rtf1\\ansi\\ansicpg1252 \\'8d}".as_slice(),
    ] {
        assert_eq!(convert(source).unwrap_err().code(), ErrorCode::Malformed);
    }
}

#[test]
fn adjacent_lists_aggregate_only_for_exact_identity_and_sequence() {
    let output = convert(
        b"{\\rtf1\\ansi\\ls7\\ilvl0{\\listtext 3.\\tab}Third\\par{\\listtext 4.\\tab}Fourth\\par}",
    )
    .unwrap();
    assert_eq!(output.document.blocks.len(), 1);
    let Block::List { kind, start, items } = &output.document.blocks[0].block else {
        panic!("list")
    };
    assert_eq!((*kind, *start, items.len()), (ListKind::Ordered, 3, 2));
    assert_eq!(items[0].marker_label.as_deref(), Some("3."));
    assert_eq!(items[1].marker_label.as_deref(), Some("4."));

    let split = convert(b"{\\rtf1\\ansi\\ls7{\\listtext 1.\\tab}One\\par\\ls8{\\listtext 1.\\tab}Other\\par\\pard ordinary\\par\\ls8{\\listtext 2.\\tab}Two\\par}").unwrap();
    assert_eq!(split.document.blocks.len(), 4);
    assert!(matches!(split.document.blocks[0].block, Block::List { .. }));
    assert!(matches!(split.document.blocks[1].block, Block::List { .. }));
    assert!(matches!(split.document.blocks[2].block, Block::Paragraph(_)));
    assert!(matches!(split.document.blocks[3].block, Block::List { .. }));

    let kind_split = convert(
        b"{\\rtf1\\ansi\\ls7{\\listtext 1.\\tab}One\\par{\\listtext\\bullet\\tab}Bullet\\par}",
    )
    .unwrap();
    assert_eq!(kind_split.document.blocks.len(), 2);

    for ambiguous in [
        b"{\\rtf1\\ansi\\ls7 No marker\\par}".as_slice(),
        b"{\\rtf1\\ansi\\ls7\\ilvl1{\\listtext 1.\\tab}Nested\\par}".as_slice(),
        b"{\\rtf1\\ansi\\ls7{\\listtext 1.\\tab}One\\par{\\listtext 3.\\tab}Three\\par}".as_slice(),
        b"{\\rtf1\\ansi\\ls7{\\listtext alpha\\tab}Ambiguous\\par}".as_slice(),
    ] {
        assert_eq!(convert(ambiguous).unwrap_err().code(), ErrorCode::Malformed);
    }
}

#[test]
fn table_shape_and_nested_structural_nodes_fail_at_parser_limits() {
    let mut options = ConversionOptions::default();
    options.limits.max_table_columns = 1;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let error = convert_rtf_bytes(
        b"{\\rtf1\\ansi\\trowd\\cellx100\\cellx200 A\\cell B\\cell\\row}",
        &options,
        &context,
    )
    .unwrap_err();
    assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_table_columns", .. }));

    assert_eq!(
        convert(b"{\\rtf1\\ansi\\trowd\\itap2 A\\cell\\row}").unwrap_err().code(),
        ErrorCode::ResourceLimit
    );
    assert_eq!(
        convert(b"{\\rtf1\\ansi\\trowd\\intbl\\ls1{\\listtext 1.\\tab}A\\cell\\row}")
            .unwrap_err()
            .code(),
        ErrorCode::Malformed
    );
    assert_eq!(
        convert(b"{\\rtf1\\ansi\\trowd A\\cell B\\cell\\row\\trowd C\\cell\\row}")
            .unwrap_err()
            .code(),
        ErrorCode::Malformed
    );

    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut node = super::parser::Parser::new(b"{\\rtf1}", &options, &context).unwrap();
    node.document_nodes = MAX_DOCUMENT_NODES;
    assert!(matches!(
        node.node(Block::Rule, 0, 1).unwrap_err(),
        ConversionError::ResourceLimit { limit: "document_nodes", .. }
    ));
    assert_eq!(node.node_sequence, 0);

    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut list = super::parser::Parser::new(b"{\\rtf1}", &options, &context).unwrap();
    list.document_nodes = MAX_DOCUMENT_NODES - 1;
    list.state_mut().list_id = Some(1);
    list.pending_list_marker = Some("1.".into());
    list.paragraph.inlines.push(Inline::Text { value: "x".into(), marks: Vec::new() });
    assert!(matches!(
        list.finish_paragraph(7).unwrap_err(),
        ConversionError::ResourceLimit { limit: "document_nodes", .. }
    ));
    assert_eq!(list.node_sequence, 0);

    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut table = super::parser::Parser::new(b"{\\rtf1}", &options, &context).unwrap();
    table.document_nodes = MAX_DOCUMENT_NODES - 1;
    table.table.active = true;
    table.state_mut().in_table = true;
    table.paragraph.inlines.push(Inline::Text { value: "x".into(), marks: Vec::new() });
    assert!(matches!(
        table.finish_cell(7).unwrap_err(),
        ConversionError::ResourceLimit { limit: "document_nodes", .. }
    ));
    assert_eq!(table.node_sequence, 0);
}

#[test]
fn control_word_and_hex_run_work_are_incrementally_bounded() {
    let long = format!("{{\\rtf1\\{} 1}}", "a".repeat(33));
    assert!(matches!(
        convert(long.as_bytes()).unwrap_err(),
        ConversionError::ResourceLimit { limit: "rtf_control_word_length", .. }
    ));

    let source = b"{\\rtf1\\'41\\'42}";
    let options = ConversionOptions::default();
    let cancellation = CancellationToken::new();
    let context = ExecutionContext::new(
        ExecutionOptions { cancellation: cancellation.clone(), ..ExecutionOptions::default() },
        options.limits.clone(),
    );
    let mut parser = super::parser::Parser::new(source, &options, &context).unwrap();
    parser.offset = source.windows(2).position(|value| value == b"\\'").unwrap();
    parser.control_count = super::parser::MAX_CONTROLS - 1;
    assert!(matches!(
        parser.control().unwrap_err(),
        ConversionError::ResourceLimit { limit: "rtf_control_count", .. }
    ));

    let mut parser = super::parser::Parser::new(source, &options, &context).unwrap();
    parser.offset = source.windows(2).position(|value| value == b"\\'").unwrap();
    parser.control_count = 1022;
    cancellation.cancel();
    assert_eq!(parser.control().unwrap_err().code(), ErrorCode::Cancelled);
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
    assert!(
        !link.diagnostics.iter().any(|diagnostic| diagnostic.code == "rtf.unknownControlIgnored")
    );
    let unknown = convert(b"{\\rtf1\\ansi\\definitelyunknown text}").unwrap();
    assert!(
        unknown.diagnostics.iter().any(|diagnostic| diagnostic.code == "rtf.unknownControlIgnored")
    );
}

#[test]
fn table_cell_definitions_reject_ambiguous_boundaries_and_merge_chains() {
    for source in [
        b"{\\rtf1\\ansi\\trowd\\cellx A\\cell\\row}".as_slice(),
        b"{\\rtf1\\ansi\\trowd\\cellx0 A\\cell\\row}".as_slice(),
        b"{\\rtf1\\ansi\\trowd\\cellx200\\cellx100 A\\cell B\\cell\\row}".as_slice(),
        b"{\\rtf1\\ansi\\trowd\\clmrg\\cellx100 A\\cell\\row}".as_slice(),
        b"{\\rtf1\\ansi\\trowd\\cellx100\\clmrg\\cellx200 A\\cell B\\cell\\row}".as_slice(),
        b"{\\rtf1\\ansi\\clmgf text}".as_slice(),
    ] {
        assert_eq!(convert(source).unwrap_err().code(), ErrorCode::Malformed);
    }

    let output = convert(
        b"{\\rtf1\\ansi\\trowd\\clmgf\\cellx100\\clmrg\\cellx200\\cellx300\\intbl merged\\cell\\cell ordinary\\cell\\row}",
    )
    .unwrap();
    let Block::Table { rows, .. } = &output.document.blocks[0].block else { panic!("table") };
    assert_eq!(rows[0].cells.len(), 2);
    assert_eq!(rows[0].cells[0].column_span, 2);
    assert_eq!(rows[0].cells[1].column_span, 1);
}

#[test]
fn png_picture_is_decoded_and_retained_but_vector_is_not() {
    let png = "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c6360606060000000050001a5f645400000000049454e44ae426082";
    let source = format!("{{\\rtf1\\ansi{{\\pict\\pngblip {png}}}}}");
    let output = convert(source.as_bytes()).unwrap();
    assert_eq!(output.assets.len(), 1);
    assert_eq!(output.assets[0].media_type, "image/png");
    assert!(matches!(output.document.blocks[0].block, Block::Image { .. }));

    let mut binary_png = Vec::new();
    for pair in png.as_bytes().chunks_exact(2) {
        binary_png.push(u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap());
    }
    let mut binary_source =
        format!("{{\\rtf1\\ansi{{\\pict\\pngblip\\bin{} ", binary_png.len()).into_bytes();
    binary_source.extend_from_slice(&binary_png);
    binary_source.extend_from_slice(b"}}");
    let binary = convert(&binary_source).unwrap();
    assert_eq!(binary.assets.len(), 1);
    assert_eq!(&binary.assets[0].bytes[..], binary_png.as_slice());

    let vector = convert(b"{\\rtf1\\ansi{\\pict\\emfblip 0102}}").unwrap();
    assert!(vector.assets.is_empty());
    assert!(
        vector.diagnostics.iter().any(|diagnostic| diagnostic.code == "rtf.unsupportedVectorImage")
    );
}

#[test]
fn shape_picture_wrapper_preserves_block_order_and_rejects_other_children() {
    let png = "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c6360606060000000050001a5f645400000000049454e44ae426082";
    let source = format!(
        "{{\\rtf1\\ansi before{{\\*\\shppict{{\\object{{\\title BAD}}}}{{\\pict\\pngblip {png}}}{{\\*\\unknown BAD}}}}after\\par}}"
    );
    let output = convert(source.as_bytes()).unwrap();
    assert_eq!(output.assets.len(), 1);
    assert!(output.document.metadata.title.is_none());
    assert_eq!(output.document.blocks.len(), 3);
    assert!(matches!(output.document.blocks[0].block, Block::Paragraph(_)));
    assert!(matches!(output.document.blocks[1].block, Block::Image { .. }));
    assert!(matches!(output.document.blocks[2].block, Block::Paragraph(_)));
    assert_eq!(paragraph_text(&output), "beforeafter");

    let no_picture =
        convert(b"{\\rtf1\\ansi{\\*\\shppict{\\object{\\pict\\pngblip 00}}}safe}").unwrap();
    assert!(no_picture.assets.is_empty());
    assert_eq!(paragraph_text(&no_picture), "safe");

    let table_source = format!(
        "{{\\rtf1\\ansi\\trowd\\cellx100\\intbl before{{\\pict\\pngblip {png}}}after\\cell\\row}}"
    );
    let table = convert(table_source.as_bytes()).unwrap();
    let Block::Table { rows, .. } = &table.document.blocks[0].block else { panic!("table") };
    let blocks = &rows[0].cells[0].blocks;
    assert_eq!(blocks.len(), 3);
    assert!(matches!(blocks[0].block, Block::Paragraph(_)));
    assert!(matches!(blocks[1].block, Block::Image { .. }));
    assert!(matches!(blocks[2].block, Block::Paragraph(_)));

    assert_eq!(
        convert(b"{\\rtf1\\ansi\\ls1{\\listtext 1.\\tab}before{\\pict\\pngblip 00}}")
            .unwrap_err()
            .code(),
        ErrorCode::Malformed
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

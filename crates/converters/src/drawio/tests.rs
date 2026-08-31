use super::*;
use base64::Engine as _;
use into_markdown_core::{
    ErrorPolicy, ExecutionOptions, FormatDetector, FormatHint, InputRef, SourceMetadata,
};
use std::{io::Write, sync::Arc, time::Duration};

const MODEL: &str = r#"<mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0" value="Layer"/><mxCell id="g" parent="1" vertex="1" value="Group" style="group"/><UserObject id="a" label="&lt;b&gt;中文&lt;/b&gt;&lt;br&gt;开始"><mxCell vertex="1" parent="g" style="html=1"/></UserObject><mxCell id="b" parent="g" vertex="1"/><mxCell id="e" edge="1" parent="1" source="a" target="b" value="go"/><mxCell id="el" vertex="1" parent="e" value="附加标签"/></root></mxGraphModel>"#;

fn context(options: &ConversionOptions) -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), options.limits.clone())
}
fn run(source: &str, options: &ConversionOptions) -> Result<ConverterOutput, ConversionError> {
    convert(source.as_bytes(), options, &context(options))
}
fn markdown(output: &ConverterOutput) -> String {
    into_markdown_render_markdown::render(
        &output.document,
        &output.assets,
        &ConversionOptions::default(),
    )
    .unwrap()
    .replace('\\', "")
}

fn payload(model: &str) -> String {
    let encoded: String = model.as_bytes().iter().map(|b| format!("%{b:02X}")).collect();
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(encoded.as_bytes()).unwrap();
    base64::engine::general_purpose::STANDARD.encode(encoder.finish().unwrap())
}
fn wrapped(model: &str) -> String {
    format!("<mxfile><diagram>{model}</diagram></mxfile>")
}

#[test]
fn encodings_have_identical_semantics_and_original_source_locations() {
    let options = ConversionOptions::default();
    let plain = run(MODEL, &options).unwrap();
    let inline = run(&wrapped(MODEL), &options).unwrap();
    let compressed = run(&wrapped(&payload(MODEL)), &options).unwrap();
    assert_eq!(markdown(&plain), markdown(&inline));
    assert_eq!(markdown(&plain), markdown(&compressed));
    let md = markdown(&plain);
    for text in [
        "中文",
        "开始",
        "Group",
        "Layer",
        "(unlabeled)",
        "go",
        "附加标签",
        "a [p1:c4]",
        "b [p1:c5]",
    ] {
        assert!(md.contains(text), "missing {text}: {md}");
    }
    assert!(md.contains("Connections"));
    assert!(!md.contains("xml-attribute"));
    let json = serde_json::to_string(&plain.document).unwrap();
    assert!(json.contains("drawio/pages/1/cells/4"));
    let encoded = serde_json::to_string(&compressed.document).unwrap();
    assert!(encoded.contains("\"byteStart\":17"));
    plain.document.validate().unwrap();
}

#[test]
fn all_pages_and_more_than_forty_cells_are_preserved() {
    let cells: String = (0..60)
        .map(|i| format!(r#"<mxCell id="n{i}" parent="1" vertex="1" value="node-{i}"/>"#))
        .collect();
    let model = format!(
        r#"<mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/>{cells}</root></mxGraphModel>"#
    );
    let input = format!(
        r#"<mxfile><diagram id="same" name="一">{model}</diagram><diagram id="same" name="二">{}</diagram></mxfile>"#,
        payload(&model)
    );
    let md = markdown(&run(&input, &ConversionOptions::default()).unwrap());
    assert_eq!(md.matches("node-59").count(), 2);
    assert!(md.contains("Page 2: 二"));
    assert!(md.contains("p2:c62"));
}

#[test]
fn relationship_cycles_self_edges_parallel_edges_and_free_points_survive_strict() {
    let input = MODEL.replace("</root>", r#"<mxCell id="self" edge="1" source="a" target="a"/><mxCell id="back" edge="1" source="b" target="a"/><mxCell id="parallel" edge="1" source="a" target="b"/><mxCell id="free" edge="1"><mxGeometry><mxPoint x="12.5" y="-3" as="sourcePoint"/></mxGeometry></mxCell></root>"#);
    let mut options = ConversionOptions::default();
    options.error_policy = ErrorPolicy::Strict;
    let result = run(&input, &options).unwrap();
    let md = markdown(&result);
    for text in ["self [", "back [", "parallel [", "free endpoint (12.5, -3)"] {
        assert!(md.contains(text), "{md}");
    }
}

#[test]
fn defects_are_located_and_strict_rejects_them() {
    for (input, code) in [
        (MODEL.replace("id=\"b\"", "id=\"a\""), "drawio.duplicateId"),
        (MODEL.replace("target=\"b\"", "target=\"missing\""), "drawio.danglingEndpoint"),
        (MODEL.replace("id=\"g\" parent=\"1\"", "id=\"g\" parent=\"a\""), "drawio.parentCycle"),
        (
            MODEL.replace("id=\"b\" parent=\"g\"", "id=\"b\" parent=\"missing\""),
            "drawio.missingParent",
        ),
        (MODEL.replace("id=\"b\"", ""), "drawio.missingId"),
    ] {
        let mut options = ConversionOptions::default();
        let result = run(&input, &options).unwrap();
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == code && d.locator.as_ref().unwrap().page == Some(1)),
            "{code}"
        );
        options.error_policy = ErrorPolicy::Strict;
        assert!(matches!(run(&input, &options), Err(ConversionError::Malformed { .. })), "{code}");
    }
}

#[test]
fn broken_page_recovers_only_when_outer_boundaries_are_sound() {
    let input = format!("<mxfile><diagram>bad!</diagram><diagram>{MODEL}</diagram></mxfile>");
    let options = ConversionOptions::default();
    let result = run(&input, &options).unwrap();
    assert!(markdown(&result).contains("Page 2"));
    assert_eq!(result.diagnostics[0].code, "drawio.pageOmitted");
    assert!(run(&input.replace("</mxfile>", ""), &options).is_err());
    assert!(run(&wrapped("bad!"), &options).is_err());
    let strict = ConversionOptions { error_policy: ErrorPolicy::Strict, ..options };
    assert!(
        matches!(run(&input, &strict), Err(ConversionError::Malformed { part: Some(part), .. }) if part == "drawio/pages/1")
    );
}

#[test]
fn safety_and_resource_errors_never_become_best_effort_success() {
    let mut options = ConversionOptions::default();
    for text in [
        "<!DOCTYPE mxGraphModel><mxGraphModel><root/></mxGraphModel>",
        "<mxGraphModel><root><mxCell id=\"x\" value=\"&#0;\"/></root></mxGraphModel>",
    ] {
        assert!(run(text, &options).is_err());
    }
    let hostile = "<!DOCTYPE mxGraphModel><mxGraphModel><root/></mxGraphModel>";
    assert!(matches!(
        run(
            &format!(
                "<mxfile><diagram>{}</diagram><diagram>{MODEL}</diagram></mxfile>",
                payload(hostile)
            ),
            &options
        ),
        Err(ConversionError::Unsupported { .. })
    ));
    for (kind, max) in [
        ("pages", 0),
        ("depth", 1),
        ("memory", 100),
        ("field", 8),
        ("decompressed", 128),
        ("table_rows", 1),
        ("table_columns", 1),
        ("table_cells", 1),
    ] {
        options = ConversionOptions::default();
        match kind {
            "pages" => options.limits.max_pages = max as u32,
            "depth" => options.limits.max_nesting_depth = max as u16,
            "memory" => options.limits.max_memory_bytes = max,
            "field" => options.limits.max_field_bytes = max,
            "decompressed" => options.limits.max_decompressed_bytes = max,
            "table_rows" => options.limits.max_table_rows = max,
            "table_columns" => options.limits.max_table_columns = max,
            _ => options.limits.max_table_cells = max,
        }
        let ctx = context(&options);
        assert!(
            matches!(
                convert(wrapped(&payload(MODEL)).as_bytes(), &options, &ctx),
                Err(ConversionError::ResourceLimit { .. })
            ),
            "{kind}"
        );
        assert_eq!(ctx.reserved_memory_bytes(), 0, "{kind}");
    }
}

#[test]
fn timeout_and_cancellation_release_reservations() {
    let options = ConversionOptions::default();
    let execution = ExecutionOptions::default();
    execution.cancellation.cancel();
    let ctx = ExecutionContext::new(execution, options.limits.clone());
    assert!(matches!(convert(MODEL.as_bytes(), &options, &ctx), Err(ConversionError::Cancelled)));
    let ctx = ExecutionContext::new(
        ExecutionOptions { timeout: Some(Duration::ZERO), ..ExecutionOptions::default() },
        options.limits.clone(),
    );
    assert!(matches!(
        convert(MODEL.as_bytes(), &options, &ctx),
        Err(ConversionError::Timeout { .. })
    ));
    assert_eq!(ctx.reserved_memory_bytes(), 0);
}

#[test]
fn detection_distinguishes_xml_and_drawio_roots() {
    let options = ConversionOptions::default();
    let ctx = context(&options);
    for (source, wanted) in
        [(MODEL, InputFormat::Drawio), ("<ordinary><mxGraphModel/></ordinary>", InputFormat::Xml)]
    {
        let input = ResolvedInput {
            bytes: Arc::from(source.as_bytes()),
            metadata: SourceMetadata::default(),
        };
        let candidates = futures::executor::block_on(crate::ContentFormatDetector.detect(
            &input,
            &FormatHint::default(),
            &ctx,
        ))
        .unwrap();
        assert_eq!(candidates[0].format, wanted);
    }
    assert_eq!(InputFormat::from_extension("DRAWIO"), Some(InputFormat::Drawio));
    assert!(matches!(
        InputRef::bytes(MODEL.as_bytes(), Some("map.drawio")),
        InputRef::Bytes { .. }
    ));
}

#[test]
fn deep_groups_expand_with_full_paths_within_ir_depth() {
    let mut cells = String::from(r#"<mxCell id="0"/><mxCell id="1" parent="0"/>"#);
    for i in 2..40 {
        cells.push_str(&format!(
            r#"<mxCell id="{i}" parent="{}" vertex="1" value="group-{i}"/>"#,
            i - 1
        ));
    }
    let result = run(
        &format!("<mxGraphModel><root>{cells}</root></mxGraphModel>"),
        &ConversionOptions::default(),
    )
    .unwrap();
    assert!(markdown(&result).contains("group-39"));
    assert!(markdown(&result).contains("ancestors:"));
    result.document.validate().unwrap();
}

#[test]
fn child_label_descendants_and_parent_candidates_keep_source_identities() {
    let input = MODEL.replace("</root>", r#"<mxCell id="nested" vertex="1" parent="el" value="Nested label"/><mxCell id="orphan"/><mxCell id="g" vertex="1" value="Second group"/></root>"#);
    let result = run(&input, &ConversionOptions::default()).unwrap();
    let md = markdown(&result);
    for text in [
        "Nested label",
        "nested [p1:c8]",
        "parent: el",
        "orphan [p1:c9]",
        "candidates: g [p1:c3]; g [p1:c10]",
    ] {
        assert!(md.contains(text), "{text}: {md}");
    }
    result.document.validate().unwrap();
}

#[test]
fn html_labels_placeholders_entities_and_safe_references_remain_offline() {
    let input = r#"<mxGraphModel><root><object id="a" label="&lt;b&gt;&lt;strong&gt;%name%&lt;/strong&gt;&lt;/b&gt;&lt;br&gt;&amp;amp; &lt;a href='https://example.invalid/doc'&gt;Doc&lt;/a&gt;&lt;img src='https://example.invalid/image.png' alt='图'&gt;&lt;script&gt;EXECUTE&lt;/script&gt;" placeholders="1" name="中文" link="javascript:alert(1)"><mxCell vertex="1" style="html=1"/></object><mxCell id="b" vertex="1" value="A&#10;B &amp; C"/></root></mxGraphModel>"#;
    let mut options = ConversionOptions::default();
    options.error_policy = ErrorPolicy::Strict;
    let result = run(input, &options).unwrap();
    result.document.validate().unwrap();
    let md = markdown(&result);
    for text in
        ["中文", "https://example.invalid/doc", "https://example.invalid/image.png", "B &amp; C"]
    {
        assert!(md.contains(text) || md.contains(&text.replace("&amp;", "&")), "{text}: {md}");
    }
    assert!(!md.contains("EXECUTE"));
    assert!(!md.contains("javascript:"));
    assert!(result.assets.is_empty());
    assert!(result.diagnostics.iter().any(|d| d.code == "drawio.unsafeReference"));
}

#[test]
fn malformed_page_models_recover_in_source_order() {
    for bad in [
        "<other/>",
        "<mxGraphModel/><mxGraphModel/>",
        "<mxGraphModel><root><object><mxCell id='a'/><mxCell id='b'/></object></root></mxGraphModel>",
    ] {
        let input = format!("<mxfile><diagram>{bad}</diagram><diagram>{MODEL}</diagram></mxfile>");
        let result = run(&input, &ConversionOptions::default()).unwrap();
        assert!(markdown(&result).starts_with("# Page 2"));
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "drawio.pageOmitted"
                    && d.locator.as_ref().unwrap().page == Some(1))
        );
    }
}

#[test]
fn original_byte_spans_identify_cells_and_encoded_payloads() {
    let input = wrapped(MODEL);
    let result = run(&input, &ConversionOptions::default()).unwrap();
    let dto = serde_json::to_value(&result.document).unwrap();
    fn locators(value: &serde_json::Value, found: &mut Vec<serde_json::Value>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(loc) = map.get("locator") {
                    found.push(loc.clone());
                }
                for child in map.values() {
                    locators(child, found);
                }
            }
            serde_json::Value::Array(items) => {
                for child in items {
                    locators(child, found);
                }
            }
            _ => (),
        }
    }
    let mut found = Vec::new();
    locators(&dto, &mut found);
    let loc = found.iter().find(|v| v["part"] == "drawio/pages/1/cells/4").unwrap();
    let raw = &input
        [loc["byteStart"].as_u64().unwrap() as usize..loc["byteEnd"].as_u64().unwrap() as usize];
    assert!(raw.starts_with("<UserObject id=\"a\""));
    assert!(raw.contains("parent=\"g\""));
    let encoded = payload(MODEL);
    let input = wrapped(&encoded);
    let dto = serde_json::to_value(&run(&input, &ConversionOptions::default()).unwrap().document)
        .unwrap();
    found.clear();
    locators(&dto, &mut found);
    let loc = found.iter().find(|v| v["part"] == "drawio/pages/1/cells/4").unwrap();
    assert_eq!(
        &input[loc["byteStart"].as_u64().unwrap() as usize
            ..loc["byteEnd"].as_u64().unwrap() as usize],
        encoded
    );
}

#[test]
fn hostile_width_cell_counts_and_bombs_are_fatal_and_release_memory() {
    let attrs: String = (0..4097).map(|i| format!(" k{i}='v'")).collect();
    let wide = format!("<mxGraphModel{attrs}><root/></mxGraphModel>");
    let cells: String = (0..100_001).map(|i| format!("<mxCell id='{i}'/>")).collect();
    let many = format!("<mxGraphModel><root>{cells}</root></mxGraphModel>");
    let bomb = wrapped(&payload(&format!(
        "<mxGraphModel><root>{}</root></mxGraphModel>",
        " ".repeat(2_000_000)
    )));
    for (source, limit) in [(wide, 64_000_000), (many, 64_000_000), (bomb, 10_000)] {
        let mut options = ConversionOptions::default();
        options.limits.max_decompressed_bytes = limit;
        options.limits.max_memory_bytes = 256 * 1024 * 1024;
        let ctx = context(&options);
        assert!(matches!(
            convert(source.as_bytes(), &options, &ctx),
            Err(ConversionError::ResourceLimit { .. })
        ));
        assert_eq!(ctx.reserved_memory_bytes(), 0);
    }
}

#[test]
fn cancellation_during_xml_work_releases_all_page_allocations() {
    let options = ConversionOptions::default();
    let execution = ExecutionOptions::default();
    let cancel = execution.cancellation.clone();
    let ctx = ExecutionContext::new(execution, options.limits.clone());
    let observer = ctx.clone();
    let cells: String = (0..80_000).map(|i| format!("<mxCell id='{i}'/>")).collect();
    let source = format!("<mxGraphModel><root>{cells}</root></mxGraphModel>");
    let worker = std::thread::spawn(move || convert(source.as_bytes(), &options, &ctx));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while observer.reserved_memory_bytes() == 0 && !worker.is_finished() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    cancel.cancel();
    assert!(matches!(worker.join().unwrap(), Err(ConversionError::Cancelled)));
    assert_eq!(observer.reserved_memory_bytes(), 0);
}

#[test]
fn exact_memory_boundary_and_active_deadline_are_enforced() {
    let source = wrapped(&payload(MODEL));
    let mut options = ConversionOptions::default();
    let ctx = context(&options);
    drop(convert(source.as_bytes(), &options, &ctx).unwrap());
    let peak = ctx.resource_usage().shared_lease_peak_bytes;
    assert_eq!(ctx.reserved_memory_bytes(), 0);
    options.limits.max_memory_bytes = peak;
    assert!(run(&source, &options).is_ok());
    options.limits.max_memory_bytes = peak - 1;
    assert!(matches!(run(&source, &options), Err(ConversionError::ResourceLimit { .. })));
    options = ConversionOptions::default();
    let large = format!("<mxGraphModel><root>{}</root></mxGraphModel>", " ".repeat(4_000_000));
    let ctx = ExecutionContext::new(
        ExecutionOptions { timeout: Some(Duration::from_millis(5)), ..ExecutionOptions::default() },
        options.limits.clone(),
    );
    assert!(matches!(
        convert(large.as_bytes(), &options, &ctx),
        Err(ConversionError::Timeout { .. })
    ));
    assert_eq!(ctx.reserved_memory_bytes(), 0);
}

#[test]
fn encoded_payload_truncation_trailing_data_and_invalid_uri_are_rejected() {
    let mut options = ConversionOptions::default();
    options.error_policy = ErrorPolicy::Strict;
    let valid = payload(MODEL);
    let mut decoded = base64::engine::general_purpose::STANDARD.decode(&valid).unwrap();
    decoded.push(0);
    let trailing = base64::engine::general_purpose::STANDARD.encode(&decoded);
    for text in [valid[..valid.len() - 4].to_owned(), trailing, "AA==AA==".into()] {
        assert!(matches!(run(&wrapped(&text), &options), Err(ConversionError::Malformed { .. })));
    }
    let spaced: String = valid
        .chars()
        .enumerate()
        .flat_map(|(i, c)| if i % 7 == 0 { vec!['\n', c] } else { vec![c] })
        .collect();
    assert_eq!(
        markdown(&run(&wrapped(&spaced), &options).unwrap()),
        markdown(&run(MODEL, &options).unwrap())
    );
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(b"%GG").unwrap();
    let invalid = base64::engine::general_purpose::STANDARD.encode(encoder.finish().unwrap());
    assert!(matches!(run(&wrapped(&invalid), &options), Err(ConversionError::Malformed { .. })));
}

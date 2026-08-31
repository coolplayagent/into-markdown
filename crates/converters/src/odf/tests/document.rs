#![allow(clippy::format_collect, clippy::needless_raw_string_hashes)]

use super::support::{NS, convert, package};
use into_markdown_core::{
    Block, CellRef, ConversionError, ConversionOptions, Inline, InputFormat, ListKind,
    ResourceLimits,
};
use into_markdown_render_markdown::render;

#[test]
fn odt_preserves_styles_lists_tables_links_annotations_and_metadata() {
    let content = format!(
        r#"<office:document-content {NS} office:version='1.3'><office:automatic-styles><style:style style:name='Strong' style:family='text'><style:text-properties fo:font-weight='bold'/></style:style><text:list-style style:name='Bullets'><text:list-level-style-bullet text:level='1' text:bullet-char='•'/></text:list-style></office:automatic-styles><office:body><office:text><text:h text:outline-level='2'>Heading</text:h><text:p>Hello <text:span text:style-name='Strong'>bold</text:span> <text:a xlink:href='https://example.com/path'>link</text:a><office:annotation><text:p>review</text:p></office:annotation></text:p><text:list text:style-name='Bullets'><text:list-item><text:p>one</text:p></text:list-item></text:list><table:table><table:table-row><table:table-cell><text:p>A</text:p></table:table-cell><table:table-cell><text:p>B</text:p></table:table-cell></table:table-row></table:table></office:text></office:body></office:document-content>"#
    );
    let bytes = package(InputFormat::Odt, &content, &[]);
    let output = convert(&bytes, InputFormat::Odt, ResourceLimits::default()).unwrap();
    output.document.validate().unwrap();
    let markdown = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(markdown.contains("## Heading"));
    assert!(markdown.contains("**bold**"), "{markdown}");
    assert!(markdown.contains("[link](<https://example.com/path>)"));
    assert!(markdown.contains("Comment: review"));
    assert!(markdown.contains("- one"));
    assert!(markdown.contains("| A | B |"));
}

#[test]
fn ods_expands_bounded_repeats_and_retains_cell_coordinates() {
    let content = format!(
        r#"<office:document-content {NS} office:version='1.3'><office:body><office:spreadsheet><table:table table:name='Data'><table:table-row><table:table-cell table:number-columns-repeated='2' office:value-type='string' office:string-value='x'/><table:table-cell office:value-type='string' office:string-value='z'/></table:table-row><table:table-row table:number-rows-repeated='2'><table:table-cell><text:p>tail</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#
    );
    let bytes = package(InputFormat::Ods, &content, &[]);
    let output = convert(&bytes, InputFormat::Ods, ResourceLimits::default()).unwrap();
    output.document.validate().unwrap();
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].cells.len(), 3);
    assert_eq!(
        rows[0].cells[2].blocks[0].provenance.locator.cell,
        Some(CellRef { row: 0, column: 2 })
    );
}

#[test]
fn list_definitions_nested_levels_and_continuations_are_semantic() {
    let content = format!(
        r#"<office:document-content {NS}><office:automatic-styles><text:list-style style:name='N'><text:list-level-style-number text:level='1' text:start-value='3' style:num-format='1'/><text:list-level-style-bullet text:level='2' text:bullet-char='•'/></text:list-style></office:automatic-styles><office:body><office:text><text:list text:style-name='N' xml:id='l1'><text:list-item><text:p>three</text:p><text:list text:style-name='N'><text:list-item><text:p>nested</text:p></text:list-item></text:list></text:list-item></text:list><text:list text:style-name='N' text:continue-list='l1'><text:list-item><text:p>four</text:p></text:list-item></text:list></office:text></office:body></office:document-content>"#
    );
    let bytes = package(InputFormat::Odt, &content, &[]);
    let output = convert(&bytes, InputFormat::Odt, ResourceLimits::default()).unwrap();
    let Block::List { kind, start, items } = &output.document.blocks[0].block else { panic!() };
    assert_eq!((*kind, *start), (ListKind::Ordered, 3));
    assert!(
        items[0]
            .blocks
            .iter()
            .any(|block| { matches!(&block.block, Block::List { kind: ListKind::Bullet, .. }) })
    );
    let Block::List { start, .. } = &output.document.blocks[1].block else { panic!() };
    assert_eq!(*start, 4);

    let unknown =
        content.replace("text:style-name='N' xml:id='l1'", "text:style-name='Missing' xml:id='l1'");
    let unknown = package(InputFormat::Odt, &unknown, &[]);
    assert!(matches!(
        convert(&unknown, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn implicit_nested_list_and_markerless_header_preserve_structure() {
    let content = format!(
        r#"<office:document-content {NS}><office:automatic-styles><text:list-style style:name='N'><text:list-level-style-number text:level='1' style:num-format='1'/><text:list-level-style-bullet text:level='2' text:bullet-char='•'/></text:list-style></office:automatic-styles><office:body><office:text><text:list text:style-name='N'><text:list-header><text:p>Unmarked preface</text:p></text:list-header><text:list-item><text:p>outer</text:p><text:list><text:list-item><text:p>implicit nested</text:p></text:list-item></text:list></text:list-item></text:list></office:text></office:body></office:document-content>"#
    );
    let bytes = package(InputFormat::Odt, &content, &[]);
    let output = convert(&bytes, InputFormat::Odt, ResourceLimits::default()).unwrap();
    assert!(matches!(
        &output.document.blocks[0].block,
        Block::Paragraph(inlines)
            if inlines.iter().any(|inline| matches!(inline, Inline::Text { value, .. } if value == "Unmarked preface"))
    ));
    let Block::List { items, .. } = &output.document.blocks[1].block else { panic!() };
    assert_eq!(items.len(), 1);
    assert!(
        items[0]
            .blocks
            .iter()
            .any(|block| { matches!(&block.block, Block::List { kind: ListKind::Bullet, .. }) })
    );

    let top_level_implicit = content.replace("text:style-name='N'", "");
    let top_level_implicit = package(InputFormat::Odt, &top_level_implicit, &[]);
    assert!(matches!(
        convert(&top_level_implicit, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn ranged_annotations_pair_strictly_and_preserve_safe_metadata() {
    let content = format!(
        r#"<office:document-content {NS}><office:body><office:text><text:p>before <office:annotation office:name='a'><dc:creator>Ada</dc:creator><dc:date>2026-08-13</dc:date><text:p>review this</text:p></office:annotation> ranged text <office:annotation-end office:name='a'/> after</text:p></office:text></office:body></office:document-content>"#
    );
    let bytes = package(InputFormat::Odt, &content, &[]);
    let output = convert(&bytes, InputFormat::Odt, ResourceLimits::default()).unwrap();
    let markdown = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(markdown.contains("Comment by Ada"), "{markdown}");
    assert!(markdown.contains("review this"), "{markdown}");
    assert!(markdown.contains("ranged text"));

    let dangling = package(
        InputFormat::Odt,
        &content.replace("<office:annotation-end office:name='a'/>", ""),
        &[],
    );
    assert!(matches!(
        convert(&dangling, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));
    let duplicate = package(
            InputFormat::Odt,
            &content.replace(
                " after",
                "<office:annotation office:name='a'><text:p>again</text:p></office:annotation><office:annotation-end office:name='a'/> after",
            ),
            &[],
        );
    assert!(matches!(
        convert(&duplicate, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));
    let crossing_xml = format!(
        "<office:document-content {NS}><office:body><office:text><text:p><office:annotation office:name='a'/><office:annotation office:name='b'/><office:annotation-end office:name='a'/><office:annotation-end office:name='b'/></text:p></office:text></office:body></office:document-content>"
    );
    let crossing = package(InputFormat::Odt, &crossing_xml, &[]);
    assert!(matches!(
        convert(&crossing, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn versions_and_style_family_origin_are_bound() {
    let common_styles = format!(
        r#"<office:document-styles {NS} office:version='1.3'><office:styles><style:style style:name='Shared' style:family='text'><style:text-properties fo:font-style='italic'/></style:style><style:style style:name='Shared' style:family='paragraph'><style:text-properties fo:font-style='italic'/></style:style></office:styles></office:document-styles>"#
    );
    let content = format!(
        r#"<office:document-content {NS} office:version='1.3'><office:automatic-styles><style:style style:name='Shared' style:family='text'><style:text-properties fo:font-weight='bold'/></style:style></office:automatic-styles><office:body><office:text><text:p text:style-name='Shared'>paragraph <text:span text:style-name='Shared'>span</text:span></text:p></office:text></office:body></office:document-content>"#
    );
    let bytes = package(
        InputFormat::Odt,
        &content,
        &[("styles.xml", "text/xml", common_styles.as_bytes())],
    );
    let output = convert(&bytes, InputFormat::Odt, ResourceLimits::default()).unwrap();
    let markdown = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(markdown.contains("*paragraph*"));
    assert!(
        markdown.contains("***span***"),
        "{markdown}"
    );

    let bad_styles = common_styles.replace("office:version='1.3'", "office:version='9.9'");
    let mismatched =
        package(InputFormat::Odt, &content, &[("styles.xml", "text/xml", bad_styles.as_bytes())]);
    assert!(matches!(
        convert(&mismatched, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "styles.xml"
    ));
    let bad_content = package(
        InputFormat::Odt,
        &content.replace("office:version='1.3'", "office:version='9.9'"),
        &[],
    );
    assert!(matches!(
        convert(&bad_content, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "content.xml"
    ));
}

#[test]
fn cell_types_formulas_headers_and_sparse_coordinates_are_strict() {
    let content = format!(
        r#"<office:document-content {NS}><office:body><office:spreadsheet><table:table table:name='S'><table:table-header-rows><table:table-row><table:table-cell office:value-type='string' office:string-value='H'/></table:table-row></table:table-header-rows><table:table-row><table:table-cell office:value-type='float' office:value='2' table:formula='of:=[.A1]+1'/><table:table-cell table:number-columns-repeated='3'/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#
    );
    let bytes = package(InputFormat::Ods, &content, &[]);
    let output = convert(&bytes, InputFormat::Ods, ResourceLimits::default()).unwrap();
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    assert!(rows[0].cells[0].header);
    assert!(rows[1].cells[0].blocks.iter().any(|block| {
            matches!(&block.block, Block::Code { language: Some(language), text } if language == "openformula" && text == "[.A1]+1")
        }));
    assert_eq!(rows[1].cells.len(), 1);

    let mismatch = content.replace("office:value-type='float'", "office:value-type='date'");
    let mismatch = package(InputFormat::Ods, &mismatch, &[]);
    assert!(matches!(
        convert(&mismatch, InputFormat::Ods, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn odp_emits_one_title_and_notes_inside_slide() {
    let content = format!(
        r#"<office:document-content {NS} office:version='1.3'><office:body><office:presentation><draw:page draw:name='p1'><draw:frame presentation:class='title'><draw:text-box><text:p>Deck title</text:p></draw:text-box></draw:frame><draw:frame presentation:class='subtitle'><draw:text-box><text:p>Subtitle retained</text:p></draw:text-box></draw:frame><draw:g draw:transform='translate(1cm 2cm)'><draw:g draw:transform='scale(2 3)'><draw:frame svg:x='1cm' svg:y='2cm' svg:width='3cm' svg:height='4cm'><draw:text-box><text:p>Body</text:p></draw:text-box></draw:frame></draw:g></draw:g><presentation:notes><text:p>Remember this</text:p></presentation:notes></draw:page></office:presentation></office:body></office:document-content>"#
    );
    let bytes = package(InputFormat::Odp, &content, &[]);
    let output = convert(&bytes, InputFormat::Odp, ResourceLimits::default()).unwrap();
    output.document.validate().unwrap();
    let markdown = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert_eq!(markdown.matches("Deck title").count(), 1);
    assert!(markdown.contains("Subtitle retained"));
    assert!(markdown.contains("Body"));
    assert!(markdown.contains("Speaker notes"));
    assert!(markdown.contains("Remember this"));
    let Block::Slide { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let body = blocks.iter().find(|block| {
            matches!(&block.block, Block::Paragraph(inlines) if inlines.iter().any(|inline| matches!(inline, Inline::Text { value, .. } if value == "Body")))
        }).unwrap();
    let bounds = body.provenance.locator.bounds.as_ref().unwrap();
    assert!((bounds.x - 85.03937).abs() < 0.01);
    assert!((bounds.y - 226.77165).abs() < 0.01);

    let overflow = format!(
        r#"<office:document-content {NS}><office:body><office:presentation><draw:page><draw:g draw:transform='scale(3.4e38)'><draw:g draw:transform='scale(3.4e38)'><draw:frame svg:x='1cm' svg:y='1cm' svg:width='1cm' svg:height='1cm'><draw:text-box><text:p>x</text:p></draw:text-box></draw:frame></draw:g></draw:g></draw:page></office:presentation></office:body></office:document-content>"#
    );
    let overflow = package(InputFormat::Odp, &overflow, &[]);
    assert!(matches!(
        convert(&overflow, InputFormat::Odp, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "content.xml"
    ));
}

#[test]
fn odp_notes_omit_empty_groups_and_keep_images_alt_and_tables() {
    use into_markdown_core::AssetMode;
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(2, 2).write_to(&mut png, image::ImageFormat::Png).unwrap();
    let picture = "<draw:frame><draw:image xlink:href='Pictures/a.png'/></draw:frame>";
    for (body, extracted, omitted) in [
        ("".to_owned(), false, false), ("<text:p> \t </text:p>".into(), false, false),
        ("<text:p>Useful note</text:p>".into(), true, true),
        (picture.to_owned(), true, false),
        (picture.replace("<draw:image", "<svg:desc>Diagram note</svg:desc><draw:image"), true, true),
        ("<table:table><table:table-row><table:table-cell><text:p>Note cell</text:p></table:table-cell></table:table-row></table:table>".into(), true, true),
    ] {
        let content = format!("<office:document-content {NS}><office:body><office:presentation><draw:page draw:name='Slide'><presentation:notes>{body}</presentation:notes></draw:page></office:presentation></office:body></office:document-content>");
        let bytes = package(InputFormat::Odp, &content, &[("Pictures/a.png", "image/png", png.get_ref())]);
        let result = convert(&bytes, InputFormat::Odp, ResourceLimits::default()).unwrap();
        for mode in [AssetMode::Extract, AssetMode::Embed, AssetMode::Omit] {
            let mut options = ConversionOptions::default(); options.output.asset_mode = mode;
            let markdown = render(&result.document, &result.assets, &options).unwrap();
            assert_eq!(markdown.contains("### Speaker notes"), if mode == AssetMode::Omit { omitted } else { extracted }, "{body}: {markdown}");
        }
    }
}

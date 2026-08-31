use super::support::{append_parts, convert, rewrite_part, valid_png};
use into_markdown_core::{AssetMode, ConversionOptions};
use into_markdown_render_markdown::render;
use std::io::{Cursor, Read};

fn notes_fixture(body: &str) -> Vec<u8> {
    let original = include_bytes!("../../../../../fixtures/small/pptx/normal.pptx");
    let mut archive = zip::ZipArchive::new(Cursor::new(original.as_slice())).unwrap();
    let mut types = String::new();
    archive.by_name("[Content_Types].xml").unwrap().read_to_string(&mut types).unwrap();
    let types =
        types.replace("</Types>", "<Default Extension=\"png\" ContentType=\"image/png\"/></Types>");
    let notes = format!(
        r#"<p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:cSld><p:spTree>{body}</p:spTree></p:cSld></p:notes>"#
    );
    let bytes = rewrite_part(original, "[Content_Types].xml", types.as_bytes());
    let bytes = rewrite_part(&bytes, "ppt/notesSlides/notesSlide1.xml", notes.as_bytes());
    append_parts(&bytes, &[
        ("ppt/media/note.png", valid_png()),
        ("ppt/notesSlides/_rels/notesSlide1.xml.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="image" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/note.png"/></Relationships>"#.to_vec()),
    ])
}

#[test]
fn notes_empty_placeholders_text_images_and_omit_use_effective_content() {
    let picture = r#"<p:pic><p:nvPicPr><p:cNvPr id="4" name="Picture"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="image"/></p:blipFill><p:spPr/></p:pic>"#;
    let text = |value: &str| {
        format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Notes"/><p:cNvSpPr/><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{value}</a:t></a:r></a:p></p:txBody></p:sp>"#
        )
    };
    let cases = [
        (String::new(), false, false),
        (text(""), false, false),
        (text(" \t\n "), false, false),
        (text("Actual note"), true, true),
        (picture.to_owned(), true, false),
        (
            picture.replace("name=\"Picture\"", "name=\"Picture\" descr=\"Diagram note\""),
            true,
            true,
        ),
        (format!("{}{picture}", text("Actual note")), true, true),
    ];
    for (body, extracted, omitted) in cases {
        let result = convert(&notes_fixture(&body)).unwrap();
        result.document.validate().unwrap();
        for mode in [AssetMode::Extract, AssetMode::Embed, AssetMode::Omit] {
            let mut options = ConversionOptions::default();
            options.output.asset_mode = mode;
            let markdown = render(&result.document, &result.assets, &options).unwrap();
            assert_eq!(
                markdown.contains("### Speaker notes"),
                if mode == AssetMode::Omit { omitted } else { extracted },
                "{body}: {markdown}"
            );
        }
    }
}

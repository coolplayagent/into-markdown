#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ConversionOptions, ExecutionOptions, ResourceLimits};
    use into_markdown_render_markdown::render;
    use std::fmt::Write as _;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    const WORD: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const OFFICE_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PACKAGE_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const MATH: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";
    const MC: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";
    const DRAWING: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
    const WORD_DRAWING: &str =
        "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
    const DOC_CONTENT_TYPE: &str =
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn strict_options() -> ConversionOptions {
        ConversionOptions {
            error_policy: into_markdown_core::ErrorPolicy::Strict,
            ..ConversionOptions::default()
        }
    }

    fn limited_context(max_memory_bytes: u64) -> ExecutionContext {
        let limits = ResourceLimits { max_memory_bytes, ..ResourceLimits::default() };
        ExecutionContext::new(ExecutionOptions::default(), limits)
    }

    fn package(parts: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut output);
            for (name, bytes) in parts {
                zip.start_file(
                    name,
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
                )
                .unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        output.into_inner()
    }

    fn append_png_chunk(output: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
        output.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        output.extend_from_slice(&kind);
        output.extend_from_slice(data);
        let mut checked = Vec::with_capacity(kind.len() + data.len());
        checked.extend_from_slice(&kind);
        checked.extend_from_slice(data);
        output.extend_from_slice(&png_crc32(&checked).to_be_bytes());
    }

    fn valid_png(padding: usize) -> Vec<u8> {
        let mut output = b"\x89PNG\r\n\x1a\n".to_vec();
        append_png_chunk(&mut output, *b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
        if padding != 0 {
            let mut text = b"Comment\0".to_vec();
            text.resize(text.len() + padding, b'x');
            append_png_chunk(&mut output, *b"tEXt", &text);
        }
        append_png_chunk(&mut output, *b"IDAT", &[0x78, 0x9c, 0x63, 0x60, 0, 0, 0, 2, 0, 1]);
        append_png_chunk(&mut output, *b"IEND", &[]);
        output
    }

    fn append_jpeg_segment(output: &mut Vec<u8>, marker: u8, data: &[u8]) {
        output.extend_from_slice(&[0xff, marker]);
        output.extend_from_slice(&u16::try_from(data.len() + 2).unwrap().to_be_bytes());
        output.extend_from_slice(data);
    }

    fn valid_jpeg() -> Vec<u8> {
        let mut output = vec![0xff, 0xd8];
        let mut quantization = vec![0];
        quantization.extend_from_slice(&[1; 64]);
        append_jpeg_segment(&mut output, 0xdb, &quantization);
        append_jpeg_segment(&mut output, 0xc0, &[8, 0, 1, 0, 1, 1, 1, 0x11, 0]);
        let mut huffman = vec![0, 1];
        huffman.extend_from_slice(&[0; 15]);
        huffman.push(0);
        huffman.extend_from_slice(&[0x10, 1]);
        huffman.extend_from_slice(&[0; 15]);
        huffman.push(0);
        append_jpeg_segment(&mut output, 0xc4, &huffman);
        append_jpeg_segment(&mut output, 0xda, &[1, 1, 0, 0, 63, 0]);
        output.extend_from_slice(&[0x3f, 0xff, 0xd9]);
        output
    }

    fn base(document: &[u8], extra: &[(&str, &[u8])]) -> Vec<u8> {
        base_with_type(document, extra, DOC_CONTENT_TYPE)
    }

    fn base_with_type(document: &[u8], extra: &[(&str, &[u8])], content_type: &str) -> Vec<u8> {
        let mut overrides =
            format!(r#"<Override PartName="/word/document.xml" ContentType="{content_type}"/>"#);
        for (name, _) in extra {
            let content_type = match *name {
                "word/styles.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml",
                ),
                "word/numbering.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml",
                ),
                "word/comments.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml",
                ),
                "word/footnotes.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml",
                ),
                "word/endnotes.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.endnotes+xml",
                ),
                "word/glossary/document.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.glossary+xml",
                ),
                "word/header1.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml",
                ),
                "word/footer1.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml",
                ),
                "docProps/core.xml" => {
                    Some("application/vnd.openxmlformats-package.core-properties+xml")
                }
                _ => None,
            };
            if let Some(content_type) = content_type {
                write!(
                    &mut overrides,
                    r#"<Override PartName="/{name}" ContentType="{content_type}"/>"#
                )
                .unwrap();
            }
        }
        let types = format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="png" ContentType="image/png"/>{overrides}</Types>"#
        );
        let core_relationship = if extra.iter().any(|(name, _)| *name == "docProps/core.xml") {
            format!(
                r#"<Relationship Id="rCore" Type="{REL_TYPE_PREFIX}metadata/core-properties" Target="docProps/core.xml"/>"#
            )
        } else {
            String::new()
        };
        let root_relationships = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rDocument" Type="{OFFICE_REL_TYPE}" Target="word/document.xml"/>{core_relationship}</Relationships>"#
        );
        let mut parts = vec![
            ("[Content_Types].xml".to_owned(), types.into_bytes()),
            ("_rels/.rels".to_owned(), root_relationships.into_bytes()),
            ("word/document.xml".to_owned(), document.to_vec()),
        ];
        parts.extend(extra.iter().map(|(name, bytes)| ((*name).to_owned(), bytes.to_vec())));
        package(&parts)
    }

    fn image_package(part: &str, declared_content_type: &str, bytes: &[u8]) -> Vec<u8> {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}" xmlns:a="{DRAWING}"><w:body><w:p><w:r><w:drawing><a:blip r:embed="rImage"/></w:drawing></w:r></w:p></w:body></w:document>"#
        );
        let types = format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/word/document.xml" ContentType="{DOC_CONTENT_TYPE}"/><Override PartName="/{part}" ContentType="{declared_content_type}"/></Types>"#
        );
        let root = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rDocument" Type="{OFFICE_REL_TYPE}" Target="word/document.xml"/></Relationships>"#
        );
        let target = part.strip_prefix("word/").expect("test image belongs to word part");
        let relationships = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rImage" Type="{REL_TYPE_PREFIX}image" Target="{target}"/></Relationships>"#
        );
        package(&[
            ("[Content_Types].xml".into(), types.into_bytes()),
            ("_rels/.rels".into(), root.into_bytes()),
            ("word/document.xml".into(), document.into_bytes()),
            ("word/_rels/document.xml.rels".into(), relationships.into_bytes()),
            (part.into(), bytes.to_vec()),
        ])
    }

    #[test]
    fn authenticated_glossary_only_source_emits_a_visible_placeholder() {
        let document =
            format!(r#"<w:document xmlns:w="{WORD}"><w:body><w:p/></w:body></w:document>"#);
        let relationships = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rGlossary" Type="{REL_TYPE_PREFIX}glossaryDocument" Target="glossary/document.xml"/></Relationships>"#
        );
        let glossary = format!(
            r#"<w:glossaryDocument xmlns:w="{WORD}"><w:docParts><w:docPart><w:docPartBody><w:sdt><w:sdtContent><w:p><w:r><w:t>Building block</w:t></w:r></w:p></w:sdtContent></w:sdt></w:docPartBody></w:docPart></w:docParts></w:glossaryDocument>"#
        );
        let bytes = base(
            document.as_bytes(),
            &[
                ("word/_rels/document.xml.rels", relationships.as_bytes()),
                ("word/glossary/document.xml", glossary.as_bytes()),
            ],
        );

        let output = convert_docx(&bytes, &ConversionOptions::default(), &context()).unwrap();

        assert_eq!(output.source_content_evidence(), SourceContentEvidence::Unknown);
        let markdown = render(&output.document, &output.assets, &ConversionOptions::default())
            .unwrap();
        assert!(markdown.contains("Word glossary content omitted"), "{markdown}");
        assert!(
            output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "word.glossaryContentOmitted")
        );
    }

    #[test]
    fn best_effort_recovers_loose_fields_unsafe_links_duplicate_marks_and_ragged_tables() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body>
                <w:p><w:r><w:rPr><w:b/><w:i/><w:b/></w:rPr><w:t>styled</w:t></w:r><w:instrText>HYPERLINK "file:///private/report"</w:instrText></w:p>
                <w:tbl>
                  <w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr>
                  <w:tr><w:tc><w:p><w:r><w:t>C</w:t></w:r></w:p></w:tc></w:tr>
                </w:tbl>
            </w:body></w:document>"#
        );
        let bytes = base(document.as_bytes(), &[]);
        let options = ConversionOptions::default();
        let output = convert_docx(&bytes, &options, &context()).unwrap();
        output.document.validate().unwrap();
        assert!(
            output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "word.unsupportedWrapperOmitted")
        );
        assert!(
            output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "office.relationshipOmitted")
        );
        assert!(
            output.diagnostics.iter().any(|diagnostic| diagnostic.code == "word.tableNormalized")
        );
        let markdown = render(&output.document, &output.assets, &options).unwrap();
        assert!(markdown.contains("file:///private/report"));
        assert!(!markdown.contains("](file:///private/report)"));
        let table = output.document.blocks.iter().find_map(|node| match &node.block {
            Block::Table { rows, .. } => Some(rows),
            _ => None,
        });
        assert!(
            matches!(table, Some(rows) if rows.len() == 2 && rows.iter().all(|row| row.cells.len() == 2))
        );
        assert!(matches!(
            convert_docx(&bytes, &strict_options(), &context()),
            Err(ConversionError::Malformed { .. })
        ));
    }

    #[test]
    fn converts_styles_lists_links_images_footnotes_headers_comments_fields_and_formula() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}" xmlns:m="{MATH}" xmlns:a="{DRAWING}" xmlns:wp="{WORD_DRAWING}"><w:body><w:p><w:pPr><w:pStyle w:val="CustomHeading"/></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t>Title</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:hyperlink r:id="rLink"><w:r><w:t>site</w:t></w:r></w:hyperlink><w:r><w:footnoteReference w:id="2"/></w:r><w:r><w:commentReference w:id="0"/></w:r><w:r><w:fldChar w:fldCharType="begin"/><w:instrText> PAGE </w:instrText><w:fldChar w:fldCharType="end"/></w:r><m:oMath><m:r><m:t>x+y</m:t></m:r></m:oMath><w:r><w:drawing><wp:docPr id="1" name="picture" descr="alt"/><a:blip r:embed="rImg"/></w:drawing></w:r></w:p><w:tbl><w:tr><w:trPr><w:tblHeader/></w:trPr><w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl><w:sectPr><w:headerReference r:id="rHeader"/></w:sectPr></w:body></w:document>"#
        );
        let rels = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rLink" Type="{REL_TYPE_PREFIX}hyperlink" Target="https://example.com" TargetMode="External"/><Relationship Id="rImg" Type="{REL_TYPE_PREFIX}image" Target="media/a.png"/><Relationship Id="rStyles" Type="{REL_TYPE_PREFIX}styles" Target="styles.xml"/><Relationship Id="rNumbering" Type="{REL_TYPE_PREFIX}numbering" Target="numbering.xml"/><Relationship Id="rFootnotes" Type="{REL_TYPE_PREFIX}footnotes" Target="footnotes.xml"/><Relationship Id="rComments" Type="{REL_TYPE_PREFIX}comments" Target="comments.xml"/><Relationship Id="rHeader" Type="{REL_TYPE_PREFIX}header" Target="header1.xml"/></Relationships>"#
        );
        let styles = format!(
            r#"<w:styles xmlns:w="{WORD}"><w:style w:styleId="Heading1"><w:name w:val="heading 1"/></w:style><w:style w:styleId="CustomHeading"><w:basedOn w:val="Heading1"/></w:style></w:styles>"#
        );
        let numbering = format!(
            r#"<w:numbering xmlns:w="{WORD}"><w:abstractNum w:abstractNumId="7"><w:lvl w:ilvl="0"><w:start w:val="3"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="7"/><w:lvlOverride w:ilvl="0"><w:startOverride w:val="5"/></w:lvlOverride></w:num></w:numbering>"#
        );
        let footnotes = format!(
            r#"<w:footnotes xmlns:w="{WORD}"><w:footnote w:id="2"><w:p><w:r><w:t>note</w:t></w:r></w:p></w:footnote></w:footnotes>"#
        );
        let comments = format!(
            r#"<w:comments xmlns:w="{WORD}"><w:comment w:id="0"><w:p><w:r><w:t>review</w:t></w:r></w:p></w:comment></w:comments>"#
        );
        let header =
            format!(r#"<w:hdr xmlns:w="{WORD}"><w:p><w:r><w:t>head</w:t></w:r></w:p></w:hdr>"#);
        let core = r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Fixture title</dc:title><dc:creator>Fixture author</dc:creator></cp:coreProperties>"#;
        let image = valid_png(0);
        let bytes = base(
            document.as_bytes(),
            &[
                ("word/_rels/document.xml.rels", rels.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/footnotes.xml", footnotes.as_bytes()),
                ("word/comments.xml", comments.as_bytes()),
                ("word/header1.xml", header.as_bytes()),
                ("word/media/a.png", image.as_slice()),
                ("docProps/core.xml", core.as_bytes()),
            ],
        );
        let output = convert_docx(&bytes, &ConversionOptions::default(), &context()).unwrap();
        let markdown =
            render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
        assert!(markdown.contains("# <strong>Title</strong>"), "{markdown}");
        assert!(markdown.contains("[site](<https://example.com>)"));
        assert!(markdown.contains("5."));
        assert!(markdown.contains("[^fn-32]"));
        assert!(markdown.contains("$`x+y`$"));
        assert!(markdown.contains("cell"));
        assert!(markdown.contains("Header") && markdown.contains("head"));
        assert!(markdown.contains("Comment 0") && markdown.contains("review"));
        assert!(markdown.contains("note"));
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.document.metadata.title.as_deref(), Some("Fixture title"));
    }

    #[test]
    fn predefined_and_numeric_references_reassemble_across_all_text_and_attribute_consumers() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}" xmlns:m="{MATH}"><w:body><w:p><w:r><w:t>A&amp;<![CDATA[<B&]]>&#38;&#x4E2D;&apos;&quot;&gt;</w:t></w:r><w:hyperlink r:id="r1"><w:r><w:t>go&amp;&#x4E2D;</w:t></w:r></w:hyperlink><w:r><w:fldChar w:fldCharType="begin"/><w:instrText>HYPERLINK &quot;https://field.example/?x=1&amp;y=2&quot;</w:instrText><w:fldChar w:fldCharType="end"/></w:r><m:oMath><m:r><m:t>x&amp;<![CDATA[<y]]>&#x4E2D;</m:t></m:r></m:oMath><w:r><w:commentReference w:id="0"/></w:r><w:r><w:footnoteReference w:id="2"/></w:r></w:p></w:body></w:document>"#
        );
        let relationships = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="r&#49;" Type="{REL_TYPE_PREFIX}hyperlink" Target="https://example.com/?a=1&amp;b=&#50;" TargetMode="External"/><Relationship Id="rComments" Type="{REL_TYPE_PREFIX}comments" Target="comments.xml"/><Relationship Id="rFootnotes" Type="{REL_TYPE_PREFIX}footnotes" Target="footnotes.xml"/></Relationships>"#
        );
        let comments = format!(
            r#"<w:comments xmlns:w="{WORD}"><w:comment w:id="0"><w:p><w:r><w:t>comment&amp;<![CDATA[<piece>]]>&#x4E2D;</w:t></w:r></w:p></w:comment></w:comments>"#
        );
        let footnotes = format!(
            r#"<w:footnotes xmlns:w="{WORD}"><w:footnote w:id="2"><w:p><w:r><w:t>foot&amp;<![CDATA[<piece>]]>&#20013;</w:t></w:r></w:p></w:footnote></w:footnotes>"#
        );
        let core = r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Core&amp;<![CDATA[<Title>]]>&#x4E2D;</dc:title><dc:creator>A&amp;B</dc:creator></cp:coreProperties>"#;
        let bytes = base(
            document.as_bytes(),
            &[
                ("word/_rels/document.xml.rels", relationships.as_bytes()),
                ("word/comments.xml", comments.as_bytes()),
                ("word/footnotes.xml", footnotes.as_bytes()),
                ("docProps/core.xml", core.as_bytes()),
            ],
        );
        let output = convert_docx(&bytes, &ConversionOptions::default(), &context()).unwrap();
        assert_eq!(output.document.metadata.title.as_deref(), Some("Core&<Title>中"));
        assert_eq!(output.document.metadata.authors, ["A&B"]);

        let main = output.document.blocks.iter().find_map(|node| match &node.block {
            Block::Paragraph(inlines) => Some(inlines),
            _ => None,
        });
        let main = main.expect("main paragraph");
        assert!(main.iter().any(|inline| matches!(
            inline,
            Inline::Text { value, .. } if value == "A&<B&&中'\">"
        )));
        assert!(main.iter().any(|inline| matches!(
            inline,
            Inline::Link { target, content }
                if target == "https://example.com/?a=1&b=2"
                    && matches!(content.as_slice(), [Inline::Text { value, .. }] if value == "go&中")
        )));
        assert!(main.iter().any(|inline| matches!(
            inline,
            Inline::Link { target, .. } if target == "https://field.example/?x=1&y=2"
        )));
        assert!(
            main.iter().any(|inline| matches!(inline, Inline::Formula(value) if value == "x&<y中"))
        );

        assert!(output.document.blocks.iter().any(|node| matches!(
            &node.block,
            Block::Paragraph(inlines)
                if inlines.iter().any(|inline| matches!(inline, Inline::Text { value, .. } if value == "comment&<piece>中"))
        )));
        assert!(output.document.blocks.iter().any(|node| matches!(
            &node.block,
            Block::Footnote { blocks, .. }
                if blocks.iter().any(|block| matches!(
                    &block.block,
                    Block::Paragraph(inlines)
                        if inlines.iter().any(|inline| matches!(inline, Inline::Text { value, .. } if value == "foot&<piece>中"))
                ))
        )));
    }

    #[test]
    fn custom_dtd_and_illegal_character_references_remain_fail_closed() {
        for reference in ["&custom;", "&#0;", "&#x1;", "&#xD800;", "&#x110000;", "&#X41;"] {
            let document = format!(
                r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>{reference}</w:t></w:r></w:p></w:body></w:document>"#
            );
            assert!(matches!(
                convert_docx(
                    &base(document.as_bytes(), &[]),
                    &ConversionOptions::default(),
                    &context(),
                ),
                Err(ConversionError::Malformed { .. })
            ));
        }

        let document = format!(r#"<w:document xmlns:w="{WORD}"><w:body/></w:document>"#);
        let custom_attribute = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="bad" Type="{REL_TYPE_PREFIX}hyperlink" Target="https://example.com/?q=&custom;" TargetMode="External"/></Relationships>"#
        );
        assert!(matches!(
            convert_docx(
                &base(
                    document.as_bytes(),
                    &[("word/_rels/document.xml.rels", custom_attribute.as_bytes())],
                ),
                &ConversionOptions::default(),
                &context(),
            ),
            Err(ConversionError::Malformed { .. })
        ));

        let dtd = format!(
            r#"<!DOCTYPE w:document [<!ENTITY custom "expanded">]><w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>&custom;</w:t></w:r></w:p></w:body></w:document>"#
        );
        assert!(matches!(
            convert_docx(&base(dtd.as_bytes(), &[]), &ConversionOptions::default(), &context(),),
            Err(ConversionError::Malformed { .. })
        ));
    }

    #[test]
    fn roots_namespaces_hierarchy_and_text_context_fail_closed() {
        for invalid in [
            format!(r#"<w:hdr xmlns:w="{WORD}"/>"#),
            r#"<w:document xmlns:w="w"><w:body/></w:document>"#.to_owned(),
            format!(
                r#"<w:document xmlns:w="{WORD}"><w:p><w:r><w:t>outside</w:t></w:r></w:p><w:body/></w:document>"#
            ),
        ] {
            assert!(matches!(
                convert_docx(&base(invalid.as_bytes(), &[]), &strict_options(), &context()),
                Err(ConversionError::Malformed { .. })
            ));
        }

        let spoofed = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:e="urn:evil"><w:body><w:p><w:r><e:t>spoofed</e:t><w:t>kept</w:t></w:r></w:p></w:body></w:document>"#
        );
        let output =
            convert_docx(&base(spoofed.as_bytes(), &[]), &ConversionOptions::default(), &context())
                .unwrap();
        let markdown =
            render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
        assert!(markdown.contains("kept"), "{markdown}");
        assert!(!markdown.contains("spoofed"), "{markdown}");
        assert!(matches!(
            convert_docx(&base(spoofed.as_bytes(), &[]), &strict_options(), &context()),
            Err(ConversionError::Malformed { .. })
        ));

        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:mc="{MC}" xmlns:x="urn:fixture-extension"><w:body>
              <w:p><x:payload><w:r><w:t>descendant-must-not-leak</w:t></w:r></x:payload><w:extension><w:r><w:t>word-extension-must-not-leak</w:t></w:r></w:extension><w:r><w:t>kept</w:t></w:r></w:p>
              <mc:AlternateContent><mc:Choice Requires="x"><w:p><w:r><w:t>choice-must-not-leak</w:t></w:r></w:p></mc:Choice><mc:Fallback><w:p><w:r><w:t>fallback-kept</w:t></w:r></w:p></mc:Fallback></mc:AlternateContent>
            </w:body></w:document>"#
        );
        let output = convert_docx(
            &base(document.as_bytes(), &[]),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        let markdown =
            render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
        assert!(markdown.contains("kept") && markdown.contains("fallback\\-kept"), "{markdown}");
        assert!(
            !markdown.contains("must-not-leak")
                && !markdown.contains("choice-must-not-leak")
        );

        let structured_document_tag = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:tbl><w:tblPr/><w:sdt><w:sdtPr><w:id w:val="1001"/></w:sdtPr><w:sdtContent><w:tr><w:tc><w:p><w:r><w:t>SDT Cell</w:t></w:r></w:p></w:tc></w:tr></w:sdtContent></w:sdt></w:tbl></w:body></w:document>"#
        );
        let output = convert_docx(
            &base(structured_document_tag.as_bytes(), &[]),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        let markdown =
            render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
        assert!(markdown.contains("SDT Cell"), "{markdown}");
    }

    #[test]
    fn relationships_require_the_package_qname_before_becoming_authoritative() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}"><w:body><w:p><w:hyperlink r:id="rLink"><w:r><w:t>safe label</w:t></w:r></w:hyperlink></w:p></w:body></w:document>"#
        );
        let relationships = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}" xmlns:e="urn:evil"><e:Relationship Id="rLink" Type="{REL_TYPE_PREFIX}hyperlink" Target="https://evil.example" TargetMode="External"/><e:wrapper><Relationship Id="rLink" Type="{REL_TYPE_PREFIX}hyperlink" Target="https://evil-parent.example" TargetMode="External"/></e:wrapper><Relationship Id="rLink" Type="{REL_TYPE_PREFIX}hyperlink" Target="https://safe.example" TargetMode="External"/></Relationships>"#
        );
        let output = convert_docx(
            &base(
                document.as_bytes(),
                &[("word/_rels/document.xml.rels", relationships.as_bytes())],
            ),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        let markdown = render(&output.document, &output.assets, &ConversionOptions::default())
            .unwrap();
        assert!(markdown.contains("https://safe.example"), "{markdown}");
        assert!(!markdown.contains("evil.example"), "{markdown}");
        assert!(!markdown.contains("evil-parent.example"), "{markdown}");
    }

    #[test]
    fn annotation_parts_share_mc_tables_and_alt_chunk_degradation_with_body_parsing() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}"><w:body><w:p><w:r><w:t>body</w:t><w:commentReference w:id="7"/></w:r></w:p></w:body></w:document>"#
        );
        let document_relationships = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="comments" Type="{REL_TYPE_PREFIX}comments" Target="comments.xml"/></Relationships>"#
        );
        let comments = format!(
            r#"<w:comments xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}" xmlns:mc="{MC}" xmlns:x="urn:unsupported" xmlns:a="{DRAWING}"><w:comment w:id="7"><mc:AlternateContent><mc:Choice Requires="x"><w:p><w:r><w:t>choice-hidden</w:t></w:r></w:p></mc:Choice><mc:Fallback><w:p><w:r><w:t>fallback-visible</w:t></w:r></w:p></mc:Fallback></mc:AlternateContent><w:tbl><w:tr><w:tc><w:p><w:r><w:t>comment-table</w:t></w:r></w:p></w:tc></w:tr></w:tbl><w:p><w:r><w:drawing><a:blip r:embed="image"/></w:drawing></w:r></w:p><w:altChunk r:id="chunk"/></w:comment></w:comments>"#
        );
        let comment_relationships = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="image" Type="{REL_TYPE_PREFIX}image" Target="media/comment.png"/><Relationship Id="chunk" Type="{REL_TYPE_PREFIX}aFChunk" Target="https://example.invalid/comment.html" TargetMode="External"/></Relationships>"#
        );
        let image = valid_png(0);
        let output = convert_docx(
            &base(
                document.as_bytes(),
                &[
                    (
                        "word/_rels/document.xml.rels",
                        document_relationships.as_bytes(),
                    ),
                    ("word/comments.xml", comments.as_bytes()),
                    (
                        "word/_rels/comments.xml.rels",
                        comment_relationships.as_bytes(),
                    ),
                    ("word/media/comment.png", image.as_slice()),
                ],
            ),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        let markdown = render(&output.document, &output.assets, &ConversionOptions::default())
            .unwrap();
        assert!(markdown.contains(r"fallback\-visible"), "{markdown}");
        assert!(markdown.contains(r"comment\-table"), "{markdown}");
        assert!(markdown.contains("Embedded Word content omitted"), "{markdown}");
        assert!(!markdown.contains("choice-hidden"), "{markdown}");
        assert_eq!(output.assets.len(), 1);
        assert!(output.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "office.relationshipOmitted"
                && diagnostic.severity == DiagnosticSeverity::Warning
        }));
    }

    #[test]
    fn word_template_main_content_type_is_a_valid_docx_source() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>template-visible</w:t></w:r></w:p></w:body></w:document>"#
        );
        let bytes = base_with_type(
            document.as_bytes(),
            &[],
            "application/vnd.openxmlformats-officedocument.wordprocessingml.template.main+xml",
        );
        let output = convert_docx(&bytes, &ConversionOptions::default(), &context()).unwrap();
        let markdown = render(&output.document, &output.assets, &ConversionOptions::default())
            .unwrap();
        assert!(markdown.contains(r"template\-visible"), "{markdown}");
    }

    #[test]
    fn complex_hyperlink_fields_use_the_result_label_and_allow_nesting() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:fldChar w:fldCharType="begin"/><w:instrText>HYPERLINK "https://example.com/report"</w:instrText><w:fldChar w:fldCharType="separate"/><w:t>Report </w:t><w:fldChar w:fldCharType="begin"/><w:instrText>PAGE</w:instrText><w:fldChar w:fldCharType="separate"/><w:t>2</w:t><w:fldChar w:fldCharType="end"/><w:t> label</w:t><w:fldChar w:fldCharType="end"/></w:r></w:p></w:body></w:document>"#
        );
        let output = convert_docx(
            &base(document.as_bytes(), &[]),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        let markdown = render(&output.document, &output.assets, &ConversionOptions::default())
            .unwrap();
        assert_eq!(markdown.matches("https://example.com/report").count(), 1, "{markdown}");
        assert_eq!(markdown.matches("Report 2 label").count(), 1, "{markdown}");
        assert!(
            markdown.contains("[Report 2 label](<https://example.com/report>)"),
            "{markdown}"
        );
    }

    #[test]
    fn warning_upgrades_an_earlier_duplicate_info_diagnostic() {
        let mut state = ParseState::default();
        state.info("word.loss", "informational", "word/document.xml");
        state.warning("word.loss", "content was omitted", "word/document.xml");
        assert_eq!(state.diagnostics.len(), 1);
        assert_eq!(state.diagnostics[0].severity, DiagnosticSeverity::Warning);
        assert_eq!(state.diagnostics[0].message, "content was omitted");
    }

    #[test]
    fn merged_table_cells_preserve_row_and_column_spans() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:tbl><w:tr><w:tc><w:tcPr><w:gridSpan w:val="2"/><w:vMerge w:val="restart"/></w:tcPr><w:p><w:r><w:t>merged</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:tcPr><w:gridSpan w:val="2"/><w:vMerge/></w:tcPr><w:p/></w:tc></w:tr></w:tbl></w:body></w:document>"#
        );
        let output = convert_docx(
            &base(document.as_bytes(), &[]),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        let cell = output.document.blocks.iter().find_map(|node| match &node.block {
            Block::Table { rows, .. } => rows.first().and_then(|row| row.cells.first()),
            _ => None,
        });
        assert!(matches!(cell, Some(cell) if cell.row_span == 2 && cell.column_span == 2));
    }

    #[test]
    fn core_properties_require_authoritative_namespace_and_direct_children() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>body</w:t></w:r></w:p></w:body></w:document>"#
        );
        let core_cases = [
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:e="urn:evil"><e:lastModifiedBy>spoof</e:lastModifiedBy></cp:coreProperties>"#.to_owned(),
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"><cp:keywords><cp:lastModifiedBy>nested</cp:lastModifiedBy></cp:keywords></cp:coreProperties>"#.to_owned(),
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title><dc:creator>nested</dc:creator></dc:title></cp:coreProperties>"#.to_owned(),
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"><cp:modified>wrong namespace</cp:modified></cp:coreProperties>"#.to_owned(),
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:e="urn:evil"><dc:title><e:payload>nested text</e:payload></dc:title></cp:coreProperties>"#.to_owned(),
        ];
        for core in core_cases {
            assert!(matches!(
                convert_docx(
                    &base(document.as_bytes(), &[("docProps/core.xml", core.as_bytes())],),
                    &strict_options(),
                    &context(),
                ),
                Err(ConversionError::Malformed { .. })
            ));
        }

        let valid = r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"><dc:title>title</dc:title><dc:creator>creator</dc:creator><cp:lastModifiedBy>editor</cp:lastModifiedBy><dcterms:modified>2026-08-13T00:00:00Z</dcterms:modified></cp:coreProperties>"#;
        let output = convert_docx(
            &base(document.as_bytes(), &[("docProps/core.xml", valid.as_bytes())]),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert_eq!(output.document.metadata.title.as_deref(), Some("title"));
        assert_eq!(output.document.metadata.authors, ["creator", "editor"]);
    }

    #[test]
    fn style_numbering_and_word_semantics_reject_relocation_and_spoofing() {
        let document = format!(r#"<w:document xmlns:w="{WORD}"><w:body/></w:document>"#);
        let styles_relation = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="styles" Type="{REL_TYPE_PREFIX}styles" Target="styles.xml"/></Relationships>"#
        );
        let invalid_styles = [
            format!(
                r#"<w:styles xmlns:w="{WORD}"><w:style w:styleId="x"><w:pPr><w:name w:val="heading 1"/></w:pPr></w:style></w:styles>"#
            ),
            format!(
                r#"<w:styles xmlns:w="{WORD}" xmlns:e="urn:evil"><w:style w:styleId="x"><e:basedOn w:val="Heading1"/></w:style></w:styles>"#
            ),
            format!(
                r#"<w:styles xmlns:w="{WORD}"><w:style w:styleId="x"><w:outlineLvl w:val="0"/></w:style></w:styles>"#
            ),
            format!(
                r#"<w:styles xmlns:w="{WORD}"><w:pPr><w:style w:styleId="x"/></w:pPr></w:styles>"#
            ),
        ];
        for styles in invalid_styles {
            assert!(matches!(
                convert_docx(
                    &base(
                        document.as_bytes(),
                        &[
                            ("word/_rels/document.xml.rels", styles_relation.as_bytes()),
                            ("word/styles.xml", styles.as_bytes()),
                        ],
                    ),
                    &strict_options(),
                    &context(),
                ),
                Err(ConversionError::Malformed { .. })
            ));
        }

        let numbering_relation = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="numbering" Type="{REL_TYPE_PREFIX}numbering" Target="numbering.xml"/></Relationships>"#
        );
        let invalid_numbering = [
            format!(
                r#"<w:numbering xmlns:w="{WORD}"><w:abstractNum w:abstractNumId="1"><w:numFmt w:val="decimal"/></w:abstractNum></w:numbering>"#
            ),
            format!(
                r#"<w:numbering xmlns:w="{WORD}"><w:num w:numId="1"><w:lvl w:ilvl="0"><w:start w:val="9"/></w:lvl></w:num></w:numbering>"#
            ),
            format!(
                r#"<w:numbering xmlns:w="{WORD}"><w:num w:numId="1"><w:startOverride w:val="9"/></w:num></w:numbering>"#
            ),
            format!(
                r#"<w:numbering xmlns:w="{WORD}" xmlns:e="urn:evil"><w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><e:lvlText w:val="%1"/></w:lvl></w:abstractNum></w:numbering>"#
            ),
        ];
        for numbering in invalid_numbering {
            assert!(matches!(
                convert_docx(
                    &base(
                        document.as_bytes(),
                        &[
                            ("word/_rels/document.xml.rels", numbering_relation.as_bytes()),
                            ("word/numbering.xml", numbering.as_bytes()),
                        ],
                    ),
                    &strict_options(),
                    &context(),
                ),
                Err(ConversionError::Malformed { .. })
            ));
        }

        let invalid_documents = [
            format!(
                r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:pStyle w:val="Heading1"/><w:r><w:t>x</w:t></w:r></w:p></w:body></w:document>"#
            ),
            format!(
                r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:b/><w:t>not bold</w:t></w:r></w:p></w:body></w:document>"#
            ),
            format!(
                r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:tab/><w:r><w:t>x</w:t></w:r></w:p></w:body></w:document>"#
            ),
            format!(
                r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:numId w:val="1"/><w:t>x</w:t></w:r></w:p></w:body></w:document>"#
            ),
            format!(
                r#"<w:document xmlns:w="{WORD}" xmlns:m="{MATH}"><w:body><w:p><m:t>spoof</m:t></w:p></w:body></w:document>"#
            ),
            format!(
                r#"<w:document xmlns:w="{WORD}"><w:body><w:comment w:id="7"><w:p><w:r><w:t>relocated annotation</w:t></w:r></w:p></w:comment></w:body></w:document>"#
            ),
        ];
        for invalid in invalid_documents {
            assert!(matches!(
                convert_docx(&base(invalid.as_bytes(), &[]), &strict_options(), &context(),),
                Err(ConversionError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn images_require_content_type_extension_and_valid_bounded_structure() {
        let png = valid_png(0);
        let output = convert_docx(
            &image_package("word/media/image.png", "image/png", &png),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].media_type, "image/png");
        assert_eq!(output.assets[0].bytes, png);
        let jpeg = valid_jpeg();
        let output = convert_docx(
            &image_package("word/media/image.jpg", "image/jpeg", &jpeg),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].media_type, "image/jpeg");
        assert_eq!(output.assets[0].bytes, jpeg);

        let mut corrupt_crc = valid_png(0);
        let index = corrupt_crc.len() - 5;
        corrupt_crc[index] ^= 1;
        let corrupt_idat = {
            let mut value = valid_png(0);
            value[41] ^= 1;
            let crc = png_crc32(&value[37..51]);
            value[51..55].copy_from_slice(&crc.to_be_bytes());
            value
        };
        let oversized_header = {
            let mut value = valid_png(0);
            value[16..20].copy_from_slice(&(MAX_IMAGE_DIMENSION + 1).to_be_bytes());
            let crc = png_crc32(&value[12..29]);
            value[29..33].copy_from_slice(&crc.to_be_bytes());
            value
        };
        let mismatch = valid_png(0);
        let mut truncated_jpeg = valid_jpeg();
        truncated_jpeg.pop();
        let mut corrupt_jpeg_codestream = valid_jpeg();
        let entropy = corrupt_jpeg_codestream.len() - 3;
        corrupt_jpeg_codestream[entropy] = 0x7f;
        assert_eq!(
            validate_jpeg(&corrupt_jpeg_codestream, "word/media/codestream.jpg").unwrap(),
            (1, 1),
            "the adversarial fixture must remain marker/table/frame/scan valid"
        );
        match convert_docx(
            &image_package("word/media/codestream.jpg", "image/jpeg", &corrupt_jpeg_codestream),
            &strict_options(),
            &context(),
        ) {
            Err(ConversionError::Malformed { detail, .. }) => {
                assert!(detail.contains("entropy stream"), "unexpected error: {detail}");
            }
            other => panic!("expected corrupt JPEG codestream rejection, got {other:?}"),
        }
        let adversarial = [
            ("word/media/fake.png", "image/png", b"PNG".as_slice()),
            ("word/media/truncated.png", "image/png", &valid_png(0)[..20]),
            ("word/media/corrupt.png", "image/png", corrupt_crc.as_slice()),
            ("word/media/broken-stream.png", "image/png", corrupt_idat.as_slice()),
            ("word/media/huge.png", "image/png", oversized_header.as_slice()),
            ("word/media/mismatch.png", "image/jpeg", mismatch.as_slice()),
            ("word/media/fake.jpg", "image/jpeg", mismatch.as_slice()),
            ("word/media/truncated.jpg", "image/jpeg", truncated_jpeg.as_slice()),
            ("word/media/ole.png", "image/png", b"\xd0\xcf\x11\xe0OLE"),
            ("word/media/program.png", "image/png", b"MZ"),
            ("word/media/vector.png", "image/png", b"<svg><script/></svg>"),
            ("word/media/opaque.bin", "application/octet-stream", b"opaque"),
            ("word/media/vector.svg", "image/svg+xml", b"<svg><script/></svg>"),
        ];
        for (part, content_type, bytes) in adversarial {
            assert!(matches!(
                convert_docx(
                    &image_package(part, content_type, bytes),
                    &strict_options(),
                    &context(),
                ),
                Err(ConversionError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn relationships_are_type_checked_and_unreferenced_parts_cannot_inject() {
        let safe = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>safe</w:t></w:r></w:p></w:body></w:document>"#
        );
        let header = format!(
            r#"<w:hdr xmlns:w="{WORD}"><w:p><w:r><w:t>injected-header</w:t></w:r></w:p></w:hdr>"#
        );
        let comments = format!(
            r#"<w:comments xmlns:w="{WORD}"><w:comment w:id="1"><w:p><w:r><w:t>injected-comment</w:t></w:r></w:p></w:comment></w:comments>"#
        );
        let output = convert_docx(
            &base(
                safe.as_bytes(),
                &[
                    ("word/header1.xml", header.as_bytes()),
                    ("word/comments.xml", comments.as_bytes()),
                ],
            ),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        let markdown =
            render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
        assert!(markdown.contains("safe"));
        assert!(!markdown.contains("injected-header") && !markdown.contains("injected-comment"));

        let image_document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}" xmlns:a="{DRAWING}"><w:body><w:p><w:r><w:drawing><a:blip r:embed="rWrong"/></w:drawing></w:r></w:p></w:body></w:document>"#
        );
        let wrong_rels = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rWrong" Type="{REL_TYPE_PREFIX}comments" Target="media/a.png"/></Relationships>"#
        );
        let image = valid_png(0);
        assert!(matches!(
            convert_docx(
                &base(
                    image_document.as_bytes(),
                    &[
                        ("word/_rels/document.xml.rels", wrong_rels.as_bytes()),
                        ("word/media/a.png", image.as_slice()),
                    ],
                ),
                &ConversionOptions::default(),
                &context(),
            ),
            Err(ConversionError::Malformed { .. })
        ));
    }

    fn renamed_macro_package(by_content_type: bool) -> Vec<u8> {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>safe</w:t></w:r></w:p></w:body></w:document>"#
        );
        let macro_part =
            if by_content_type { "word/media/renamed.dat" } else { "word/media/renamed.rels" };
        let macro_override = if by_content_type {
            r#"<Override PartName="/word/media/renamed.dat" ContentType="application/vnd.ms-office.vbaProject"/>"#
        } else {
            ""
        };
        let types = format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="dat" ContentType="application/octet-stream"/><Override PartName="/word/document.xml" ContentType="{DOC_CONTENT_TYPE}"/>{macro_override}</Types>"#
        );
        let root = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rDocument" Type="{OFFICE_REL_TYPE}" Target="word/document.xml"/></Relationships>"#
        );
        let macro_relation = if by_content_type {
            String::new()
        } else {
            format!(
                r#"<Relationship Id="rMacro" Type="{REL_TYPE_PREFIX}vbaProject" Target="media/renamed.rels"/>"#
            )
        };
        let rels =
            format!(r#"<Relationships xmlns="{PACKAGE_REL}">{macro_relation}</Relationships>"#);
        let marker = b"UNIQUE_CORRUPTED_VBA_PAYLOAD".to_vec();
        let mut bytes = package(&[
            ("[Content_Types].xml".into(), types.into_bytes()),
            ("_rels/.rels".into(), root.into_bytes()),
            ("word/document.xml".into(), document.into_bytes()),
            (macro_part.into(), marker.clone()),
            ("word/_rels/document.xml.rels".into(), rels.into_bytes()),
        ]);
        let offset = bytes.windows(marker.len()).position(|value| value == marker).unwrap();
        bytes[offset] ^= 0x40;
        bytes
    }

    #[test]
    fn content_types_and_relationship_types_exclude_renamed_macros_before_decompression() {
        for bytes in [renamed_macro_package(true), renamed_macro_package(false)] {
            let output = convert_docx(&bytes, &ConversionOptions::default(), &context()).unwrap();
            assert!(output.diagnostics.iter().any(|d| d.code == "docx.macrosIgnored"));
            assert_eq!(
                output.document.metadata.properties.get("docx.macrosPresent").map(String::as_str),
                Some("true")
            );
        }
    }

    #[test]
    fn peak_memory_boundary_is_stable_and_assets_transfer_without_copying() {
        let text = "x".repeat(32 * 1024);
        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}" xmlns:a="{DRAWING}"><w:body><w:p><w:r><w:t>{text}</w:t></w:r><w:r><w:drawing><a:blip r:embed="rImage"/><a:blip r:embed="rImage"/></w:drawing></w:r></w:p></w:body></w:document>"#
        );
        let rels = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rImage" Type="{REL_TYPE_PREFIX}image" Target="media/large.png"/></Relationships>"#
        );
        let image = valid_png(64 * 1024);
        let bytes = base(
            document.as_bytes(),
            &[
                ("word/_rels/document.xml.rels", rels.as_bytes()),
                ("word/media/large.png", image.as_slice()),
            ],
        );
        let succeeds =
            |limit| convert_docx(&bytes, &ConversionOptions::default(), &limited_context(limit));
        let mut low = 0_u64;
        let mut high = 4 * 1024 * 1024_u64;
        assert!(succeeds(high).is_ok());
        while low + 1 < high {
            let middle = low + (high - low) / 2;
            if succeeds(middle).is_ok() {
                high = middle;
            } else {
                low = middle;
            }
        }
        assert!(matches!(
            succeeds(high - 1),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        let output = succeeds(high).unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].bytes.len(), image.len());
    }

    #[test]
    fn producer_style_non_empty_blip_preserves_the_image_reference() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}" xmlns:a="{DRAWING}"><w:body><w:p><w:r><w:drawing><a:blip r:embed="rImage"><a:extLst><a:ext uri="{{producer-extension}}"/></a:extLst></a:blip></w:drawing></w:r></w:p></w:body></w:document>"#
        );
        let rels = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rImage" Type="{REL_TYPE_PREFIX}image" Target="media/producer.png"/></Relationships>"#
        );
        let image = valid_png(256);
        let bytes = base(
            document.as_bytes(),
            &[
                ("word/_rels/document.xml.rels", rels.as_bytes()),
                ("word/media/producer.png", image.as_slice()),
            ],
        );

        let output = convert_docx(&bytes, &ConversionOptions::default(), &context()).unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].bytes.as_slice(), image.as_slice());
    }

    #[test]
    fn nested_tables_and_vertical_merges_have_stable_errors() {
        let nested = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:tbl><w:tr><w:tc><w:tbl><w:tr><w:tc><w:p><w:r><w:t>x</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:tc></w:tr></w:tbl></w:body></w:document>"#
        );
        let merged = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:tbl><w:tr><w:tc><w:tcPr><w:vMerge w:val="restart"/></w:tcPr><w:p><w:r><w:t>x</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#
        );
        for (document, expected) in [
            (nested, "nested tables are unsupported"),
            (merged, "vertical table merges are unsupported"),
        ] {
            let recovered = convert_docx(
                &base(document.as_bytes(), &[]),
                &ConversionOptions::default(),
                &context(),
            )
            .unwrap();
            assert!(
                recovered
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.code == "word.tableNormalized")
            );
            match convert_docx(&base(document.as_bytes(), &[]), &strict_options(), &context()) {
                Err(ConversionError::Malformed { detail, .. }) => {
                    assert!(detail.contains(expected));
                }
                other => panic!("expected stable table diagnostic, got {other:?}"),
            }
        }
    }

    #[test]
    fn corruption_dtd_traversal_and_budgets_fail_closed() {
        assert!(matches!(
            convert_docx(b"PK bad", &ConversionOptions::default(), &context()),
            Err(ConversionError::Malformed { .. })
        ));
        let dtd = base(
            format!(r#"<!DOCTYPE x [<!ENTITY a "x">]><w:document xmlns:w="{WORD}"><w:body/></w:document>"#).as_bytes(),
            &[],
        );
        assert!(matches!(
            convert_docx(&dtd, &ConversionOptions::default(), &context()),
            Err(ConversionError::Malformed { .. })
        ));
        let traversal = package(&[
            ("[Content_Types].xml".into(), format!(r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="{DOC_CONTENT_TYPE}"/></Types>"#).into_bytes()),
            ("../word/document.xml".into(), b"x".to_vec()),
        ]);
        assert!(matches!(
            convert_docx(&traversal, &ConversionOptions::default(), &context()),
            Err(ConversionError::Malformed { .. })
        ));
        let escaping_relationship = base(
            format!(r#"<w:document xmlns:w="{WORD}"><w:body/></w:document>"#).as_bytes(),
            &[(
                "word/_rels/document.xml.rels",
                format!(r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="bad" Type="{REL_TYPE_PREFIX}image" Target="../../secret"/></Relationships>"#).as_bytes(),
            )],
        );
        assert!(matches!(
            convert_docx(&escaping_relationship, &ConversionOptions::default(), &context()),
            Err(ConversionError::Malformed { .. })
        ));
        let valid_document = format!(r#"<w:document xmlns:w="{WORD}"><w:body/></w:document>"#);
        let valid = base(valid_document.as_bytes(), &[]);
        let mut options = ConversionOptions::default();
        options.limits.max_archive_entries = 2;
        assert!(matches!(
            convert_docx(&valid, &options, &context()),
            Err(ConversionError::ResourceLimit { limit: "max_archive_entries", .. })
        ));
        let deeply_nested = base(
            format!(r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>x</w:t></w:r></w:p></w:body></w:document>"#).as_bytes(),
            &[],
        );
        let mut depth_options = ConversionOptions::default();
        depth_options.limits.max_nesting_depth = 2;
        assert!(matches!(
            convert_docx(&deeply_nested, &depth_options, &context()),
            Err(ConversionError::ResourceLimit { limit: "max_nesting_depth", .. })
        ));
    }

    #[test]
    fn encrypted_ooxml_wrapper_has_stable_error() {
        let mut ole = vec![0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
        ole.extend_from_slice(b"EncryptionInfo\0EncryptedPackage");
        assert!(matches!(
            convert_docx(&ole, &ConversionOptions::default(), &context()),
            Err(ConversionError::Encrypted)
        ));

        let document = format!(r#"<w:document xmlns:w="{WORD}"><w:body/></w:document>"#);
        let mut encrypted_zip = base(document.as_bytes(), &[]);
        let local = encrypted_zip.windows(4).position(|value| value == b"PK\x03\x04").unwrap();
        encrypted_zip[local + 6] |= 1;
        let central = encrypted_zip.windows(4).position(|value| value == b"PK\x01\x02").unwrap();
        encrypted_zip[central + 8] |= 1;
        assert!(matches!(
            convert_docx(&encrypted_zip, &ConversionOptions::default(), &context()),
            Err(ConversionError::Encrypted)
        ));
    }
}

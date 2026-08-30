use super::*;

fn root_message(records: &[PropertyRecord]) -> Vec<u8> {
    let mut entries = root_property_streams(records);
    entries.push(TestEntry {
        path: vec!["__properties_version1.0".into()],
        bytes: Some(root_properties_stream(records, 0, 0)),
    });
    cfb(entries)
}

fn best_effort() -> ConversionOptions {
    ConversionOptions { error_policy: ErrorPolicy::BestEffort, ..Default::default() }
}

fn strict() -> ConversionOptions {
    ConversionOptions { error_policy: ErrorPolicy::Strict, ..Default::default() }
}

#[test]
fn message_and_html_codepages_are_independent_without_replacement() {
    let (subject, _, errors) = encoding_rs::WINDOWS_1251.encode("Тема");
    assert!(!errors);
    for kind in [0x0102, 0x001e] {
        let bytes = root_message(&[
            property_long(0x3ffd, 1251),
            property_long(0x3fde, 65001),
            root_string8(0x0037, &subject),
            variable(0x1013, kind, "<main><p>HTML öäü 中文</p></main>".as_bytes().to_vec()),
        ]);
        let output = convert(&bytes).unwrap();
        assert_eq!(output.document.metadata.title.as_deref(), Some("Тема"));
        assert_eq!(output.document.metadata.properties["msg.body_kind"], "html");
        assert!(paragraph_text(&output).contains("HTML öäü 中文"));
        assert!(!paragraph_text(&output).contains('\u{fffd}'));
    }
    let (html, _, errors) = encoding_rs::WINDOWS_1251.encode("<main><p>автоматически</p></main>");
    assert!(!errors);
    let bytes = root_message(&[
        property_long(0x3ff1, 1031),
        property_long(0x3fde, 1251),
        root_string8(0x0037, b"Subject \xf6\xe4\xfc"),
        variable(0x1013, 0x0102, html.into_owned()),
    ]);
    let output = convert(&bytes).unwrap();
    assert_eq!(output.document.metadata.title.as_deref(), Some("Subject öäü"));
    assert!(paragraph_text(&output).contains("автоматически"));
}

#[test]
fn locale_default_and_child_properties_keep_the_message_encoding() {
    for (locale, encoding, expected) in
        [(1049, encoding_rs::WINDOWS_1251, "Тема"), (1028, encoding_rs::BIG5, "中文")]
    {
        let (encoded, _, errors) = encoding.encode(expected);
        assert!(!errors);
        let records = [property_long(0x3ff1, locale), root_string8(0x1000, &encoded)];
        let output = convert(&root_message(&records)).unwrap();
        assert!(paragraph_text(&output).contains(expected));
    }
    let (display, _, errors) = encoding_rs::WINDOWS_1251.encode("Имя");
    assert!(!errors);
    let root = [property_long(0x3ffd, 1251), property_unicode(0x1000, "Body")];
    let mut entries = root_property_streams(&root);
    entries.push(TestEntry {
        path: vec!["__properties_version1.0".into()],
        bytes: Some(root_properties_stream(&root, 1, 0)),
    });
    let recipient_path = vec!["__recip_version1.0_#00000000".into()];
    let recipient = [property_long(0x0c15, 1), root_string8(0x3001, &display)];
    entries.extend(property_streams(&recipient_path, &recipient));
    entries.push(TestEntry {
        path: joined(&recipient_path, "__properties_version1.0"),
        bytes: Some(properties_stream(false, &recipient)),
    });
    let bytes = cfb(entries);
    assert_eq!(convert_with(&bytes, &strict()).unwrap_err().code(), ErrorCode::Malformed);
    let output = convert_with(&bytes, &best_effort()).unwrap();
    assert_eq!(output.document.metadata.properties["msg.to"], "Имя");
    assert!(paragraph_text(&output).contains("To: Имя\n"));
    assert!(output.diagnostics.iter().any(|item| item.code == "msg.recipientAddressMissing"));
}

#[test]
fn stored_terminators_and_empty_metadata_have_exact_best_effort_boundaries() {
    for kind in [0x001e, 0x001f] {
        let bytes = if kind == 0x001e {
            b"Body\0".to_vec()
        } else {
            "Body\0".encode_utf16().flat_map(u16::to_le_bytes).collect()
        };
        for count_stored_terminator in [false, true] {
            let mut body = variable(0x1000, kind, bytes.clone());
            if count_stored_terminator {
                body.value[..4].copy_from_slice(&u32::try_from(bytes.len()).unwrap().to_le_bytes());
            }
            let source = root_message(&[body, variable(0x2000, kind, vec![])]);
            assert_eq!(convert_with(&source, &strict()).unwrap_err().code(), ErrorCode::Malformed);
            let output = convert_with(&source, &best_effort()).unwrap();
            assert_eq!(paragraph_text(&output), "Body\n");
            assert!(
                output.diagnostics.iter().any(|item| item.code == "msg.stringTerminatorIgnored")
            );
            assert!(output.diagnostics.iter().any(|item| item.code == "msg.emptyStringProperty"));
        }
    }
    for body in [
        root_string8(0x1000, b"a\0b"),
        root_string8(0x1000, b"Body\0\0"),
        variable(0x1000, 0x001f, vec![0, 0xd8]),
        variable(0x1000, 0x001f, vec![b'a', 0, 0]),
    ] {
        assert_eq!(
            convert_with(&root_message(&[body]), &best_effort()).unwrap_err().code(),
            ErrorCode::Malformed
        );
    }
    let mut mismatch = root_string8(0x1000, b"Body\0");
    mismatch.value[..4].copy_from_slice(&999_u32.to_le_bytes());
    assert!(convert_with(&root_message(&[mismatch]), &best_effort()).is_err());
}

#[test]
fn unused_padding_does_not_change_values_or_weaken_declared_streams() {
    let mut value = property_long(0x4000, 7);
    value.value[4..].fill(0x9a);
    let bytes = root_message(&[value, property_unicode(0x1000, "Body")]);
    assert_eq!(convert_with(&bytes, &strict()).unwrap_err().code(), ErrorCode::Malformed);
    let output = convert_with(&bytes, &best_effort()).unwrap();
    assert!(paragraph_text(&output).contains("Body"));
    assert!(output.diagnostics.iter().any(|item| item.code == "msg.propertyPaddingIgnored"));

    let records = [property_unicode(0x1000, "Body")];
    let missing = cfb(vec![TestEntry {
        path: vec!["__properties_version1.0".into()],
        bytes: Some(root_properties_stream(&records, 0, 0)),
    }]);
    assert_eq!(convert_with(&missing, &best_effort()).unwrap_err().code(), ErrorCode::Malformed);
}

#[test]
fn rtf_boundary_is_one_authenticated_nul_and_never_a_body_fallback() {
    let raw = b"{\\rtf1\\ansi RTF body}\r\n\0";
    for envelope in [lzfu_uncompressed(raw), lzfu_compressed_literals(raw)] {
        let bytes = message(vec![], vec![], Some("must not select plain"), None, Some(&envelope));
        assert!(convert_with(&bytes, &strict()).is_err());
        let output = convert_with(&bytes, &best_effort()).unwrap();
        assert_eq!(output.document.metadata.properties["msg.body_kind"], "rtf");
        assert!(paragraph_text(&output).contains("RTF body"));
        assert!(!paragraph_text(&output).contains("must not select plain"));
        assert!(output.diagnostics.iter().any(|item| item.code == "msg.rtfTerminatorIgnored"));
    }
    for raw in [
        b"{\\rtf1 body}\0\0".as_slice(),
        b"{\\rtf1 body}hidden\0",
        b"{\\rtf1 body}\0{\\rtf1 hidden}",
    ] {
        let bytes = message(
            vec![],
            vec![],
            Some("must not select plain"),
            None,
            Some(&lzfu_uncompressed(raw)),
        );
        assert_eq!(convert_with(&bytes, &best_effort()).unwrap_err().code(), ErrorCode::Malformed);
    }
    let mut corrupt = lzfu_compressed_literals(raw);
    corrupt[12] ^= 1;
    let bytes = message(vec![], vec![], Some("must not select plain"), None, Some(&corrupt));
    assert_eq!(convert_with(&bytes, &best_effort()).unwrap_err().code(), ErrorCode::Malformed);
}

#[test]
fn empty_body_preserves_real_headers_and_attachments_without_plain_fallback() {
    let source = message(
        vec![],
        vec![],
        Some("must not select plain"),
        None,
        Some(&lzfu_uncompressed(b"{\\rtf1\\ansi \\~}")),
    );
    let output = convert(&source).unwrap();
    assert_eq!(output.document.metadata.title.as_deref(), Some("Repository MSG"));
    assert_eq!(output.document.metadata.properties["msg.body_kind"], "rtf");
    assert!(!paragraph_text(&output).contains("must not select plain"));
    assert_eq!(
        output.source_content_evidence(),
        into_markdown_core::SourceContentEvidence::Unknown
    );

    let source = message(
        vec![AttachmentFixture::value("note.txt", "text/plain", None, b"Attachment".to_vec())],
        vec![],
        None,
        None,
        None,
    );
    let output = convert(&source).unwrap();
    assert_eq!(output.document.metadata.properties["msg.body_kind"], "empty");
    assert!(paragraph_text(&output).contains("note.txt"));
    assert_eq!(output.assets.len(), 1);
    assert_eq!(
        output.source_content_evidence(),
        into_markdown_core::SourceContentEvidence::Unknown
    );

    let empty_html = message(vec![], vec![], Some("fallback"), Some(b"<p> </p>"), None);
    assert!(convert(&empty_html).is_err());
}

#[test]
fn genuinely_empty_messages_are_certified_only_after_scanning_all_content() {
    for records in [
        vec![],
        vec![property_unicode(0x1000, " \r\n")],
        vec![variable(0x1009, 0x0102, lzfu_uncompressed(b"{\\rtf1\\ansi}"))],
    ] {
        let output = convert(&root_message(&records)).unwrap();
        assert!(into_markdown_core::document_is_empty(&output.document));
        assert!(output.assets.is_empty());
        assert_eq!(
            output.source_content_evidence(),
            into_markdown_core::SourceContentEvidence::Empty
        );
    }
}

#[test]
fn actual_unknown_and_non_ascii_ascii_codepages_fail_closed() {
    for codepage in [42, 20127] {
        let bytes = root_message(&[property_long(0x3ffd, codepage), root_string8(0x1000, b"\xe9")]);
        assert_eq!(convert_with(&bytes, &best_effort()).unwrap_err().code(), ErrorCode::Malformed);
        let bytes = root_message(&[
            property_long(0x3fde, codepage),
            variable(0x1013, 0x0102, b"<p>\xe9</p>".to_vec()),
        ]);
        assert_eq!(convert_with(&bytes, &best_effort()).unwrap_err().code(), ErrorCode::Malformed);
    }
}

#[test]
fn encrypted_containers_and_reference_or_ole_attachments_remain_rejected() {
    let encrypted = cfb(vec![
        TestEntry { path: vec!["EncryptionInfo".into()], bytes: Some(vec![1; 16]) },
        TestEntry { path: vec!["EncryptedPackage".into()], bytes: Some(vec![2; 32]) },
    ]);
    assert_eq!(convert_with(&encrypted, &best_effort()).unwrap_err().code(), ErrorCode::Malformed);
    for method in [2, 3, 4, 6] {
        let root = [property_unicode(0x1000, "Body")];
        let mut entries = root_property_streams(&root);
        entries.push(TestEntry {
            path: vec!["__properties_version1.0".into()],
            bytes: Some(root_properties_stream(&root, 0, 1)),
        });
        let path = vec!["__attach_version1.0_#00000000".into()];
        let records = [property_long(0x3705, method), property_unicode(0x3707, "attachment.bin")];
        entries.extend(property_streams(&path, &records));
        entries.push(TestEntry {
            path: joined(&path, "__properties_version1.0"),
            bytes: Some(properties_stream(false, &records)),
        });
        assert_eq!(
            convert_with(&cfb(entries), &best_effort()).unwrap_err().code(),
            ErrorCode::Malformed
        );
    }
}

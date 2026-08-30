use super::super::convert_presentation;
use super::support::{corrupt_stored_entry, fixture};
use into_markdown_core::{
    ConversionError, ConversionOptions, ConverterOutput, DiagnosticSeverity, ErrorPolicy,
    ExecutionContext, ExecutionOptions,
};

fn convert(bytes: &[u8], options: &ConversionOptions) -> Result<ConverterOutput, ConversionError> {
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    convert_presentation(bytes, options, &context)
}

fn presentation(comment: &[u8]) -> Vec<u8> {
    let mut bytes = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let end = bytes.len();
    bytes[end - 2..].copy_from_slice(&u16::try_from(comment.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(comment);
    bytes
}

#[test]
fn trailing_lf_crlf_and_bounded_whitespace_preserve_content_and_zip_comments() {
    let options = ConversionOptions { error_policy: ErrorPolicy::BestEffort, ..Default::default() };
    let strict = ConversionOptions { error_policy: ErrorPolicy::Strict, ..Default::default() };
    for comment in [Vec::new(), b"audit comment\r\n ".to_vec(), vec![b'x'; 65_535]] {
        let original = presentation(&comment);
        let expected = convert(&original, &options).unwrap();
        assert!(convert(&original, &strict).is_ok());
        for tail in [b"\n".as_slice(), b"\r\n", &[b' '; 64], b" \t\r\n"] {
            let mut bytes = original.clone();
            bytes.extend_from_slice(tail);
            let mut actual = convert(&bytes, &options).unwrap();
            assert_eq!(actual.document, expected.document);
            assert_eq!(actual.assets, expected.assets);
            let diagnostic = actual.diagnostics.remove(0);
            assert_eq!(diagnostic.code, "presentation.zipTrailingWhitespaceIgnored");
            assert_eq!(diagnostic.severity, DiagnosticSeverity::Info);
            assert!(diagnostic.message.starts_with(&format!("ignored {} whitespace", tail.len())));
            assert_eq!(actual.diagnostics, expected.diagnostics);
            assert!(matches!(convert(&bytes, &strict), Err(ConversionError::Malformed { .. })));
        }
    }
}

#[test]
fn non_whitespace_and_excessive_zip_tails_remain_malformed() {
    let options = ConversionOptions { error_policy: ErrorPolicy::BestEffort, ..Default::default() };
    for tail in [b"\0".as_slice(), b"\nX", b"\x0b", &[b'\n'; 65], b"PK\x05\x06"] {
        let mut bytes = presentation(b"comment");
        bytes.extend_from_slice(tail);
        assert!(matches!(convert(&bytes, &options), Err(ConversionError::Malformed { .. })));
    }
}

#[test]
fn tolerated_tail_does_not_bypass_crc_central_directory_or_resource_limits() {
    let options = ConversionOptions { error_policy: ErrorPolicy::BestEffort, ..Default::default() };
    let original = presentation(b"");
    let mut bad_crc = corrupt_stored_entry(original.clone(), "ppt/slides/slide1.xml");
    bad_crc.push(b'\n');
    assert!(matches!(convert(&bad_crc, &options), Err(ConversionError::Malformed { .. })));
    let mut bad_central = original.clone();
    let end = bad_central.len();
    bad_central[end - 6..end - 2].copy_from_slice(&0_u32.to_le_bytes());
    bad_central.push(b'\n');
    assert!(matches!(convert(&bad_central, &options), Err(ConversionError::Malformed { .. })));

    let mut bytes = original;
    bytes.push(b'\n');
    let mut limited = options.clone();
    limited.limits.max_input_bytes = u64::try_from(bytes.len() - 1).unwrap();
    assert!(matches!(
        convert(&bytes, &limited),
        Err(ConversionError::ResourceLimit { limit: "max_input_bytes", .. })
    ));
    limited = options;
    limited.limits.max_archive_entries = 1;
    assert!(matches!(
        convert(&bytes, &limited),
        Err(ConversionError::ResourceLimit { limit: "max_archive_entries", .. })
    ));
}

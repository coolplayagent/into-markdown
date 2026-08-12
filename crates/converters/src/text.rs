use chardetng::EncodingDetector;
use encoding_rs::{
    BIG5, Encoding, GB18030, GBK, SHIFT_JIS, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252,
};
use into_markdown_core::{
    Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput,
    Diagnostic, DiagnosticSeverity, Document, ExecutionContext, FormatCandidate, Inline,
    InputFormat, MAX_DOCUMENT_NODES, NodeId, ProbeOutcome, Provenance, ProvenanceKind,
    ResolvedInput, Services, SourceLocator, TextDecodingMode,
};

const TEXT_FORMATS: &[InputFormat] = &[InputFormat::Text];
const PROVIDER_ID: &str = "builtin.converter.text";
const INVALID_SEQUENCE_CODE: &str = "text.invalidByteSequenceReplaced";
const MAX_UNSAFE_PERCENT: usize = 1;
const MIN_PRINTABLE_PERCENT: usize = 95;

/// Plain-text converter with bounded character-set decoding and source-byte provenance.
#[derive(Debug, Default)]
pub struct TextConverter;

impl Converter for TextConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        100
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        TEXT_FORMATS
    }

    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            if candidate.format != InputFormat::Text {
                return Ok(ProbeOutcome::NotApplicable);
            }
            let explicit_charset_hint = candidate.detector_id == "builtin.detector.hints"
                && candidate.evidence.contains("character encoding hint");
            if candidate.explicit || explicit_charset_hint || sniff_text(&input.bytes).is_some() {
                Ok(ProbeOutcome::Match { confidence: 1.0 })
            } else {
                Ok(ProbeOutcome::NotApplicable)
            }
        })
    }

    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move { convert_text(input, options, context) })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Charset {
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
    Gb18030,
    Big5,
    ShiftJis,
}

impl Charset {
    const fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16Le => "utf-16le",
            Self::Utf16Be => "utf-16be",
            Self::Windows1252 => "windows-1252",
            Self::Gb18030 => "gb18030",
            Self::Big5 => "big5",
            Self::ShiftJis => "shift_jis",
        }
    }

    const fn encoding(self) -> &'static Encoding {
        match self {
            Self::Utf8 => UTF_8,
            Self::Utf16Le => UTF_16LE,
            Self::Utf16Be => UTF_16BE,
            Self::Windows1252 => WINDOWS_1252,
            Self::Gb18030 => GB18030,
            Self::Big5 => BIG5,
            Self::ShiftJis => SHIFT_JIS,
        }
    }
}

fn normalize_charset(label: &str) -> Result<Charset, ConversionError> {
    let normalized = label.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    let charset = match normalized.as_str() {
        "utf-8" | "utf8" => Charset::Utf8,
        "utf-16le" | "utf16le" => Charset::Utf16Le,
        "utf-16be" | "utf16be" => Charset::Utf16Be,
        "windows-1252" | "windows1252" | "cp1252" => Charset::Windows1252,
        "gb18030" | "gb-18030" => Charset::Gb18030,
        "big5" | "big-5" => Charset::Big5,
        "shift-jis" | "shiftjis" | "sjis" | "cp932" | "windows-31j" => Charset::ShiftJis,
        _ => {
            return Err(ConversionError::Malformed {
                part: Some("charset".into()),
                detail: format!("unsupported character encoding label: {label}"),
            });
        }
    };
    Ok(charset)
}

fn bom(bytes: &[u8]) -> Option<(Charset, usize)> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        Some((Charset::Utf8, 3))
    } else if bytes.starts_with(&[0xff, 0xfe]) {
        Some((Charset::Utf16Le, 2))
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        Some((Charset::Utf16Be, 2))
    } else {
        None
    }
}

pub(crate) fn sniff_text(bytes: &[u8]) -> Option<f32> {
    if bytes.is_empty() {
        return Some(0.80);
    }
    if bom(bytes).is_some() {
        return Some(0.99);
    }
    if !raw_text_safe(bytes) {
        return None;
    }
    if let Ok(text) = std::str::from_utf8(bytes)
        && decoded_text_safe(text)
    {
        return Some(0.88);
    }
    let charset = detect_legacy(bytes)?;
    let text = charset.encoding().decode_without_bom_handling_and_without_replacement(bytes)?;
    (decoded_text_safe(&text) && legacy_roundtrips(charset, bytes, &text)).then_some(0.72)
}

fn raw_text_safe(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return false;
    }
    let controls = bytes
        .iter()
        .filter(|&&byte| byte < 0x20 && !matches!(byte, b'\t' | b'\n' | b'\r' | 0x0c))
        .count();
    controls.saturating_mul(100) <= bytes.len().saturating_mul(MAX_UNSAFE_PERCENT)
}

fn decoded_text_safe(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    let mut total = 0_usize;
    let mut unsafe_controls = 0_usize;
    let mut replacement = 0_usize;
    let mut printable = 0_usize;
    for value in text.chars() {
        total += 1;
        unsafe_controls +=
            usize::from(value.is_control() && !matches!(value, '\t' | '\n' | '\r' | '\u{c}'));
        replacement += usize::from(value == '\u{fffd}');
        printable +=
            usize::from(!value.is_control() || matches!(value, '\t' | '\n' | '\r' | '\u{c}'));
    }
    unsafe_controls.saturating_mul(100) <= total.saturating_mul(MAX_UNSAFE_PERCENT)
        && replacement.saturating_mul(100) <= total.saturating_mul(MAX_UNSAFE_PERCENT)
        && printable.saturating_mul(100) >= total.saturating_mul(MIN_PRINTABLE_PERCENT)
}

fn legacy_roundtrips(charset: Charset, bytes: &[u8], text: &str) -> bool {
    let (encoded, _, had_errors) = charset.encoding().encode(text);
    !had_errors && encoded.as_ref() == bytes
}

fn detect_legacy(bytes: &[u8]) -> Option<Charset> {
    let mut detector = EncodingDetector::new();
    detector.feed(bytes, true);
    let guessed = detector.guess(None, true);
    if guessed == WINDOWS_1252 {
        Some(Charset::Windows1252)
    } else if guessed == GB18030 || guessed == GBK {
        Some(Charset::Gb18030)
    } else if guessed == BIG5 {
        Some(Charset::Big5)
    } else if guessed == SHIFT_JIS {
        Some(Charset::ShiftJis)
    } else {
        None
    }
}

fn select_charset(
    bytes: &[u8],
    explicit: Option<&str>,
) -> Result<(Charset, usize), ConversionError> {
    let detected_bom = bom(bytes);
    if let Some(label) = explicit {
        let selected = normalize_charset(label)?;
        if let Some((marked, offset)) = detected_bom {
            if selected != marked {
                return Err(ConversionError::Malformed {
                    part: Some("charset".into()),
                    detail: format!(
                        "explicit {} conflicts with {} BOM",
                        selected.name(),
                        marked.name()
                    ),
                });
            }
            return Ok((selected, offset));
        }
        return Ok((selected, 0));
    }
    if let Some(marked) = detected_bom {
        return Ok(marked);
    }
    if std::str::from_utf8(bytes).is_ok() {
        return Ok((Charset::Utf8, 0));
    }
    if !raw_text_safe(bytes) {
        return Err(ConversionError::Unsupported {
            detail: "input does not satisfy plain-text binary safety thresholds".into(),
        });
    }
    let charset = detect_legacy(bytes).ok_or_else(|| ConversionError::Malformed {
        part: Some("charset".into()),
        detail: "character encoding could not be detected with sufficient confidence".into(),
    })?;
    let _text = charset
        .encoding()
        .decode_without_bom_handling_and_without_replacement(bytes)
        .filter(|text| decoded_text_safe(text) && legacy_roundtrips(charset, bytes, text))
        .ok_or_else(|| ConversionError::Malformed {
            part: Some("charset".into()),
            detail:
                "detected encoding did not pass replacement, printable, and round-trip thresholds"
                    .into(),
        })?;
    Ok((charset, 0))
}

fn convert_text(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let size = u64::try_from(input.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_input_bytes",
        detail: "text input size cannot be represented as u64".into(),
    })?;
    if size > options.limits.max_input_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: format!("{size} > {}", options.limits.max_input_bytes),
        });
    }
    let (charset, bom_len) = select_charset(&input.bytes, options.text.charset.as_deref())?;
    // Legacy single-byte characters can expand to three UTF-8 bytes. Count raw
    // newline bytes conservatively to cover line vectors and resulting IR
    // containers while both are live (UTF-16 non-newline low bytes may only
    // over-reserve, never under-reserve).
    let newline_count = input.bytes.iter().filter(|&&byte| matches!(byte, b'\r' | b'\n')).count();
    let newline_count =
        u64::try_from(newline_count).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "text newline count cannot be represented as u64".into(),
        })?;
    let working_bytes = size
        .checked_mul(3)
        .and_then(|text| newline_count.checked_mul(96).and_then(|lines| text.checked_add(lines)))
        .ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "text decoding memory estimate overflowed".into(),
        })?;
    let _working_memory = context.reserve_memory(working_bytes)?;
    let (lines, diagnostics) = decode_lines(
        &input.bytes[bom_len..],
        bom_len,
        charset,
        options.text.decoding_mode,
        context,
    )?;
    let document = build_document(lines, context)?;
    Ok(ConverterOutput { document, diagnostics, assets: Vec::new() })
}

fn decode_lines(
    bytes: &[u8],
    base: usize,
    charset: Charset,
    mode: TextDecodingMode,
    context: &ExecutionContext,
) -> Result<(Vec<Line>, Vec<Diagnostic>), ConversionError> {
    match charset {
        Charset::Utf8 => decode_utf8(bytes, base, mode, charset, context),
        Charset::Utf16Le | Charset::Utf16Be => decode_utf16(bytes, base, mode, charset, context),
        _ => decode_legacy(bytes, base, mode, charset, context),
    }
}

fn decode_utf8(
    bytes: &[u8],
    base: usize,
    mode: TextDecodingMode,
    charset: Charset,
    context: &ExecutionContext,
) -> Result<(Vec<Line>, Vec<Diagnostic>), ConversionError> {
    let mut decoded = DecodedLines::default();
    let mut diagnostics = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if offset % 4096 == 0 {
            context.checkpoint()?;
        }
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(valid) => {
                push_str_units(&mut decoded, valid, base + offset);
                break;
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                let valid =
                    std::str::from_utf8(&bytes[offset..offset + valid_len]).map_err(|_| {
                        ConversionError::Internal {
                            detail: "UTF-8 validation prefix changed".into(),
                        }
                    })?;
                push_str_units(&mut decoded, valid, base + offset);
                offset += valid_len;
                let invalid_len = error.error_len().unwrap_or(bytes.len() - offset).max(1);
                recover_invalid(
                    &mut decoded,
                    &mut diagnostics,
                    base + offset,
                    base + offset + invalid_len,
                    charset,
                    mode,
                )?;
                offset += invalid_len;
            }
        }
    }
    Ok((decoded.finish(), diagnostics))
}

fn push_str_units(decoded: &mut DecodedLines, text: &str, base: usize) {
    for (offset, value) in text.char_indices() {
        decoded.push(value, base + offset, base + offset + value.len_utf8());
    }
}

fn decode_utf16(
    bytes: &[u8],
    base: usize,
    mode: TextDecodingMode,
    charset: Charset,
    context: &ExecutionContext,
) -> Result<(Vec<Line>, Vec<Diagnostic>), ConversionError> {
    let mut decoded = DecodedLines::default();
    let mut diagnostics = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if offset % 4096 == 0 {
            context.checkpoint()?;
        }
        if bytes.len() - offset < 2 {
            recover_invalid(
                &mut decoded,
                &mut diagnostics,
                base + offset,
                base + bytes.len(),
                charset,
                mode,
            )?;
            break;
        }
        let read = |at: usize| match charset {
            Charset::Utf16Le => u16::from_le_bytes([bytes[at], bytes[at + 1]]),
            _ => u16::from_be_bytes([bytes[at], bytes[at + 1]]),
        };
        let first = read(offset);
        let (value, width) = if (0xd800..=0xdbff).contains(&first) {
            if bytes.len() - offset < 4 {
                recover_invalid(
                    &mut decoded,
                    &mut diagnostics,
                    base + offset,
                    base + bytes.len(),
                    charset,
                    mode,
                )?;
                break;
            }
            let second = read(offset + 2);
            if !(0xdc00..=0xdfff).contains(&second) {
                recover_invalid(
                    &mut decoded,
                    &mut diagnostics,
                    base + offset,
                    base + offset + 2,
                    charset,
                    mode,
                )?;
                offset += 2;
                continue;
            }
            let scalar =
                0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00);
            (char::from_u32(scalar), 4)
        } else if (0xdc00..=0xdfff).contains(&first) {
            (None, 2)
        } else {
            (char::from_u32(u32::from(first)), 2)
        };
        if let Some(value) = value {
            decoded.push(value, base + offset, base + offset + width);
        } else {
            recover_invalid(
                &mut decoded,
                &mut diagnostics,
                base + offset,
                base + offset + width,
                charset,
                mode,
            )?;
        }
        offset += width;
    }
    Ok((decoded.finish(), diagnostics))
}

fn decode_legacy(
    bytes: &[u8],
    base: usize,
    mode: TextDecodingMode,
    charset: Charset,
    context: &ExecutionContext,
) -> Result<(Vec<Line>, Vec<Diagnostic>), ConversionError> {
    let mut decoded = DecodedLines::default();
    let mut diagnostics = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if offset % 4096 == 0 {
            context.checkpoint()?;
        }
        let width = legacy_width(charset, &bytes[offset..]);
        let end = offset.saturating_add(width).min(bytes.len());
        let decoded_text = charset
            .encoding()
            .decode_without_bom_handling_and_without_replacement(&bytes[offset..end]);
        if let Some(text) = decoded_text.filter(|value| !value.is_empty()) {
            for value in text.chars() {
                decoded.push(value, base + offset, base + end);
            }
        } else {
            recover_invalid(
                &mut decoded,
                &mut diagnostics,
                base + offset,
                base + end,
                charset,
                mode,
            )?;
        }
        offset = end;
    }
    Ok((decoded.finish(), diagnostics))
}

fn legacy_width(charset: Charset, bytes: &[u8]) -> usize {
    let first = bytes[0];
    match charset {
        Charset::ShiftJis if (0x81..=0x9f).contains(&first) || (0xe0..=0xfc).contains(&first) => 2,
        Charset::Big5 if (0x81..=0xfe).contains(&first) => 2,
        Charset::Gb18030 if (0x81..=0xfe).contains(&first) => {
            if bytes.get(1).is_some_and(|value| (0x30..=0x39).contains(value)) { 4 } else { 2 }
        }
        _ => 1,
    }
}

fn recover_invalid(
    decoded: &mut DecodedLines,
    diagnostics: &mut Vec<Diagnostic>,
    start: usize,
    end: usize,
    charset: Charset,
    mode: TextDecodingMode,
) -> Result<(), ConversionError> {
    if mode == TextDecodingMode::Strict {
        return Err(ConversionError::Malformed {
            part: Some("text".into()),
            detail: format!(
                "invalid {} byte sequence at byte range {start}..{end}",
                charset.name()
            ),
        });
    }
    decoded.push('\u{fffd}', start, end);
    diagnostics.push(Diagnostic {
        code: INVALID_SEQUENCE_CODE.into(),
        severity: DiagnosticSeverity::Warning,
        message: format!("replaced invalid {} byte sequence at {start}..{end}", charset.name()),
        locator: Some(byte_locator(start, end)),
    });
    Ok(())
}

#[derive(Debug, Default)]
struct Line {
    text: String,
    start: Option<usize>,
    end: Option<usize>,
}

#[derive(Debug, Default)]
struct DecodedLines {
    lines: Vec<Line>,
    current: Line,
    pending_cr: bool,
}

impl DecodedLines {
    fn push(&mut self, value: char, start: usize, end: usize) {
        if self.pending_cr {
            self.finish_line();
            self.pending_cr = false;
            if value == '\n' {
                return;
            }
        }
        match value {
            '\r' => self.pending_cr = true,
            '\n' => self.finish_line(),
            _ => {
                self.current.start.get_or_insert(start);
                self.current.end = Some(end);
                self.current.text.push(value);
            }
        }
    }

    fn finish_line(&mut self) {
        self.lines.push(std::mem::take(&mut self.current));
    }

    fn finish(mut self) -> Vec<Line> {
        if self.pending_cr {
            self.finish_line();
        }
        self.finish_line();
        self.lines
    }
}

fn build_document(
    lines: Vec<Line>,
    context: &ExecutionContext,
) -> Result<Document, ConversionError> {
    let mut document = Document::default();
    let mut paragraph: Vec<Line> = Vec::new();
    for (index, line) in lines.into_iter().enumerate() {
        if index % 4096 == 0 {
            context.checkpoint()?;
        }
        if line.text.is_empty() {
            flush_paragraph(&mut document, &mut paragraph)?;
        } else {
            paragraph.push(line);
        }
    }
    flush_paragraph(&mut document, &mut paragraph)?;
    Ok(document)
}

fn flush_paragraph(document: &mut Document, lines: &mut Vec<Line>) -> Result<(), ConversionError> {
    if lines.is_empty() {
        return Ok(());
    }
    if document.blocks.len() >= MAX_DOCUMENT_NODES {
        return Err(ConversionError::ResourceLimit {
            limit: "max_document_nodes",
            detail: format!("plain text exceeds {MAX_DOCUMENT_NODES} paragraph nodes"),
        });
    }
    let start = lines.first().and_then(|line| line.start).unwrap_or(0);
    let end = lines.last().and_then(|line| line.end).unwrap_or(start);
    let mut content = Vec::with_capacity(lines.len().saturating_mul(2).saturating_sub(1));
    for (index, line) in lines.drain(..).enumerate() {
        if index > 0 {
            content.push(Inline::LineBreak);
        }
        content.push(Inline::Text { value: line.text, marks: Vec::new() });
    }
    let id = format!("text-paragraph-{}", document.blocks.len() + 1);
    document.blocks.push(BlockNode {
        id: NodeId(id),
        block: Block::Paragraph(content),
        provenance: Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: PROVIDER_ID.into(),
            locator: byte_locator(start, end),
            confidence: Some(1.0),
        },
    });
    Ok(())
}

fn byte_locator(start: usize, end: usize) -> SourceLocator {
    SourceLocator {
        byte_start: u64::try_from(start).ok(),
        byte_end: u64::try_from(end).ok(),
        ..SourceLocator::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits, SourceMetadata};
    use std::sync::Arc;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn input(bytes: &[u8]) -> ResolvedInput {
        ResolvedInput {
            bytes: Arc::from(bytes),
            metadata: SourceMetadata { size: bytes.len() as u64, ..SourceMetadata::default() },
        }
    }

    #[test]
    fn utf8_bom_and_mixed_newlines_preserve_ranges() {
        let source = input(b"\xef\xbb\xbfhello\r\nworld\r\n\rnext\n\nlast");
        let output = convert_text(&source, &ConversionOptions::default(), &context()).unwrap();
        assert_eq!(output.document.blocks.len(), 3);
        assert_eq!(output.document.blocks[0].provenance.locator.byte_start, Some(3));
        assert_eq!(output.document.blocks[0].provenance.locator.byte_end, Some(15));
    }

    #[test]
    fn utf16_surrogate_offsets_reference_source_bytes() {
        let source = input(&[0xff, 0xfe, 0x41, 0x00, 0x3d, 0xd8, 0x00, 0xde]);
        let output = convert_text(&source, &ConversionOptions::default(), &context()).unwrap();
        assert_eq!(output.document.blocks[0].provenance.locator.byte_start, Some(2));
        assert_eq!(output.document.blocks[0].provenance.locator.byte_end, Some(8));
    }

    #[test]
    fn replacement_is_diagnostic_and_strict_is_default() {
        let source = input(b"ok\xffend");
        let mut explicit = ConversionOptions::default();
        explicit.text.charset = Some("utf-8".into());
        assert!(convert_text(&source, &explicit, &context()).is_err());
        explicit.text.decoding_mode = TextDecodingMode::Replace;
        let output = convert_text(&source, &explicit, &context()).unwrap();
        assert_eq!(output.diagnostics[0].code, INVALID_SEQUENCE_CODE);
        assert_eq!(output.diagnostics[0].locator.as_ref().unwrap().byte_start, Some(2));
    }

    #[test]
    fn binary_disguised_as_text_is_not_auto_detected() {
        assert_eq!(sniff_text(b"MZ\0\x01\x02payload"), None);
    }

    #[test]
    fn legacy_allowlist_decodes_mixed_language_text() {
        for (charset, sample) in [
            (Charset::Windows1252, "Café déjà vu — naïve façade €"),
            (Charset::Gb18030, "中华人民共和国简体中文编码测试，这是一段确定的中文文本。"),
            (Charset::Big5, "繁體中文編碼測試，這是一段確定而且足夠長的中文文字。"),
            (Charset::ShiftJis, "日本語の文字コードを判定するための十分に長いテスト文章です。"),
        ] {
            let (encoded, _, had_errors) = charset.encoding().encode(sample);
            assert!(!had_errors, "{} fixture must be encodable", charset.name());
            assert!(sniff_text(&encoded).is_some(), "{} must be auto-detectable", charset.name());
            let source = input(&encoded);
            assert!(
                !convert_text(&source, &ConversionOptions::default(), &context())
                    .unwrap()
                    .document
                    .blocks
                    .is_empty()
            );
            let mut options = ConversionOptions::default();
            options.text.charset = Some(charset.name().into());
            let output = convert_text(&source, &options, &context()).unwrap();
            let Block::Paragraph(content) = &output.document.blocks[0].block else {
                unreachable!();
            };
            assert_eq!(content[0], Inline::Text { value: sample.into(), marks: Vec::new() });
            assert_eq!(
                output.document.blocks[0].provenance.locator.byte_end,
                Some(encoded.len() as u64)
            );
        }
    }

    #[test]
    fn truncated_multibyte_and_odd_utf16_are_never_silent() {
        for (bytes, charset) in [(&[0x81][..], "shift_jis"), (&[0xff, 0xfe, 0x41][..], "utf-16le")]
        {
            let source = input(bytes);
            let mut options = ConversionOptions::default();
            options.text.charset = Some(charset.into());
            assert_eq!(
                convert_text(&source, &options, &context()).unwrap_err().code(),
                into_markdown_core::ErrorCode::Malformed
            );
            options.text.decoding_mode = TextDecodingMode::Replace;
            let recovered = convert_text(&source, &options, &context()).unwrap();
            assert!(!recovered.diagnostics.is_empty());
        }
    }

    #[test]
    fn empty_and_long_lines_are_bounded_and_deterministic() {
        let empty = convert_text(&input(b""), &ConversionOptions::default(), &context()).unwrap();
        assert!(empty.document.blocks.is_empty());
        let long = "a".repeat(256 * 1024);
        let converted =
            convert_text(&input(long.as_bytes()), &ConversionOptions::default(), &context())
                .unwrap();
        assert_eq!(converted.document.blocks.len(), 1);

        let mut limited = ConversionOptions::default();
        limited.limits.max_input_bytes = 3;
        assert_eq!(
            convert_text(&input(b"four"), &limited, &context()).unwrap_err().code(),
            into_markdown_core::ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn unknown_charset_and_bom_conflict_are_rejected() {
        let mut options = ConversionOptions::default();
        options.text.charset = Some("x-user-defined".into());
        assert!(convert_text(&input(b"text"), &options, &context()).is_err());
        options.text.charset = Some("utf-16be".into());
        assert!(convert_text(&input(&[0xff, 0xfe, 0x41, 0]), &options, &context()).is_err());
    }
}

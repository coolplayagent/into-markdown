use chardetng::EncodingDetector;
use encoding_rs::{
    BIG5, DecoderResult, Encoding, GB18030, GBK, SHIFT_JIS, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252,
};
use into_markdown_core::{
    Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput,
    Diagnostic, DiagnosticSeverity, Document, ExecutionContext, FormatCandidate, Inline,
    InputFormat, MAX_DOCUMENT_INLINES, MAX_DOCUMENT_NODES, NodeId, ProbeOutcome, Provenance,
    ProvenanceKind, ResolvedInput, Services, SourceLocator, TextDecodingMode,
};

const TEXT_FORMATS: &[InputFormat] = &[InputFormat::Text];
const PROVIDER_ID: &str = "builtin.converter.text";
const INVALID_SEQUENCE_CODE: &str = "text.invalidByteSequenceReplaced";
const MAX_UNSAFE_PERCENT: usize = 1;
const MIN_PRINTABLE_PERCENT: usize = 95;
const TEXT_SNIFF_BYTE_LIMIT: usize = 64 * 1024;

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
            if candidate.explicit
                || explicit_charset_hint
                || sniff_text(&input.bytes, context)?.is_some()
            {
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

pub(crate) fn sniff_text(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Option<f32>, ConversionError> {
    if bytes.is_empty() {
        return Ok(Some(0.80));
    }
    if super::structured_text_candidate(bytes, context)?.is_some() {
        return Ok(None);
    }
    sniff_unstructured_text(bytes, context)
}

pub(crate) fn sniff_unstructured_text(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Option<f32>, ConversionError> {
    if bytes.is_empty() {
        return Ok(Some(0.80));
    }
    if let Some((charset, bom_len)) = bom(bytes) {
        return sniff_bom_text(
            bytes.get(bom_len..).ok_or_else(|| ConversionError::Internal {
                detail: "BOM offset exceeds text input".into(),
            })?,
            charset,
            context,
        );
    }
    let sample_len = bytes.len().min(TEXT_SNIFF_BYTE_LIMIT);
    let sample = bytes.get(..sample_len).ok_or_else(|| ConversionError::Internal {
        detail: "bounded text sample exceeds input".into(),
    })?;
    if let Ok(text) = std::str::from_utf8(sample)
        && decoded_text_safe(text)
    {
        return Ok(decoded_input_safe(bytes, Charset::Utf8, context)?.then_some(0.88));
    }
    let Some(charset) = detect_legacy(sample) else {
        return Ok(None);
    };
    let Some(text) = charset.encoding().decode_without_bom_handling_and_without_replacement(sample)
    else {
        return Ok(None);
    };
    if !decoded_text_safe(&text) || !legacy_roundtrips(charset, sample, &text) {
        return Ok(None);
    }
    Ok(decoded_input_safe(bytes, charset, context)?.then_some(0.72))
}

fn sniff_bom_text(
    bytes: &[u8],
    charset: Charset,
    context: &ExecutionContext,
) -> Result<Option<f32>, ConversionError> {
    let sample_len = bytes.len().min(TEXT_SNIFF_BYTE_LIMIT);
    let sample = bytes.get(..sample_len).ok_or_else(|| ConversionError::Internal {
        detail: "bounded BOM text sample exceeds input".into(),
    })?;
    let source_truncated = bytes.len() > sample_len;
    let (decoded, trailing_malformed) = match charset {
        Charset::Utf8 => {
            let Some(decoded) = strict_utf8_sample(sample, source_truncated) else {
                return Ok(None);
            };
            decoded
        }
        Charset::Utf16Le | Charset::Utf16Be => {
            let Some(decoded) = strict_utf16_sample(sample, source_truncated, charset) else {
                return Ok(None);
            };
            decoded
        }
        _ => return Ok(None),
    };
    if !decoded_text_safe(&decoded) || !decoded_input_safe(bytes, charset, context)? {
        return Ok(None);
    }
    Ok(Some(if trailing_malformed { 0.80 } else { 0.95 }))
}

fn decoded_input_safe(
    bytes: &[u8],
    charset: Charset,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    let mut decoder = charset.encoding().new_decoder_without_bom_handling();
    let mut output = String::with_capacity(16 * 1024);
    let mut offset = 0_usize;
    while offset < bytes.len() {
        context.checkpoint()?;
        output.clear();
        let end = offset.saturating_add(4096).min(bytes.len());
        let (result, read) =
            decoder.decode_to_string_without_replacement(&bytes[offset..end], &mut output, false);
        if !decoded_text_safe(&output) || matches!(result, DecoderResult::Malformed(_, _)) {
            return Ok(false);
        }
        if result == DecoderResult::OutputFull || read == 0 {
            return Err(ConversionError::Internal {
                detail: format!("{} safety decoder made no bounded progress", charset.name()),
            });
        }
        offset = offset.checked_add(read).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: "text safety decoder position overflowed".into(),
        })?;
    }
    output.clear();
    let (result, read) = decoder.decode_to_string_without_replacement(b"", &mut output, true);
    Ok(read == 0
        && matches!(result, DecoderResult::InputEmpty | DecoderResult::Malformed(_, _))
        && decoded_text_safe(&output))
}

fn strict_utf8_sample(bytes: &[u8], source_truncated: bool) -> Option<(String, bool)> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some((text.into(), false)),
        Err(error) if error.error_len().is_none() => {
            let valid = std::str::from_utf8(bytes.get(..error.valid_up_to())?).ok()?;
            Some((valid.into(), !source_truncated))
        }
        Err(_) => None,
    }
}

fn strict_utf16_sample(
    bytes: &[u8],
    source_truncated: bool,
    charset: Charset,
) -> Option<(String, bool)> {
    let mut complete_len = bytes.len() - bytes.len() % 2;
    let mut trailing_malformed = !bytes.len().is_multiple_of(2) && !source_truncated;
    let read = |at: usize| match charset {
        Charset::Utf16Le => u16::from_le_bytes([bytes[at], bytes[at + 1]]),
        _ => u16::from_be_bytes([bytes[at], bytes[at + 1]]),
    };
    if complete_len >= 2 && (0xd800..=0xdbff).contains(&read(complete_len - 2)) {
        complete_len -= 2;
        trailing_malformed |= !source_truncated;
    }
    let complete = bytes.get(..complete_len)?;
    let decoded =
        charset.encoding().decode_without_bom_handling_and_without_replacement(complete)?;
    Some((decoded.into_owned(), trailing_malformed))
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
        unsafe_controls += usize::from(is_unsafe_auto_control(value));
        replacement += usize::from(value == '\u{fffd}');
        printable += usize::from(!is_unsafe_auto_control(value));
    }
    unsafe_controls == 0
        && replacement.saturating_mul(100) <= total.saturating_mul(MAX_UNSAFE_PERCENT)
        && printable.saturating_mul(100) >= total.saturating_mul(MIN_PRINTABLE_PERCENT)
}

fn is_unsafe_auto_control(value: char) -> bool {
    matches!(value, '\0'..='\u{8}' | '\u{b}'..='\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
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

/// Decode text once for all built-in text converters while retaining exact
/// original-byte spans for every Unicode scalar.
pub(crate) fn decode_source(
    bytes: &[u8],
    explicit_charset: Option<&str>,
    mode: TextDecodingMode,
    context: &ExecutionContext,
) -> Result<(DecodedText, Vec<Diagnostic>), ConversionError> {
    let (charset, bom_len) = select_charset(bytes, explicit_charset)?;
    decode_mapped(&bytes[bom_len..], bom_len, charset, mode, context)
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
    let (decoded, diagnostics) = decode_source(
        &input.bytes,
        options.text.charset.as_deref(),
        options.text.decoding_mode,
        context,
    )?;
    let max_decoded_text_bytes =
        size.checked_mul(3).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_text_decoded_bytes",
            detail: "decoded text byte budget overflowed".into(),
        })?;
    let document = build_document(decoded.into_lines(), max_decoded_text_bytes, context)?;
    Ok(ConverterOutput { document, diagnostics, assets: Vec::new() })
}

fn decode_mapped(
    bytes: &[u8],
    base: usize,
    charset: Charset,
    mode: TextDecodingMode,
    context: &ExecutionContext,
) -> Result<(DecodedText, Vec<Diagnostic>), ConversionError> {
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
) -> Result<(DecodedText, Vec<Diagnostic>), ConversionError> {
    let mut decoded = DecodedText::default();
    let mut recoveries = RecoveryTracker::default();
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
                    &mut recoveries,
                    base + offset,
                    base + offset + invalid_len,
                    charset,
                    mode,
                )?;
                offset += invalid_len;
            }
        }
    }
    Ok((decoded, recoveries.into_diagnostics(charset)))
}

fn push_str_units(decoded: &mut impl DecodedSink, text: &str, base: usize) {
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
) -> Result<(DecodedText, Vec<Diagnostic>), ConversionError> {
    let mut decoded = DecodedText::default();
    let mut recoveries = RecoveryTracker::default();
    let mut offset = 0;
    while offset < bytes.len() {
        if offset % 4096 == 0 {
            context.checkpoint()?;
        }
        if bytes.len() - offset < 2 {
            recover_invalid(
                &mut decoded,
                &mut recoveries,
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
                    &mut recoveries,
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
                    &mut recoveries,
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
                &mut recoveries,
                base + offset,
                base + offset + width,
                charset,
                mode,
            )?;
        }
        offset += width;
    }
    Ok((decoded, recoveries.into_diagnostics(charset)))
}

fn decode_legacy(
    bytes: &[u8],
    base: usize,
    mode: TextDecodingMode,
    charset: Charset,
    context: &ExecutionContext,
) -> Result<(DecodedText, Vec<Diagnostic>), ConversionError> {
    let mut lines_builder = DecodedText::default();
    let mut recoveries = RecoveryTracker::default();
    let mut charset_decoder = charset.encoding().new_decoder_without_bom_handling();
    let mut output = String::with_capacity(16);
    let mut offset = 0;
    let mut sequence_start = 0;
    while offset < bytes.len() {
        if offset % 4096 == 0 {
            context.checkpoint()?;
        }
        output.clear();
        let (result, read) = charset_decoder.decode_to_string_without_replacement(
            &bytes[offset..=offset],
            &mut output,
            false,
        );
        let consumed_end =
            offset.checked_add(read).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_input_bytes",
                detail: "legacy decoder input position overflowed".into(),
            })?;
        match result {
            DecoderResult::InputEmpty => {
                if !output.is_empty() {
                    push_str_range(
                        &mut lines_builder,
                        &output,
                        base + sequence_start,
                        base + consumed_end,
                    );
                    sequence_start = consumed_end;
                }
                if consumed_end <= offset {
                    return Err(ConversionError::Internal {
                        detail: format!("{} decoder made no input progress", charset.name()),
                    });
                }
                offset = consumed_end;
            }
            DecoderResult::Malformed(malformed, after) => {
                let error_end = consumed_end.checked_sub(usize::from(after)).ok_or_else(|| {
                    ConversionError::Internal {
                        detail: format!(
                            "{} decoder returned an invalid malformed range",
                            charset.name()
                        ),
                    }
                })?;
                let error_start =
                    error_end.checked_sub(usize::from(malformed)).ok_or_else(|| {
                        ConversionError::Internal {
                            detail: format!(
                                "{} decoder malformed range precedes the input",
                                charset.name()
                            ),
                        }
                    })?;
                if !output.is_empty() {
                    push_str_range(
                        &mut lines_builder,
                        &output,
                        base + sequence_start,
                        base + error_start,
                    );
                }
                recover_invalid(
                    &mut lines_builder,
                    &mut recoveries,
                    base + error_start,
                    base + error_end,
                    charset,
                    mode,
                )?;
                sequence_start = error_end;
                offset = consumed_end;
            }
            DecoderResult::OutputFull => {
                return Err(ConversionError::Internal {
                    detail: format!(
                        "{} decoder exhausted its bounded scalar buffer",
                        charset.name()
                    ),
                });
            }
        }
    }

    finish_legacy_decoder(
        &mut charset_decoder,
        &mut output,
        &mut lines_builder,
        &mut recoveries,
        bytes.len(),
        base,
        sequence_start,
        charset,
        mode,
    )?;
    Ok((lines_builder, recoveries.into_diagnostics(charset)))
}

#[allow(clippy::too_many_arguments)]
fn finish_legacy_decoder(
    decoder: &mut encoding_rs::Decoder,
    output: &mut String,
    lines_builder: &mut impl DecodedSink,
    recoveries: &mut RecoveryTracker,
    input_len: usize,
    base: usize,
    mut sequence_start: usize,
    charset: Charset,
    mode: TextDecodingMode,
) -> Result<(), ConversionError> {
    for _ in 0..8 {
        output.clear();
        let (result, read) = decoder.decode_to_string_without_replacement(b"", output, true);
        if read != 0 {
            return Err(ConversionError::Internal {
                detail: format!("{} decoder consumed bytes from empty final input", charset.name()),
            });
        }
        match result {
            DecoderResult::InputEmpty => {
                if !output.is_empty() {
                    push_str_range(lines_builder, output, base + sequence_start, base + input_len);
                }
                return Ok(());
            }
            DecoderResult::Malformed(malformed, after) => {
                let error_end = input_len.checked_sub(usize::from(after)).ok_or_else(|| {
                    ConversionError::Internal {
                        detail: format!(
                            "{} decoder returned an invalid final range",
                            charset.name()
                        ),
                    }
                })?;
                let error_start =
                    error_end.checked_sub(usize::from(malformed)).ok_or_else(|| {
                        ConversionError::Internal {
                            detail: format!(
                                "{} decoder final range precedes the input",
                                charset.name()
                            ),
                        }
                    })?;
                if !output.is_empty() {
                    push_str_range(
                        lines_builder,
                        output,
                        base + sequence_start,
                        base + error_start,
                    );
                }
                recover_invalid(
                    lines_builder,
                    recoveries,
                    base + error_start,
                    base + error_end,
                    charset,
                    mode,
                )?;
                sequence_start = error_end;
            }
            DecoderResult::OutputFull => {
                return Err(ConversionError::Internal {
                    detail: format!(
                        "{} decoder exhausted its bounded final buffer",
                        charset.name()
                    ),
                });
            }
        }
    }
    Err(ConversionError::Internal {
        detail: format!("{} decoder did not finalize after recovery", charset.name()),
    })
}

fn push_str_range(decoded: &mut impl DecodedSink, text: &str, start: usize, end: usize) {
    for value in text.chars() {
        decoded.push(value, start, end);
    }
}

fn recover_invalid(
    decoded: &mut impl DecodedSink,
    recoveries: &mut RecoveryTracker,
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
    recoveries.record(start, end)?;
    Ok(())
}

#[derive(Debug)]
struct RecoverySpan {
    start: usize,
    end: usize,
    replacements: usize,
}

#[derive(Debug, Default)]
struct RecoveryTracker {
    spans: Vec<RecoverySpan>,
}

impl RecoveryTracker {
    fn record(&mut self, start: usize, end: usize) -> Result<(), ConversionError> {
        if let Some(previous) = self.spans.last_mut()
            && previous.end == start
        {
            previous.end = end;
            previous.replacements = previous.replacements.checked_add(1).ok_or_else(|| {
                ConversionError::ResourceLimit {
                    limit: "max_document_inlines",
                    detail: "text replacement count overflowed".into(),
                }
            })?;
            return Ok(());
        }
        self.spans.push(RecoverySpan { start, end, replacements: 1 });
        Ok(())
    }

    fn into_diagnostics(self, charset: Charset) -> Vec<Diagnostic> {
        self.spans
            .into_iter()
            .map(|span| Diagnostic {
                code: INVALID_SEQUENCE_CODE.into(),
                severity: DiagnosticSeverity::Warning,
                message: format!(
                    "inserted {} U+FFFD replacement character(s) for invalid {} byte sequence(s) at {}..{}",
                    span.replacements,
                    charset.name(),
                    span.start,
                    span.end
                ),
                locator: Some(byte_locator(span.start, span.end)),
            })
            .collect()
    }
}

#[derive(Debug, Default)]
struct Line {
    text: String,
    start: Option<usize>,
    end: Option<usize>,
}

trait DecodedSink {
    fn push(&mut self, value: char, start: usize, end: usize);
}

#[derive(Debug)]
struct DecodedUnit {
    utf8_start: usize,
    utf8_end: usize,
    source_start: usize,
    source_end: usize,
}

/// Decoded UTF-8 and its monotonic mapping back to original encoded bytes.
#[derive(Debug, Default)]
pub(crate) struct DecodedText {
    pub(crate) text: String,
    units: Vec<DecodedUnit>,
}

impl DecodedSink for DecodedText {
    fn push(&mut self, value: char, start: usize, end: usize) {
        let utf8_start = self.text.len();
        self.text.push(value);
        self.units.push(DecodedUnit {
            utf8_start,
            utf8_end: self.text.len(),
            source_start: start,
            source_end: end,
        });
    }
}

impl DecodedText {
    /// Convert a half-open decoded UTF-8 range into an original-byte range.
    pub(crate) fn source_range(&self, start: usize, end: usize) -> (usize, usize) {
        let start_source = self.units.iter().find(|unit| unit.utf8_end > start).map_or_else(
            || self.units.last().map_or(0, |unit| unit.source_end),
            |unit| unit.source_start,
        );
        let end_source = if end <= start {
            start_source
        } else {
            self.units
                .iter()
                .rev()
                .find(|unit| unit.utf8_start < end)
                .map_or(start_source, |unit| unit.source_end)
        };
        (start_source, end_source)
    }

    fn into_lines(self) -> Vec<Line> {
        let mut lines = DecodedLines::default();
        for unit in self.units {
            if let Some(value) = self.text[unit.utf8_start..unit.utf8_end].chars().next() {
                DecodedSink::push(&mut lines, value, unit.source_start, unit.source_end);
            }
        }
        lines.finish()
    }
}

#[derive(Debug, Default)]
struct DecodedLines {
    lines: Vec<Line>,
    current: Line,
    pending_cr: bool,
}

impl DecodedSink for DecodedLines {
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
}

impl DecodedLines {
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
    max_decoded_text_bytes: u64,
    context: &ExecutionContext,
) -> Result<Document, ConversionError> {
    let mut document = Document::default();
    let mut paragraph: Vec<Line> = Vec::new();
    let mut stats = DocumentStats::default();
    for (index, line) in lines.into_iter().enumerate() {
        if index % 4096 == 0 {
            context.checkpoint()?;
        }
        if line.text.is_empty() {
            flush_paragraph(&mut document, &mut paragraph, &mut stats, max_decoded_text_bytes)?;
        } else {
            paragraph.push(line);
        }
    }
    flush_paragraph(&mut document, &mut paragraph, &mut stats, max_decoded_text_bytes)?;
    Ok(document)
}

#[derive(Debug, Default)]
struct DocumentStats {
    blocks: usize,
    inlines: usize,
    text_bytes: u64,
}

fn flush_paragraph(
    document: &mut Document,
    lines: &mut Vec<Line>,
    stats: &mut DocumentStats,
    max_decoded_text_bytes: u64,
) -> Result<(), ConversionError> {
    if lines.is_empty() {
        return Ok(());
    }
    let blocks = stats.blocks.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
        limit: "max_document_nodes",
        detail: "plain-text block count overflowed".into(),
    })?;
    if blocks > MAX_DOCUMENT_NODES {
        return Err(ConversionError::ResourceLimit {
            limit: "max_document_nodes",
            detail: format!("plain text exceeds {MAX_DOCUMENT_NODES} paragraph nodes"),
        });
    }
    let new_inlines =
        lines.len().checked_mul(2).and_then(|count| count.checked_sub(1)).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_document_inlines",
                detail: "plain-text inline count overflowed".into(),
            }
        })?;
    let inlines =
        stats.inlines.checked_add(new_inlines).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_document_inlines",
            detail: "plain-text inline count overflowed".into(),
        })?;
    if inlines > MAX_DOCUMENT_INLINES {
        return Err(ConversionError::ResourceLimit {
            limit: "max_document_inlines",
            detail: format!("plain text exceeds {MAX_DOCUMENT_INLINES} inline nodes"),
        });
    }
    let added_text_bytes = lines.iter().try_fold(0_u64, |total, line| {
        let bytes = u64::try_from(line.text.len()).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_text_decoded_bytes",
            detail: "decoded line length cannot be represented as u64".into(),
        })?;
        total.checked_add(bytes).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_text_decoded_bytes",
            detail: "decoded text byte count overflowed".into(),
        })
    })?;
    let text_bytes = stats.text_bytes.checked_add(added_text_bytes).ok_or_else(|| {
        ConversionError::ResourceLimit {
            limit: "max_text_decoded_bytes",
            detail: "decoded text byte count overflowed".into(),
        }
    })?;
    if text_bytes > max_decoded_text_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_text_decoded_bytes",
            detail: format!("{text_bytes} > {max_decoded_text_bytes}"),
        });
    }
    stats.blocks = blocks;
    stats.inlines = inlines;
    stats.text_bytes = text_bytes;
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

    fn sniff(bytes: &[u8]) -> Option<f32> {
        sniff_text(bytes, &context()).unwrap()
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
        assert_eq!(sniff(b"MZ\0\x01\x02payload"), None);
    }

    #[test]
    fn full_input_unicode_control_policy_covers_utf8_legacy_and_safe_text() {
        let mut safe = vec![b'A'; TEXT_SNIFF_BYTE_LIMIT + 4096];
        safe.extend_from_slice(" 安全文本\tline\r\n".as_bytes());
        assert_eq!(sniff(&safe), Some(0.88));

        for suffix in [b"\x7f".as_slice(), b"\xc2\x80".as_slice(), b"\xc2\x9f".as_slice()] {
            let mut unsafe_text = vec![b'A'; TEXT_SNIFF_BYTE_LIMIT + 4096];
            unsafe_text.extend_from_slice(suffix);
            assert_eq!(sniff(&unsafe_text), None);
            assert!(!decoded_input_safe(&unsafe_text, Charset::Utf8, &context()).unwrap());
        }

        let (legacy_control, _, had_errors) = SHIFT_JIS.encode("\u{80}");
        assert!(!had_errors);
        let mut legacy = Charset::ShiftJis
            .encoding()
            .encode("日本語の文字コード判定に十分な長さの文章です。")
            .0
            .into_owned();
        legacy.extend(std::iter::repeat_n(b'A', TEXT_SNIFF_BYTE_LIMIT + 4096));
        legacy.extend_from_slice(&legacy_control);
        assert!(!decoded_input_safe(&legacy, Charset::ShiftJis, &context()).unwrap());
        assert_eq!(sniff(&legacy), None);
    }

    #[test]
    fn bom_probe_decodes_safe_sample_and_rejects_binary_masquerades() {
        assert_eq!(sniff(&[0xff, 0xfe, b'A']), Some(0.80));
        assert!(sniff(&[0xff, 0xfe, b'A', 0]).is_some_and(|value| value < 0.99));
        assert_eq!(sniff(&[0xef, 0xbb, 0xbf, b'M', b'Z', 0, 1, 2]), None);
        assert_eq!(sniff(&[0xff, 0xfe, 0, 0, 1, 0, 2, 0]), None);
        assert_eq!(sniff(&[0xff, 0xfe, 0x00, 0xdc, b'A', 0]), None);

        let mut sparse_control = vec![0xef, 0xbb, 0xbf];
        sparse_control.extend(std::iter::repeat_n(b'A', TEXT_SNIFF_BYTE_LIMIT + 1024));
        sparse_control.push(0x01);
        assert_eq!(sniff(&sparse_control), None);
        assert_eq!(sniff(&[0xef, 0xbb, 0xbf, b'A', 0x0c, b'B']), None);

        let error =
            convert_text(&input(&[0xff, 0xfe, b'A']), &ConversionOptions::default(), &context())
                .unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Malformed);
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
            assert!(sniff(&encoded).is_some(), "{} must be auto-detectable", charset.name());
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
    fn legacy_decoder_preserves_ascii_after_exact_invalid_ranges() {
        let cases = [
            ("shift_jis", &[0x82, 0x20, b'A'][..], "� A", 0_u64, 1_u64),
            ("big5", &[0x81, 0x20, b'B'][..], "� B", 0, 1),
            ("gb18030", &[0x81, 0x20, b'C'][..], "� C", 0, 1),
            ("gb18030", &[0x81, 0x30, 0x81, b'D'][..], "�0丏", 0, 1),
        ];
        for (charset, bytes, expected, invalid_start, invalid_end) in cases {
            let mut options = ConversionOptions::default();
            options.text.charset = Some(charset.into());
            let strict = convert_text(&input(bytes), &options, &context()).unwrap_err();
            assert!(strict.to_string().contains(&format!("{invalid_start}..{invalid_end}")));
            options.text.decoding_mode = TextDecodingMode::Replace;
            let output = convert_text(&input(bytes), &options, &context()).unwrap();
            let Block::Paragraph(content) = &output.document.blocks[0].block else {
                unreachable!();
            };
            assert_eq!(content[0], Inline::Text { value: expected.into(), marks: Vec::new() });
            assert_eq!(
                output.diagnostics[0].locator.as_ref().unwrap().byte_start,
                Some(invalid_start)
            );
            assert_eq!(output.diagnostics[0].locator.as_ref().unwrap().byte_end, Some(invalid_end));
        }
    }

    #[test]
    fn fixed_multibyte_boundaries_decode_without_replacement() {
        let cases = [
            ("shift_jis", &[0x82, 0xa0][..], "あ"),
            ("big5", &[0xa4, 0xa4][..], "中"),
            ("gb18030", &[0xd6, 0xd0][..], "中"),
            ("gb18030", &[0x94, 0x39, 0xfc, 0x36][..], "😀"),
        ];
        for (charset, bytes, expected) in cases {
            let mut options = ConversionOptions::default();
            options.text.charset = Some(charset.into());
            let output = convert_text(&input(bytes), &options, &context()).unwrap();
            assert!(output.diagnostics.is_empty());
            let Block::Paragraph(content) = &output.document.blocks[0].block else {
                unreachable!();
            };
            assert_eq!(content[0], Inline::Text { value: expected.into(), marks: Vec::new() });
            assert_eq!(
                output.document.blocks[0].provenance.locator.byte_end,
                Some(bytes.len() as u64)
            );
        }
    }

    #[test]
    fn adjacent_invalid_sequences_merge_diagnostic_but_keep_decoder_replacements() {
        let mut options = ConversionOptions::default();
        options.text.charset = Some("utf-8".into());
        options.text.decoding_mode = TextDecodingMode::Replace;
        let output = convert_text(&input(&[0xff, 0xfd, b'A']), &options, &context()).unwrap();
        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(output.diagnostics[0].locator.as_ref().unwrap().byte_start, Some(0));
        assert_eq!(output.diagnostics[0].locator.as_ref().unwrap().byte_end, Some(2));
        assert!(output.diagnostics[0].message.contains("2 U+FFFD"));
        let Block::Paragraph(content) = &output.document.blocks[0].block else {
            unreachable!();
        };
        assert_eq!(content[0], Inline::Text { value: "��A".into(), marks: Vec::new() });

        options.text.charset = Some("big5".into());
        let output = convert_text(&input(&[0x81, 0x81, 0x81, 0x30]), &options, &context()).unwrap();
        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(output.diagnostics[0].locator.as_ref().unwrap().byte_start, Some(0));
        assert_eq!(output.diagnostics[0].locator.as_ref().unwrap().byte_end, Some(3));
        assert!(output.diagnostics[0].message.contains("2 U+FFFD"));
        let Block::Paragraph(content) = &output.document.blocks[0].block else {
            unreachable!();
        };
        assert_eq!(content[0], Inline::Text { value: "��0".into(), marks: Vec::new() });
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
    fn excessive_soft_lines_fail_as_resource_limit_before_invalid_ir() {
        let source = "x\n".repeat(500_001);
        let error =
            convert_text(&input(source.as_bytes()), &ConversionOptions::default(), &context())
                .unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
        assert!(error.to_string().contains("max_document_inlines"));
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

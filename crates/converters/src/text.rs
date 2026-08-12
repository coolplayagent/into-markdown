use chardetng::EncodingDetector;
use encoding_rs::{
    BIG5, DecoderResult, EncoderResult, Encoding, GB18030, GBK, SHIFT_JIS, UTF_8, UTF_16BE,
    UTF_16LE, WINDOWS_1252,
};
use into_markdown_core::{
    Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput,
    Diagnostic, DiagnosticSeverity, Document, ExecutionContext, FormatCandidate, Inline,
    InputFormat, MAX_DOCUMENT_INLINES, MAX_DOCUMENT_NODES, NodeId, ProbeOutcome, Provenance,
    ProvenanceKind, ResolvedInput, ResourceReservation, Services, SourceLocator, TextDecodingMode,
};
use std::mem::size_of;

const TEXT_FORMATS: &[InputFormat] = &[InputFormat::Text];
const PROVIDER_ID: &str = "builtin.converter.text";
const INVALID_SEQUENCE_CODE: &str = "text.invalidByteSequenceReplaced";
const MAX_UNSAFE_PERCENT: usize = 1;
const MIN_PRINTABLE_PERCENT: usize = 95;
const TEXT_SNIFF_BYTE_LIMIT: usize = 64 * 1024;
const SAFETY_DECODE_INPUT_CHUNK: usize = 4096;

#[cfg(test)]
thread_local! {
    static SAMPLE_DECODE_INVOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

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
    let label = label.trim();
    let charset = if charset_label_eq(label, "utf-8") || charset_label_eq(label, "utf8") {
        Charset::Utf8
    } else if charset_label_eq(label, "utf-16le") || charset_label_eq(label, "utf16le") {
        Charset::Utf16Le
    } else if charset_label_eq(label, "utf-16be") || charset_label_eq(label, "utf16be") {
        Charset::Utf16Be
    } else if charset_label_eq(label, "windows-1252")
        || charset_label_eq(label, "windows1252")
        || charset_label_eq(label, "cp1252")
    {
        Charset::Windows1252
    } else if charset_label_eq(label, "gb18030") || charset_label_eq(label, "gb-18030") {
        Charset::Gb18030
    } else if charset_label_eq(label, "big5") || charset_label_eq(label, "big-5") {
        Charset::Big5
    } else if charset_label_eq(label, "shift-jis")
        || charset_label_eq(label, "shiftjis")
        || charset_label_eq(label, "sjis")
        || charset_label_eq(label, "cp932")
        || charset_label_eq(label, "windows-31j")
    {
        Charset::ShiftJis
    } else {
        return Err(ConversionError::Malformed {
            part: Some("charset".into()),
            detail: format!("unsupported character encoding label: {label}"),
        });
    };
    Ok(charset)
}

fn charset_label_eq(label: &str, expected: &str) -> bool {
    label
        .bytes()
        .map(|byte| match byte {
            b'_' | b' ' => b'-',
            _ => byte.to_ascii_lowercase(),
        })
        .eq(expected.bytes())
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
    let mut memory = LogicalMemory::new(context)?;
    if let Some((charset, bom_len)) = bom(bytes) {
        return sniff_bom_text(
            bytes.get(bom_len..).ok_or_else(|| ConversionError::Internal {
                detail: "BOM offset exceeds text input".into(),
            })?,
            charset,
            context,
            &mut memory,
        );
    }
    let sample_len = bytes.len().min(TEXT_SNIFF_BYTE_LIMIT);
    let sample = bytes.get(..sample_len).ok_or_else(|| ConversionError::Internal {
        detail: "bounded text sample exceeds input".into(),
    })?;
    if let Ok(text) = std::str::from_utf8(sample)
        && decoded_text_safe(text)
    {
        return Ok(decoded_input_safe(bytes, Charset::Utf8, context, &mut memory)?.then_some(0.88));
    }
    let Some(charset) = detect_legacy(sample, context)? else {
        return Ok(None);
    };
    let sample_memory = memory.mark();
    let Some(text) = decode_sample(sample, charset, &mut memory)? else {
        return Ok(None);
    };
    let sample_safe = decoded_text_safe(&text) && legacy_text_roundtrips(charset, sample, &text);
    drop(text);
    memory.rewind(sample_memory)?;
    if !sample_safe {
        return Ok(None);
    }
    Ok(decoded_input_safe(bytes, charset, context, &mut memory)?.then_some(0.72))
}

fn sniff_bom_text(
    bytes: &[u8],
    charset: Charset,
    context: &ExecutionContext,
    memory: &mut LogicalMemory,
) -> Result<Option<f32>, ConversionError> {
    let sample_len = bytes.len().min(TEXT_SNIFF_BYTE_LIMIT);
    let sample = bytes.get(..sample_len).ok_or_else(|| ConversionError::Internal {
        detail: "bounded BOM text sample exceeds input".into(),
    })?;
    let source_truncated = bytes.len() > sample_len;
    let sample_memory = memory.mark();
    let (decoded, trailing_malformed) = match charset {
        Charset::Utf8 => {
            let Some(decoded) = strict_utf8_sample(sample, source_truncated) else {
                return Ok(None);
            };
            (std::borrow::Cow::Borrowed(decoded.0), decoded.1)
        }
        Charset::Utf16Le | Charset::Utf16Be => {
            let Some(decoded) = strict_utf16_sample(sample, source_truncated, charset, memory)?
            else {
                return Ok(None);
            };
            (std::borrow::Cow::Owned(decoded.0), decoded.1)
        }
        _ => return Ok(None),
    };
    let sample_safe = decoded_text_safe(&decoded);
    drop(decoded);
    memory.rewind(sample_memory)?;
    if !sample_safe || !decoded_input_safe(bytes, charset, context, memory)? {
        return Ok(None);
    }
    Ok(Some(if trailing_malformed { 0.80 } else { 0.95 }))
}

fn decoded_input_safe(
    bytes: &[u8],
    charset: Charset,
    context: &ExecutionContext,
    memory: &mut LogicalMemory,
) -> Result<bool, ConversionError> {
    let mut decoder = charset.encoding().new_decoder_without_bom_handling();
    let capacity = decoder
        .max_utf8_buffer_length_without_replacement(SAFETY_DECODE_INPUT_CHUNK)
        .ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "text safety decoder output capacity overflowed".into(),
        })?;
    let mut output = String::new();
    memory.reserve_string(&mut output, capacity)?;
    let mut offset = 0_usize;
    while offset < bytes.len() {
        context.checkpoint()?;
        output.clear();
        let end = offset.saturating_add(SAFETY_DECODE_INPUT_CHUNK).min(bytes.len());
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

fn strict_utf8_sample(bytes: &[u8], source_truncated: bool) -> Option<(&str, bool)> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some((text, false)),
        Err(error) if error.error_len().is_none() => {
            let valid = std::str::from_utf8(bytes.get(..error.valid_up_to())?).ok()?;
            Some((valid, !source_truncated))
        }
        Err(_) => None,
    }
}

fn strict_utf16_sample(
    bytes: &[u8],
    source_truncated: bool,
    charset: Charset,
    memory: &mut LogicalMemory,
) -> Result<Option<(String, bool)>, ConversionError> {
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
    let Some(complete) = bytes.get(..complete_len) else { return Ok(None) };
    Ok(decode_sample(complete, charset, memory)?.map(|decoded| (decoded, trailing_malformed)))
}

fn decode_sample(
    bytes: &[u8],
    charset: Charset,
    memory: &mut LogicalMemory,
) -> Result<Option<String>, ConversionError> {
    let mut decoder = charset.encoding().new_decoder_without_bom_handling();
    let capacity =
        decoder.max_utf8_buffer_length_without_replacement(bytes.len()).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "bounded text sample output capacity overflowed".into(),
            }
        })?;
    let mut output = String::new();
    memory.reserve_string(&mut output, capacity)?;
    #[cfg(test)]
    SAMPLE_DECODE_INVOCATIONS.with(|count| count.set(count.get() + 1));
    let (result, read) = decoder.decode_to_string_without_replacement(bytes, &mut output, true);
    if read != bytes.len() || !matches!(result, DecoderResult::InputEmpty) {
        return Ok(None);
    }
    Ok(Some(output))
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

fn legacy_text_roundtrips(charset: Charset, bytes: &[u8], text: &str) -> bool {
    let mut encoder = charset.encoding().new_encoder();
    let mut text_offset = 0_usize;
    let mut byte_offset = 0_usize;
    let mut output = [0; SAFETY_DECODE_INPUT_CHUNK];
    loop {
        let (result, read, written) =
            encoder.encode_from_utf8_without_replacement(&text[text_offset..], &mut output, true);
        let Some(expected) = bytes.get(byte_offset..byte_offset.saturating_add(written)) else {
            return false;
        };
        if output[..written] != *expected {
            return false;
        }
        text_offset = match text_offset.checked_add(read) {
            Some(value) => value,
            None => return false,
        };
        byte_offset = match byte_offset.checked_add(written) {
            Some(value) => value,
            None => return false,
        };
        match result {
            EncoderResult::InputEmpty => {
                return text_offset == text.len() && byte_offset == bytes.len();
            }
            EncoderResult::OutputFull if read != 0 || written != 0 => {}
            EncoderResult::OutputFull | EncoderResult::Unmappable(_) => return false,
        }
    }
}

fn legacy_input_safe_roundtrip(
    bytes: &[u8],
    charset: Charset,
    context: &ExecutionContext,
    memory: &mut LogicalMemory,
) -> Result<bool, ConversionError> {
    let mark = memory.mark();
    let mut decoder = charset.encoding().new_decoder_without_bom_handling();
    let capacity = decoder
        .max_utf8_buffer_length_without_replacement(SAFETY_DECODE_INPUT_CHUNK)
        .ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "legacy validation output capacity overflowed".into(),
        })?;
    let mut output = String::new();
    memory.reserve_string(&mut output, capacity)?;
    let mut encoder = charset.encoding().new_encoder();
    let mut source_offset = 0;
    let mut roundtrip_offset = 0;
    let mut valid = true;
    while source_offset < bytes.len() && valid {
        context.checkpoint()?;
        output.clear();
        let end = source_offset.saturating_add(SAFETY_DECODE_INPUT_CHUNK).min(bytes.len());
        let (result, read) = decoder.decode_to_string_without_replacement(
            &bytes[source_offset..end],
            &mut output,
            false,
        );
        valid = !matches!(result, DecoderResult::Malformed(_, _))
            && decoded_text_safe(&output)
            && encode_chunk_matches(&mut encoder, &output, false, bytes, &mut roundtrip_offset);
        if read == 0 && end > source_offset {
            valid = false;
            break;
        }
        source_offset =
            source_offset.checked_add(read).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_input_bytes",
                detail: "legacy validation input position overflowed".into(),
            })?;
    }
    if valid {
        output.clear();
        let (result, read) = decoder.decode_to_string_without_replacement(b"", &mut output, true);
        valid = read == 0
            && result == DecoderResult::InputEmpty
            && decoded_text_safe(&output)
            && encode_chunk_matches(&mut encoder, &output, true, bytes, &mut roundtrip_offset)
            && roundtrip_offset == bytes.len();
    }
    drop(output);
    memory.rewind(mark)?;
    Ok(valid)
}

fn encode_chunk_matches(
    encoder: &mut encoding_rs::Encoder,
    text: &str,
    last: bool,
    source: &[u8],
    source_offset: &mut usize,
) -> bool {
    let mut input_offset = 0;
    let mut output = [0; SAFETY_DECODE_INPUT_CHUNK];
    loop {
        let (result, read, written) =
            encoder.encode_from_utf8_without_replacement(&text[input_offset..], &mut output, last);
        let Some(expected) = source.get(*source_offset..source_offset.saturating_add(written))
        else {
            return false;
        };
        if output[..written] != *expected {
            return false;
        }
        let Some(next_input) = input_offset.checked_add(read) else { return false };
        let Some(next_source) = source_offset.checked_add(written) else { return false };
        input_offset = next_input;
        *source_offset = next_source;
        match result {
            EncoderResult::InputEmpty => return input_offset == text.len(),
            EncoderResult::OutputFull if read != 0 || written != 0 => {}
            EncoderResult::OutputFull | EncoderResult::Unmappable(_) => return false,
        }
    }
}

fn detect_legacy(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Option<Charset>, ConversionError> {
    let mut detector = EncodingDetector::new();
    if bytes.is_empty() {
        detector.feed(bytes, true);
    } else {
        let mut chunks = bytes.chunks(SAFETY_DECODE_INPUT_CHUNK).peekable();
        while let Some(chunk) = chunks.next() {
            context.checkpoint()?;
            detector.feed(chunk, chunks.peek().is_none());
        }
    }
    let guessed = detector.guess(None, true);
    Ok(if guessed == WINDOWS_1252 {
        Some(Charset::Windows1252)
    } else if guessed == GB18030 || guessed == GBK {
        Some(Charset::Gb18030)
    } else if guessed == BIG5 {
        Some(Charset::Big5)
    } else if guessed == SHIFT_JIS {
        Some(Charset::ShiftJis)
    } else {
        None
    })
}

fn select_charset(
    bytes: &[u8],
    explicit: Option<&str>,
    context: &ExecutionContext,
    memory: &mut LogicalMemory,
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
    let charset = detect_legacy(bytes, context)?.ok_or_else(|| ConversionError::Malformed {
        part: Some("charset".into()),
        detail: "character encoding could not be detected with sufficient confidence".into(),
    })?;
    if !legacy_input_safe_roundtrip(bytes, charset, context, memory)? {
        return Err(ConversionError::Malformed {
            part: Some("charset".into()),
            detail:
                "detected encoding did not pass replacement, printable, and round-trip thresholds"
                    .into(),
        });
    }
    Ok((charset, 0))
}

/// Decode text once for all built-in text converters while retaining the exact
/// original-byte span for every decoder output sequence.
pub(crate) fn decode_source(
    bytes: &[u8],
    explicit_charset: Option<&str>,
    mode: TextDecodingMode,
    context: &ExecutionContext,
) -> Result<(DecodedText, Vec<Diagnostic>), ConversionError> {
    let mut memory = LogicalMemory::new(context)?;
    let (charset, bom_len) = select_charset(bytes, explicit_charset, context, &mut memory)?;
    decode_mapped(&bytes[bom_len..], bom_len, charset, mode, context, memory)
}

/// Logical heap-capacity accounting shared by text-family converters.
///
/// The accounting covers requested `String` bytes and `Vec<T>` element slots.
/// Allocator bookkeeping and platform-specific size-class slack are excluded;
/// every logical capacity increase is charged before its allocation request.
pub(crate) struct LogicalMemory {
    reservation: ResourceReservation,
    charged: usize,
}

impl LogicalMemory {
    pub(crate) fn new(context: &ExecutionContext) -> Result<Self, ConversionError> {
        Ok(Self { reservation: context.reserve_memory(0)?, charged: 0 })
    }

    pub(crate) fn charge(&mut self, bytes: usize) -> Result<(), ConversionError> {
        let bytes_u64 = u64::try_from(bytes).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "logical heap capacity cannot be represented as u64".into(),
        })?;
        let next =
            self.charged.checked_add(bytes).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "logical heap capacity overflowed".into(),
            })?;
        self.reservation.grow(bytes_u64)?;
        self.charged = next;
        Ok(())
    }

    fn mark(&self) -> usize {
        self.charged
    }

    fn rewind(&mut self, mark: usize) -> Result<(), ConversionError> {
        let released = self.charged.checked_sub(mark).ok_or_else(|| ConversionError::Internal {
            detail: "logical memory mark exceeds current charge".into(),
        })?;
        self.reservation.shrink(u64::try_from(released).map_err(|_| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "logical memory release cannot be represented as u64".into(),
            }
        })?)?;
        self.charged = mark;
        Ok(())
    }

    pub(crate) fn reserve_vec<T>(
        &mut self,
        vector: &mut Vec<T>,
        additional: usize,
    ) -> Result<(), ConversionError> {
        let required =
            vector.len().checked_add(additional).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "logical vector capacity overflowed".into(),
            })?;
        if required <= vector.capacity() {
            return Ok(());
        }
        let target = required.max(vector.capacity().saturating_mul(2)).max(4);
        let new_slots = target.checked_sub(vector.capacity()).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "logical vector capacity underflowed".into(),
            }
        })?;
        self.charge(new_slots.checked_mul(size_of::<T>()).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "logical vector byte capacity overflowed".into(),
            }
        })?)?;
        vector.try_reserve_exact(target - vector.len()).map_err(|error| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: format!("logical vector allocation failed: {error}"),
            }
        })
    }

    pub(crate) fn reserve_string(
        &mut self,
        string: &mut String,
        additional: usize,
    ) -> Result<(), ConversionError> {
        let required =
            string.len().checked_add(additional).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "logical string capacity overflowed".into(),
            })?;
        if required <= string.capacity() {
            return Ok(());
        }
        let target = required.max(string.capacity().saturating_mul(2)).max(64);
        self.charge(target - string.capacity())?;
        string.try_reserve_exact(target - string.len()).map_err(|error| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: format!("logical string allocation failed: {error}"),
            }
        })
    }
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
    let mut lines = decoded.into_lines(context)?;
    let document = build_document(
        std::mem::take(&mut lines.lines),
        max_decoded_text_bytes,
        context,
        &mut lines.memory,
    )?;
    Ok(ConverterOutput { document, diagnostics, assets: Vec::new() })
}

fn decode_mapped(
    bytes: &[u8],
    base: usize,
    charset: Charset,
    mode: TextDecodingMode,
    context: &ExecutionContext,
    memory: LogicalMemory,
) -> Result<(DecodedText, Vec<Diagnostic>), ConversionError> {
    match charset {
        Charset::Utf8 => decode_utf8(bytes, base, mode, charset, context, memory),
        Charset::Utf16Le | Charset::Utf16Be => {
            decode_utf16(bytes, base, mode, charset, context, memory)
        }
        _ => decode_legacy(bytes, base, mode, charset, context, memory),
    }
}

fn decode_utf8(
    bytes: &[u8],
    base: usize,
    mode: TextDecodingMode,
    charset: Charset,
    context: &ExecutionContext,
    memory: LogicalMemory,
) -> Result<(DecodedText, Vec<Diagnostic>), ConversionError> {
    let mut decoded = DecodedText::new(base, memory);
    let mut recoveries = RecoveryTracker::default();
    let mut offset = 0;
    while offset < bytes.len() {
        if offset % 4096 == 0 {
            context.checkpoint()?;
        }
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(valid) => {
                push_str_units(&mut decoded, valid, base + offset)?;
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
                push_str_units(&mut decoded, valid, base + offset)?;
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
    let diagnostics = recoveries.into_diagnostics(charset, &mut decoded.memory)?;
    Ok((decoded, diagnostics))
}

fn push_str_units(
    decoded: &mut impl DecodedSink,
    text: &str,
    base: usize,
) -> Result<(), ConversionError> {
    for (offset, value) in text.char_indices() {
        push_scalar(decoded, value, base + offset, base + offset + value.len_utf8())?;
    }
    Ok(())
}

fn decode_utf16(
    bytes: &[u8],
    base: usize,
    mode: TextDecodingMode,
    charset: Charset,
    context: &ExecutionContext,
    memory: LogicalMemory,
) -> Result<(DecodedText, Vec<Diagnostic>), ConversionError> {
    let mut decoded = DecodedText::new(base, memory);
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
            push_scalar(&mut decoded, value, base + offset, base + offset + width)?;
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
    let diagnostics = recoveries.into_diagnostics(charset, &mut decoded.memory)?;
    Ok((decoded, diagnostics))
}

fn decode_legacy(
    bytes: &[u8],
    base: usize,
    mode: TextDecodingMode,
    charset: Charset,
    context: &ExecutionContext,
    memory: LogicalMemory,
) -> Result<(DecodedText, Vec<Diagnostic>), ConversionError> {
    let mut lines_builder = DecodedText::new(base, memory);
    let mut recoveries = RecoveryTracker::default();
    let mut charset_decoder = charset.encoding().new_decoder_without_bom_handling();
    lines_builder.memory.charge(16)?;
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
                    )?;
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
                    )?;
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
    let diagnostics = recoveries.into_diagnostics(charset, &mut lines_builder.memory)?;
    Ok((lines_builder, diagnostics))
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
                    push_str_range(lines_builder, output, base + sequence_start, base + input_len)?;
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
                    )?;
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

fn push_str_range(
    decoded: &mut impl DecodedSink,
    text: &str,
    start: usize,
    end: usize,
) -> Result<(), ConversionError> {
    decoded.push_sequence(text, start, end)
}

fn push_scalar(
    decoded: &mut impl DecodedSink,
    value: char,
    start: usize,
    end: usize,
) -> Result<(), ConversionError> {
    let mut buffer = [0; 4];
    decoded.push_sequence(value.encode_utf8(&mut buffer), start, end)
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
    push_scalar(decoded, '\u{fffd}', start, end)?;
    recoveries.record(start, end, decoded.memory())?;
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
    fn record(
        &mut self,
        start: usize,
        end: usize,
        memory: &mut LogicalMemory,
    ) -> Result<(), ConversionError> {
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
        memory.reserve_vec(&mut self.spans, 1)?;
        self.spans.push(RecoverySpan { start, end, replacements: 1 });
        Ok(())
    }

    fn into_diagnostics(
        self,
        charset: Charset,
        memory: &mut LogicalMemory,
    ) -> Result<Vec<Diagnostic>, ConversionError> {
        let mut diagnostics = Vec::new();
        memory.reserve_vec(&mut diagnostics, self.spans.len())?;
        for span in self.spans {
            // Decimal offsets/counts and the longest supported charset name
            // fit this fixed logical message allowance.
            memory.charge(256 + INVALID_SEQUENCE_CODE.len())?;
            diagnostics.push(Diagnostic {
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
            });
        }
        Ok(diagnostics)
    }
}

#[derive(Debug, Default)]
struct Line {
    text: String,
    start: Option<usize>,
    end: Option<usize>,
}

trait DecodedSink {
    fn push_sequence(
        &mut self,
        value: &str,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError>;
    fn memory(&mut self) -> &mut LogicalMemory;
}

#[derive(Debug, Clone, Copy)]
struct MapRun {
    decoded_start: u64,
    source_start: u64,
    units: u32,
    decoded_width: u32,
    source_width: u32,
}

impl MapRun {
    fn decoded_end(self) -> u64 {
        self.decoded_start + u64::from(self.units) * u64::from(self.decoded_width)
    }

    fn source_end(self) -> u64 {
        self.source_start + u64::from(self.units) * u64::from(self.source_width)
    }
}

/// Decoded UTF-8 and its monotonic mapping back to original encoded bytes.
pub(crate) struct DecodedText {
    pub(crate) text: String,
    runs: Vec<MapRun>,
    initial_source: usize,
    pub(crate) memory: LogicalMemory,
}

impl DecodedSink for DecodedText {
    fn push_sequence(
        &mut self,
        value: &str,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        if value.is_empty() {
            return Ok(());
        }
        let decoded_start =
            u64::try_from(self.text.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "decoded UTF-8 offset cannot be represented as u64".into(),
            })?;
        let decoded_width =
            u32::try_from(value.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "decoded scalar width cannot be represented as u32".into(),
            })?;
        let source_start = u64::try_from(start).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: "source offset cannot be represented as u64".into(),
        })?;
        let source_width = u32::try_from(end.saturating_sub(start)).map_err(|_| {
            ConversionError::ResourceLimit {
                limit: "max_input_bytes",
                detail: "source scalar width cannot be represented as u32".into(),
            }
        })?;
        self.memory.reserve_string(&mut self.text, value.len())?;
        let merge = self.runs.last().is_some_and(|run| {
            run.decoded_end() == decoded_start
                && run.source_end() == source_start
                && run.decoded_width == decoded_width
                && run.source_width == source_width
                && run.units < u32::MAX
        });
        if !merge {
            self.memory.reserve_vec(&mut self.runs, 1)?;
        }
        self.text.push_str(value);
        if merge {
            if let Some(run) = self.runs.last_mut() {
                run.units += 1;
            }
        } else {
            self.runs.push(MapRun {
                decoded_start,
                source_start,
                units: 1,
                decoded_width,
                source_width,
            });
        }
        Ok(())
    }

    fn memory(&mut self) -> &mut LogicalMemory {
        &mut self.memory
    }
}

impl DecodedText {
    fn new(initial_source: usize, memory: LogicalMemory) -> Self {
        Self { text: String::new(), runs: Vec::new(), initial_source, memory }
    }

    /// Convert a half-open decoded UTF-8 range into an original-byte range.
    pub(crate) fn source_range(&self, start: usize, end: usize) -> (usize, usize) {
        self.mapping().source_range(start, end)
    }

    pub(crate) fn mapping_and_memory(&mut self) -> (DecodedMapping<'_>, &mut LogicalMemory) {
        (DecodedMapping { runs: &self.runs, initial_source: self.initial_source }, &mut self.memory)
    }

    fn mapping(&self) -> DecodedMapping<'_> {
        DecodedMapping { runs: &self.runs, initial_source: self.initial_source }
    }

    #[cfg(test)]
    fn source_range_lookup_count(&self, start: usize, end: usize) -> usize {
        self.mapping().source_range_lookup_count(start, end)
    }

    fn into_lines(self, context: &ExecutionContext) -> Result<DecodedLineOutput, ConversionError> {
        let Self { text, runs, initial_source, memory } = self;
        let mut lines = DecodedLines::new(memory);
        let mut run_index = 0;
        for (decoded_start, value) in text.char_indices() {
            if decoded_start.is_multiple_of(4096) {
                context.checkpoint()?;
            }
            while runs.get(run_index).is_some_and(|run| {
                u64::try_from(decoded_start).unwrap_or(u64::MAX) >= run.decoded_end()
            }) {
                run_index += 1;
            }
            let Some(run) = runs.get(run_index).copied() else {
                return Err(ConversionError::Internal {
                    detail: "decoded text mapping ended before its text".into(),
                });
            };
            let within =
                u64::try_from(decoded_start).unwrap_or(u64::MAX).saturating_sub(run.decoded_start);
            let unit = within / u64::from(run.decoded_width);
            let source_start = run.source_start + unit * u64::from(run.source_width);
            let source_end = source_start + u64::from(run.source_width);
            let mut buffer = [0; 4];
            lines.push_sequence(
                value.encode_utf8(&mut buffer),
                usize::try_from(source_start).unwrap_or(initial_source),
                usize::try_from(source_end).unwrap_or(initial_source),
            )?;
        }
        lines.finish()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DecodedMapping<'a> {
    runs: &'a [MapRun],
    initial_source: usize,
}

impl DecodedMapping<'_> {
    pub(crate) fn source_range(self, start: usize, end: usize) -> (usize, usize) {
        if end <= start {
            let (source, _) = self.source_boundary(start);
            return (source, source);
        }
        let (start_source, start_lookups) = self.source_start_for_overlap(start);
        let (end_source, end_lookups) = self.source_end_for_overlap(end);
        let _ = start_lookups + end_lookups;
        (start_source, end_source.max(start_source))
    }

    fn source_boundary(self, decoded: usize) -> (usize, usize) {
        if self.runs.is_empty() {
            return (self.initial_source, 0);
        }
        let decoded = u64::try_from(decoded).unwrap_or(u64::MAX);
        let mut low = 0;
        let mut high = self.runs.len();
        let mut comparisons = 0;
        while low < high {
            comparisons += 1;
            let middle = low + (high - low) / 2;
            if self.runs[middle].decoded_start <= decoded {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if low == 0 {
            return (self.initial_source, comparisons);
        }
        let run = self.runs[low - 1];
        let within = decoded
            .saturating_sub(run.decoded_start)
            .min(u64::from(run.units) * u64::from(run.decoded_width));
        let units = within / u64::from(run.decoded_width);
        let source = run.source_start + units * u64::from(run.source_width);
        (usize::try_from(source).unwrap_or(usize::MAX), comparisons)
    }

    fn source_start_for_overlap(self, decoded: usize) -> (usize, usize) {
        if self.runs.is_empty() {
            return (self.initial_source, 0);
        }
        let decoded = u64::try_from(decoded).unwrap_or(u64::MAX);
        let mut low = 0;
        let mut high = self.runs.len();
        let mut comparisons = 0;
        while low < high {
            comparisons += 1;
            let middle = low + (high - low) / 2;
            if self.runs[middle].decoded_start <= decoded {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let run = self.runs[low.saturating_sub(1)];
        let within = decoded
            .saturating_sub(run.decoded_start)
            .min(u64::from(run.units.saturating_sub(1)) * u64::from(run.decoded_width));
        let unit = within / u64::from(run.decoded_width);
        let source = run.source_start + unit * u64::from(run.source_width);
        (usize::try_from(source).unwrap_or(usize::MAX), comparisons)
    }

    fn source_end_for_overlap(self, decoded_end: usize) -> (usize, usize) {
        if self.runs.is_empty() {
            return (self.initial_source, 0);
        }
        let decoded_end = u64::try_from(decoded_end).unwrap_or(u64::MAX);
        let mut low = 0;
        let mut high = self.runs.len();
        let mut comparisons = 0;
        while low < high {
            comparisons += 1;
            let middle = low + (high - low) / 2;
            if self.runs[middle].decoded_start < decoded_end {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let run = self.runs[low.saturating_sub(1)];
        let within = decoded_end
            .saturating_sub(run.decoded_start)
            .min(u64::from(run.units) * u64::from(run.decoded_width));
        let units = within.div_ceil(u64::from(run.decoded_width));
        let source = run.source_start + units * u64::from(run.source_width);
        (usize::try_from(source).unwrap_or(usize::MAX), comparisons)
    }

    #[cfg(test)]
    fn source_range_lookup_count(self, start: usize, end: usize) -> usize {
        if end <= start {
            self.source_boundary(start).1
        } else {
            self.source_start_for_overlap(start).1 + self.source_end_for_overlap(end).1
        }
    }
}

struct DecodedLines {
    lines: Vec<Line>,
    current: Line,
    pending_cr: bool,
    memory: LogicalMemory,
}

impl DecodedSink for DecodedLines {
    fn push_sequence(
        &mut self,
        value: &str,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        for value in value.chars() {
            if self.pending_cr {
                self.finish_line()?;
                self.pending_cr = false;
                if value == '\n' {
                    continue;
                }
            }
            match value {
                '\r' => self.pending_cr = true,
                '\n' => self.finish_line()?,
                _ => {
                    self.current.start.get_or_insert(start);
                    self.current.end = Some(end);
                    self.memory.reserve_string(&mut self.current.text, value.len_utf8())?;
                    self.current.text.push(value);
                }
            }
        }
        Ok(())
    }

    fn memory(&mut self) -> &mut LogicalMemory {
        &mut self.memory
    }
}

impl DecodedLines {
    fn new(memory: LogicalMemory) -> Self {
        Self { lines: Vec::new(), current: Line::default(), pending_cr: false, memory }
    }

    fn finish_line(&mut self) -> Result<(), ConversionError> {
        self.memory.reserve_vec(&mut self.lines, 1)?;
        self.lines.push(std::mem::take(&mut self.current));
        Ok(())
    }

    fn finish(mut self) -> Result<DecodedLineOutput, ConversionError> {
        if self.pending_cr {
            self.finish_line()?;
        }
        self.finish_line()?;
        Ok(DecodedLineOutput { lines: self.lines, memory: self.memory })
    }
}

struct DecodedLineOutput {
    lines: Vec<Line>,
    memory: LogicalMemory,
}

fn build_document(
    lines: Vec<Line>,
    max_decoded_text_bytes: u64,
    context: &ExecutionContext,
    memory: &mut LogicalMemory,
) -> Result<Document, ConversionError> {
    let mut document = Document::default();
    let mut paragraph: Vec<Line> = Vec::new();
    let mut stats = DocumentStats::default();
    for (index, line) in lines.into_iter().enumerate() {
        if index % 4096 == 0 {
            context.checkpoint()?;
        }
        if line.text.is_empty() {
            flush_paragraph(
                &mut document,
                &mut paragraph,
                &mut stats,
                max_decoded_text_bytes,
                memory,
            )?;
        } else {
            memory.reserve_vec(&mut paragraph, 1)?;
            paragraph.push(line);
        }
    }
    flush_paragraph(&mut document, &mut paragraph, &mut stats, max_decoded_text_bytes, memory)?;
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
    memory: &mut LogicalMemory,
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
    let content_capacity = lines.len().saturating_mul(2).saturating_sub(1);
    let mut content = Vec::new();
    memory.reserve_vec(&mut content, content_capacity)?;
    for (index, line) in lines.drain(..).enumerate() {
        if index > 0 {
            content.push(Inline::LineBreak);
        }
        content.push(Inline::Text { value: line.text, marks: Vec::new() });
    }
    memory.charge(64)?;
    let id = format!("text-paragraph-{}", document.blocks.len() + 1);
    memory.charge(PROVIDER_ID.len())?;
    memory.reserve_vec(&mut document.blocks, 1)?;
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
            let context = context();
            let mut memory = LogicalMemory::new(&context).unwrap();
            assert!(
                !decoded_input_safe(&unsafe_text, Charset::Utf8, &context, &mut memory).unwrap()
            );
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
        let context = context();
        let mut memory = LogicalMemory::new(&context).unwrap();
        assert!(!decoded_input_safe(&legacy, Charset::ShiftJis, &context, &mut memory).unwrap());
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

    #[test]
    fn compact_ascii_mapping_fits_a_450_kib_logical_memory_budget() {
        let bytes = vec![b'a'; 100 * 1024];
        let input = ResolvedInput { bytes: Arc::from(bytes), metadata: SourceMetadata::default() };
        let limits = ResourceLimits { max_memory_bytes: 450 * 1024, ..ResourceLimits::default() };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits);
        let output = convert_text(&input, &ConversionOptions::default(), &context).unwrap();
        assert_eq!(output.document.blocks.len(), 1);
    }

    #[test]
    fn decoding_allocations_are_budgeted_from_charset_selection_onward() {
        let limits = ResourceLimits { max_memory_bytes: 1, ..ResourceLimits::default() };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits);
        let error =
            convert_text(&input(b"a"), &ConversionOptions::default(), &context).unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));

        let mut utf16 = vec![0xff, 0xfe];
        for _ in 0..(TEXT_SNIFF_BYTE_LIMIT / 2) {
            utf16.extend(u16::from(b'a').to_le_bytes());
        }
        SAMPLE_DECODE_INVOCATIONS.with(|count| count.set(0));
        let error = sniff_unstructured_text(&utf16, &context).unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
        SAMPLE_DECODE_INVOCATIONS.with(|count| assert_eq!(count.get(), 0));
    }

    #[test]
    fn hundred_kib_auto_legacy_and_utf16_avoid_full_input_selection_copies() {
        let (legacy_unit, _, had_errors) =
            SHIFT_JIS.encode("日本語の文字コード判定に十分な長さの文章です。");
        assert!(!had_errors);
        let mut legacy = Vec::new();
        while legacy.len() + legacy_unit.len() <= 100 * 1024 {
            legacy.extend_from_slice(&legacy_unit);
        }
        legacy.resize(100 * 1024, b'A');
        let limits = ResourceLimits { max_memory_bytes: 700 * 1024, ..ResourceLimits::default() };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits.clone());
        let output =
            convert_text(&input(&legacy), &ConversionOptions::default(), &context).unwrap();
        assert_eq!(output.document.blocks.len(), 1);

        let mut utf16 = vec![0xff, 0xfe];
        utf16.extend(std::iter::repeat_n([b'a', 0], 50 * 1024).flatten());
        let context = ExecutionContext::new(ExecutionOptions::default(), limits);
        let output = convert_text(&input(&utf16), &ConversionOptions::default(), &context).unwrap();
        assert_eq!(output.document.blocks.len(), 1);
    }

    #[test]
    fn big5_multi_scalar_sequence_ranges_cover_both_source_bytes() {
        let context = context();
        let (decoded, diagnostics) =
            decode_source(&[0x88, 0x62], Some("big5"), TextDecodingMode::Strict, &context).unwrap();
        assert!(diagnostics.is_empty());
        assert_eq!(decoded.text, "Ê\u{304}");
        assert_eq!(decoded.source_range(0, 2), (0, 2));
        assert_eq!(decoded.source_range(2, 4), (0, 2));
        assert_eq!(decoded.source_range(0, 4), (0, 2));
        assert_eq!(decoded.source_range(2, 2), (0, 0));
        assert_eq!(decoded.source_range(4, 4), (2, 2));

        let mut options = ConversionOptions::default();
        options.text.charset = Some("big5".into());
        let output = convert_text(&input(&[0x88, 0x62]), &options, &context).unwrap();
        assert_eq!(output.document.blocks[0].provenance.locator.byte_start, Some(0));
        assert_eq!(output.document.blocks[0].provenance.locator.byte_end, Some(2));
    }

    #[test]
    fn source_mapping_lookup_is_logarithmic_for_16k_utf16_table_columns() {
        let mut bytes = vec![0xff, 0xfe];
        let mut decoded_ranges = Vec::new();
        let mut decoded_len = 0;
        for index in 0..16_384 {
            if index > 0 {
                bytes.extend(u16::from(b'\t').to_le_bytes());
                decoded_len += 1;
            }
            let value = if index % 2 == 0 { 'a' } else { '界' };
            let start = decoded_len;
            decoded_len += value.len_utf8();
            decoded_ranges.push((start, decoded_len));
            let mut encoded = [0; 2];
            for unit in value.encode_utf16(&mut encoded).iter() {
                bytes.extend(unit.to_le_bytes());
            }
        }
        let (decoded, _) =
            decode_source(&bytes, None, TextDecodingMode::Strict, &context()).unwrap();
        assert!(decoded.runs.len() >= 8_192);
        for (start, end) in decoded_ranges {
            assert!(decoded.source_range_lookup_count(start, end) <= 32);
        }
    }
}

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Charset {
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
    Gb18030,
    Big5,
    ShiftJis,
    Other(&'static Encoding),
}

impl Charset {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16Le => "utf-16le",
            Self::Utf16Be => "utf-16be",
            Self::Windows1252 => "windows-1252",
            Self::Gb18030 => "gb18030",
            Self::Big5 => "big5",
            Self::ShiftJis => "shift_jis",
            Self::Other(encoding) => encoding.name(),
        }
    }

    pub(super) const fn encoding(self) -> &'static Encoding {
        match self {
            Self::Utf8 => UTF_8,
            Self::Utf16Le => UTF_16LE,
            Self::Utf16Be => UTF_16BE,
            Self::Windows1252 => WINDOWS_1252,
            Self::Gb18030 => GB18030,
            Self::Big5 => BIG5,
            Self::ShiftJis => SHIFT_JIS,
            Self::Other(encoding) => encoding,
        }
    }
}

pub(super) fn normalize_charset(label: &str) -> Result<Charset, ConversionError> {
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
    } else if let Some(encoding) = Encoding::for_label(label.as_bytes()) {
        match encoding {
            value if value == UTF_8 => Charset::Utf8,
            value if value == UTF_16LE => Charset::Utf16Le,
            value if value == UTF_16BE => Charset::Utf16Be,
            value => Charset::Other(value),
        }
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

pub(super) fn bom(bytes: &[u8]) -> Option<(Charset, usize)> {
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

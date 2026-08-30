use super::budget::{MsgBudget, malformed};
use super::ole::Storage;
use encoding_rs::{Encoding, UTF_8};
use into_markdown_core::{ConversionError, ErrorPolicy};
use std::collections::BTreeMap;

pub(super) const PT_LONG: u16 = 0x0003;
pub(super) const PT_BOOLEAN: u16 = 0x000b;
pub(super) const PT_OBJECT: u16 = 0x000d;
pub(super) const PT_I8: u16 = 0x0014;
pub(super) const PT_STRING8: u16 = 0x001e;
pub(super) const PT_UNICODE: u16 = 0x001f;
pub(super) const PT_SYSTIME: u16 = 0x0040;
pub(super) const PT_BINARY: u16 = 0x0102;

const PROPERTIES: &str = "__properties_version1.0";
const PR_INTERNET_CPID: u16 = 0x3fde;
const PR_MESSAGE_CODEPAGE: u16 = 0x3ffd;
const PR_MESSAGE_LOCALE_ID: u16 = 0x3ff1;

#[derive(Clone, Copy)]
pub(super) enum PropertyScope {
    Message,
    EmbeddedMessage,
    Object,
}

#[derive(Clone, Debug)]
pub(super) enum PropertyValue {
    Integer(i64),
    Time(u64),
    Text(String),
    Binary(Vec<u8>),
    Object,
    Opaque,
}

#[derive(Clone, Debug)]
struct Property {
    value: PropertyValue,
    source: String,
}

#[derive(Clone, Debug)]
pub(super) struct Properties {
    values: BTreeMap<(u16, u16), Property>,
    codepage: u32,
    recipient_count: Option<u32>,
    attachment_count: Option<u32>,
}

impl Properties {
    pub(super) fn parse(
        storage: Storage<'_>,
        scope: PropertyScope,
        fallback_codepage: u32,
        budget: &mut MsgBudget<'_>,
    ) -> Result<Self, ConversionError> {
        let bytes = storage
            .stream(PROPERTIES)
            .ok_or_else(|| malformed(storage.path(), "missing __properties_version1.0 stream"))?;
        let header = match scope {
            PropertyScope::Message => 32,
            PropertyScope::EmbeddedMessage => 24,
            PropertyScope::Object => 8,
        };
        let root = !matches!(scope, PropertyScope::Object);
        if bytes.len() < header || !(bytes.len() - header).is_multiple_of(16) {
            return Err(malformed(storage.path(), "MAPI property stream is not record aligned"));
        }
        if bytes[..8].iter().any(|byte| *byte != 0) {
            return Err(malformed(
                storage.path(),
                "MAPI property header reserved bytes are non-zero",
            ));
        }
        if matches!(scope, PropertyScope::Message) && bytes[24..32].iter().any(|byte| *byte != 0) {
            return Err(malformed(
                storage.path(),
                "root MAPI property header reserved bytes are non-zero",
            ));
        }
        let records = bytes[header..]
            .chunks_exact(16)
            .map(RawProperty::parse)
            .collect::<Result<Vec<_>, _>>()?;
        let codepage = select_codepage(&records, &storage, fallback_codepage)?;
        let html_codepage = records
            .iter()
            .find(|record| record.id == PR_INTERNET_CPID && record.kind == PT_LONG)
            .map_or(codepage, |record| {
                u32::from_le_bytes(record.value[..4].try_into().expect("fixed property value"))
            });
        let mut values = BTreeMap::new();
        for record in records {
            budget.entry()?;
            let key = (record.id, record.kind);
            if values.contains_key(&key) {
                return Err(malformed(
                    storage.path(),
                    format!("duplicate MAPI property {:04X}{:04X}", record.id, record.kind),
                ));
            }
            let source =
                format!("{}/{}#{:04X}{:04X}", storage.path(), PROPERTIES, record.id, record.kind);
            let decoding_codepage = if record.id == 0x1013 { html_codepage } else { codepage };
            let value = decode_property(storage, record, decoding_codepage, budget)?;
            values.insert(key, Property { value, source });
        }
        let recipient_count =
            root.then(|| u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]));
        let attachment_count =
            root.then(|| u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]));
        Ok(Self { values, codepage, recipient_count, attachment_count })
    }

    pub(super) fn codepage(&self) -> u32 {
        self.codepage
    }

    pub(super) fn html_codepage(&self) -> u32 {
        self.integer(PR_INTERNET_CPID)
            .map_or(self.codepage, |value| u32::try_from(value).unwrap_or(u32::MAX))
    }

    pub(super) fn recipient_count(&self) -> Option<u32> {
        self.recipient_count
    }

    pub(super) fn attachment_count(&self) -> Option<u32> {
        self.attachment_count
    }

    pub(super) fn text(&self, id: u16) -> Option<&str> {
        [PT_UNICODE, PT_STRING8].into_iter().find_map(|kind| self.values.get(&(id, kind))).and_then(
            |property| match &property.value {
                PropertyValue::Text(value) => Some(value.as_str()),
                _ => None,
            },
        )
    }

    pub(super) fn binary(&self, id: u16) -> Option<&[u8]> {
        self.values.get(&(id, PT_BINARY)).and_then(|property| match &property.value {
            PropertyValue::Binary(value) => Some(value.as_slice()),
            _ => None,
        })
    }

    pub(super) fn integer(&self, id: u16) -> Option<i64> {
        [PT_LONG, PT_I8].into_iter().find_map(|kind| self.values.get(&(id, kind))).and_then(
            |property| match property.value {
                PropertyValue::Integer(value) => Some(value),
                _ => None,
            },
        )
    }

    pub(super) fn time(&self, id: u16) -> Option<u64> {
        self.values.get(&(id, PT_SYSTIME)).and_then(|property| match property.value {
            PropertyValue::Time(value) => Some(value),
            _ => None,
        })
    }

    pub(super) fn has_object(&self, id: u16) -> bool {
        self.values
            .get(&(id, PT_OBJECT))
            .is_some_and(|property| matches!(property.value, PropertyValue::Object))
    }

    pub(super) fn source(&self, id: u16) -> Option<&str> {
        [PT_UNICODE, PT_STRING8, PT_BINARY, PT_SYSTIME, PT_LONG, PT_I8, PT_BOOLEAN, PT_OBJECT]
            .into_iter()
            .find_map(|kind| self.values.get(&(id, kind)))
            .map(|property| property.source.as_str())
    }
}

#[derive(Clone, Copy)]
struct RawProperty {
    id: u16,
    kind: u16,
    value: [u8; 8],
}

impl RawProperty {
    fn parse(raw: &[u8]) -> Result<Self, ConversionError> {
        let tag = u32::from_le_bytes(
            raw[..4]
                .try_into()
                .map_err(|_| malformed("msg/properties", "truncated property tag"))?,
        );
        let kind = u16::try_from(tag & 0xffff)
            .map_err(|_| malformed("msg/properties", "property type cannot be represented"))?;
        let id = (tag >> 16) as u16;
        let flags = u32::from_le_bytes(
            raw[4..8]
                .try_into()
                .map_err(|_| malformed("msg/properties", "truncated property flags"))?,
        );
        if id == 0 || flags & !0x7 != 0 {
            return Err(malformed("msg/properties", "invalid MAPI property tag or flags"));
        }
        Ok(Self {
            id,
            kind,
            value: raw[8..16]
                .try_into()
                .map_err(|_| malformed("msg/properties", "truncated property value"))?,
        })
    }
}

fn select_codepage(
    records: &[RawProperty],
    storage: &Storage<'_>,
    fallback: u32,
) -> Result<u32, ConversionError> {
    let mut selected = None;
    for record in
        records.iter().filter(|record| record.kind == PT_LONG && record.id == PR_MESSAGE_CODEPAGE)
    {
        let value = u32::from_le_bytes(
            record.value[..4]
                .try_into()
                .map_err(|_| malformed(storage.path(), "truncated codepage"))?,
        );
        if let Some(previous) = selected
            && previous != value
        {
            return Err(malformed(storage.path(), "MAPI codepage properties conflict"));
        }
        encoding_for_codepage(value).ok_or_else(|| {
            malformed(storage.path(), format!("unsupported MAPI codepage {value}"))
        })?;
        selected = Some(value);
    }
    let locale = records
        .iter()
        .find(|record| record.kind == PT_LONG && record.id == PR_MESSAGE_LOCALE_ID)
        .map(|record| {
            u32::from_le_bytes(record.value[..4].try_into().expect("fixed property value"))
        });
    Ok(selected.or_else(|| locale.and_then(locale_codepage)).unwrap_or(fallback))
}

fn locale_codepage(locale: u32) -> Option<u32> {
    // Deterministic Windows ANSI defaults when the Message codepage is absent.
    // Internet codepage can independently describe UTF-8/ASCII body data.
    match locale & 0xffff {
        0x0404 | 0x0c04 | 0x1404 => Some(950),
        0x0804 | 0x1004 => Some(936),
        language => match language & 0x03ff {
            0x07 | 0x09 | 0x0c => Some(1252), // German, English, French
            0x19 => Some(1251),               // Russian
            _ => None,
        },
    }
}

fn decode_property(
    storage: Storage<'_>,
    record: RawProperty,
    codepage: u32,
    budget: &mut MsgBudget<'_>,
) -> Result<PropertyValue, ConversionError> {
    let part = format!("{}/__substg1.0_{:04X}{:04X}", storage.path(), record.id, record.kind);
    match record.kind {
        PT_LONG => {
            padding_zero(&record.value[4..], &part, budget)?;
            Ok(PropertyValue::Integer(i64::from(i32::from_le_bytes(
                record.value[..4].try_into().map_err(|_| malformed(&part, "truncated integer"))?,
            ))))
        }
        PT_I8 => Ok(PropertyValue::Integer(i64::from_le_bytes(record.value))),
        PT_BOOLEAN => {
            padding_zero(&record.value[2..], &part, budget)?;
            let value = u16::from_le_bytes([record.value[0], record.value[1]]);
            if !matches!(value, 0 | 1) {
                return Err(malformed(part, "invalid MAPI boolean"));
            }
            Ok(PropertyValue::Opaque)
        }
        PT_SYSTIME => Ok(PropertyValue::Time(u64::from_le_bytes(record.value))),
        PT_STRING8 | PT_UNICODE | PT_BINARY => {
            let length = usize::try_from(u32::from_le_bytes(
                record.value[..4]
                    .try_into()
                    .map_err(|_| malformed(&part, "truncated property length"))?,
            ))
            .map_err(|_| malformed(&part, "property length cannot be represented"))?;
            let name = format!("__substg1.0_{:04X}{:04X}", record.id, record.kind);
            let mut bytes = storage
                .stream(&name)
                .ok_or_else(|| malformed(&part, "variable MAPI property stream is missing"))?;
            let terminator_bytes = match record.kind {
                PT_STRING8 => 1,
                PT_UNICODE => 2,
                _ => 0,
            };
            let expected = bytes
                .len()
                .checked_add(terminator_bytes)
                .ok_or_else(|| malformed(&part, "variable MAPI property size overflowed"))?;
            let has_terminator = match record.kind {
                PT_STRING8 => bytes.ends_with(&[0]),
                PT_UNICODE => bytes.ends_with(&[0, 0]) && bytes.len().is_multiple_of(2),
                _ => false,
            };
            let recover_terminator =
                has_terminator && budget.options().error_policy == ErrorPolicy::BestEffort;
            if expected != length && !(recover_terminator && bytes.len() == length) {
                return Err(malformed(
                    part,
                    format!(
                        "property length {length} does not match stream length {} and string terminator contract",
                        bytes.len(),
                    ),
                ));
            }
            if recover_terminator {
                bytes = &bytes[..bytes.len() - terminator_bytes];
                budget.warning(
                    "msg.stringTerminatorIgnored",
                    "one stored string terminator was excluded from the property text",
                    &part,
                );
            }
            if matches!(record.kind, PT_STRING8 | PT_UNICODE)
                && bytes.is_empty()
                && budget.options().error_policy == ErrorPolicy::BestEffort
            {
                budget.warning(
                    "msg.emptyStringProperty",
                    "an empty string property was retained as empty, without inventing text",
                    &part,
                );
                return Ok(PropertyValue::Text(String::new()));
            }
            match record.kind {
                PT_STRING8 => decode_string8(bytes, codepage, &part).map(PropertyValue::Text),
                PT_UNICODE => decode_unicode(bytes, &part).map(PropertyValue::Text),
                _ => Ok(PropertyValue::Binary(bytes.to_vec())),
            }
        }
        PT_OBJECT => {
            if record.value[..4] != u32::MAX.to_le_bytes() {
                return Err(malformed(part, "MAPI object property does not declare a storage"));
            }
            Ok(PropertyValue::Object)
        }
        other => decode_opaque(storage, record, other, &part, budget),
    }
}

fn decode_opaque(
    storage: Storage<'_>,
    record: RawProperty,
    kind: u16,
    part: &str,
    budget: &mut MsgBudget<'_>,
) -> Result<PropertyValue, ConversionError> {
    let fixed = match kind {
        0x0002 | 0x000b => Some(2),
        0x0003 | 0x0004 | 0x000a => Some(4),
        0x0005..=0x0007 | 0x0014 | 0x0040 => Some(8),
        _ => None,
    };
    if let Some(size) = fixed {
        padding_zero(&record.value[size..], part, budget)?;
        return Ok(PropertyValue::Opaque);
    }
    if kind == 0x0048 || kind & 0x1000 != 0 {
        let length = usize::try_from(u32::from_le_bytes(
            record.value[..4]
                .try_into()
                .map_err(|_| malformed(part, "truncated opaque property length"))?,
        ))
        .map_err(|_| malformed(part, "opaque property length cannot be represented"))?;
        let name = format!("__substg1.0_{:04X}{:04X}", record.id, record.kind);
        let bytes = storage
            .stream(&name)
            .ok_or_else(|| malformed(part, "opaque variable property stream is missing"))?;
        if bytes.len() != length {
            return Err(malformed(part, "opaque property length does not match stream length"));
        }
        return Ok(PropertyValue::Opaque);
    }
    Err(malformed(part, format!("unsupported MAPI property type {kind:04X}")))
}

fn decode_unicode(bytes: &[u8], part: &str) -> Result<String, ConversionError> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return Err(malformed(part, "Unicode MAPI property is empty or has odd byte length"));
    }
    let units =
        bytes.chunks_exact(2).map(|raw| u16::from_le_bytes([raw[0], raw[1]])).collect::<Vec<_>>();
    if units.contains(&0) {
        return Err(malformed(part, "Unicode MAPI property contains embedded NUL"));
    }
    String::from_utf16(&units)
        .map_err(|_| malformed(part, "Unicode MAPI property contains invalid UTF-16"))
}

fn decode_string8(bytes: &[u8], codepage: u32, part: &str) -> Result<String, ConversionError> {
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(malformed(part, "String8 MAPI property is empty or contains NUL"));
    }
    decode_bytes(bytes, codepage, part)
}

pub(super) fn decode_bytes(
    bytes: &[u8],
    codepage: u32,
    part: &str,
) -> Result<String, ConversionError> {
    if codepage == 20127 && !bytes.is_ascii() {
        return Err(malformed(part, "non-ASCII byte in codepage 20127"));
    }
    let encoding = encoding_for_codepage(codepage)
        .ok_or_else(|| malformed(part, format!("unsupported MAPI codepage {codepage}")))?;
    encoding
        .decode_without_bom_handling_and_without_replacement(bytes)
        .map(std::borrow::Cow::into_owned)
        .ok_or_else(|| {
            malformed(part, format!("String8 property is invalid in codepage {codepage}"))
        })
}

fn encoding_for_codepage(codepage: u32) -> Option<&'static Encoding> {
    let label: &[u8] = match codepage {
        65001 | 20127 => return Some(UTF_8),
        874 => b"windows-874",
        932 => b"shift_jis",
        936 => b"gbk",
        949 => b"euc-kr",
        950 => b"big5",
        1250 => b"windows-1250",
        1251 => b"windows-1251",
        1252 | 28591 => b"windows-1252",
        1253 => b"windows-1253",
        1254 => b"windows-1254",
        1255 => b"windows-1255",
        1256 => b"windows-1256",
        1257 => b"windows-1257",
        1258 => b"windows-1258",
        _ => return None,
    };
    Encoding::for_label(label)
}

fn padding_zero(
    bytes: &[u8],
    part: &str,
    budget: &mut MsgBudget<'_>,
) -> Result<(), ConversionError> {
    if bytes.iter().any(|byte| *byte != 0) {
        if budget.options().error_policy == ErrorPolicy::BestEffort {
            budget.warning(
                "msg.propertyPaddingIgnored",
                "unused fixed-property padding was ignored",
                part,
            );
            return Ok(());
        }
        return Err(malformed(part, "MAPI property padding is non-zero"));
    }
    Ok(())
}

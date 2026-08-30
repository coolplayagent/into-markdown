use super::budget::{MsgBudget, malformed};
use super::ole::Storage;
use super::properties::{Properties, PropertyScope};
use into_markdown_core::{Asset, AssetId, ConversionError};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const PR_ATTACH_METHOD: u16 = 0x3705;
const PR_ATTACH_FILENAME: u16 = 0x3704;
const PR_ATTACH_LONG_FILENAME: u16 = 0x3707;
const PR_ATTACH_MIME_TAG: u16 = 0x370e;
const PR_ATTACH_CONTENT_ID: u16 = 0x3712;
const PR_DISPLAY_NAME: u16 = 0x3001;
const PR_ATTACH_DATA: u16 = 0x3701;

const ATTACH_BY_VALUE: i64 = 1;
const ATTACH_EMBEDDED_MSG: i64 = 5;

pub(super) struct ParsedAttachment<'a> {
    pub(super) asset: Option<Asset>,
    pub(super) nested: Option<Storage<'a>>,
    pub(super) content_id: Option<String>,
    pub(super) safe_image: bool,
    pub(super) filename: String,
    pub(super) source: String,
}

pub(super) fn parse_all<'a>(
    root: Storage<'a>,
    codepage: u32,
    budget: &mut MsgBudget<'_>,
) -> Result<Vec<ParsedAttachment<'a>>, ConversionError> {
    let mut storages = root
        .storages()
        .filter(|storage| storage.name().starts_with("__attach_version1.0_#"))
        .collect::<Vec<_>>();
    storages.sort_by(|left, right| left.name().cmp(right.name()));
    let mut cids = BTreeSet::new();
    let mut output = Vec::with_capacity(storages.len());
    for (ordinal, storage) in storages.into_iter().enumerate() {
        budget.entry()?;
        let properties = Properties::parse(storage, PropertyScope::Object, codepage, budget)?;
        let filename = properties
            .text(PR_ATTACH_LONG_FILENAME)
            .or_else(|| properties.text(PR_ATTACH_FILENAME))
            .or_else(|| properties.text(PR_DISPLAY_NAME))
            .map_or_else(|| format!("attachment-{}.bin", ordinal + 1), str::to_owned);
        validate_filename(&filename, &storage.path())?;
        let content_id = properties.text(PR_ATTACH_CONTENT_ID).map(canonical_cid).transpose()?;
        if let Some(cid) = &content_id
            && !cids.insert(cid.clone())
        {
            return Err(malformed(
                storage.path(),
                format!("duplicate attachment Content-ID {cid}"),
            ));
        }
        let method = properties
            .integer(PR_ATTACH_METHOD)
            .ok_or_else(|| malformed(storage.path(), "attachment has no PR_ATTACH_METHOD"))?;
        let storage_path = storage.path();
        let source = properties.source(PR_ATTACH_DATA).unwrap_or(&storage_path).to_owned();
        match method {
            ATTACH_BY_VALUE => {
                let bytes = properties.binary(PR_ATTACH_DATA).ok_or_else(|| {
                    malformed(storage.path(), "by-value attachment has no binary data")
                })?;
                budget.asset(bytes.len())?;
                let media_type = properties
                    .text(PR_ATTACH_MIME_TAG)
                    .map(|value| validate_media_type(value, &storage.path()))
                    .transpose()?
                    .unwrap_or_else(|| infer_media_type(&filename).into());
                let digest = format!("{:x}", Sha256::digest(bytes));
                let asset = Asset {
                    id: AssetId(format!("msg-attachment-{}-{}", ordinal + 1, &digest[..16])),
                    filename: Some(filename.clone()),
                    media_type,
                    bytes: bytes.to_vec(),
                    external_uri: None,
                };
                let safe_image = content_id.is_some() && audit_cid_image(&asset, budget)?;
                output.push(ParsedAttachment {
                    asset: Some(asset),
                    nested: None,
                    content_id,
                    safe_image,
                    filename,
                    source,
                });
            }
            ATTACH_EMBEDDED_MSG => {
                if content_id.is_some() {
                    return Err(malformed(
                        storage.path(),
                        "embedded MSG attachment cannot be a CID resource",
                    ));
                }
                if !properties.has_object(PR_ATTACH_DATA) {
                    return Err(malformed(
                        storage.path(),
                        "embedded MSG attachment lacks object property",
                    ));
                }
                let object = storage.storage("__substg1.0_3701000D").ok_or_else(|| {
                    malformed(storage.path(), "embedded MSG object storage is missing")
                })?;
                output.push(ParsedAttachment {
                    asset: None,
                    nested: Some(object),
                    content_id: None,
                    safe_image: false,
                    filename,
                    source,
                });
            }
            other => {
                return Err(malformed(
                    storage.path(),
                    format!(
                        "attachment method {other} is not an offline by-value or embedded MSG attachment"
                    ),
                ));
            }
        }
    }
    Ok(output)
}

fn audit_cid_image(asset: &Asset, budget: &MsgBudget<'_>) -> Result<bool, ConversionError> {
    if !matches!(asset.media_type.as_str(), "image/png" | "image/jpeg") {
        return Ok(false);
    }
    let mut memory = budget.context().reserve_memory(0)?;
    match crate::rtf::audit_embedded_raster(
        &asset.bytes,
        &asset.media_type,
        budget.options(),
        budget.context(),
        &mut memory,
    ) {
        Ok(()) => Ok(true),
        Err(ConversionError::Malformed { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

fn validate_filename(value: &str, part: &str) -> Result<(), ConversionError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.len() > 255
        || value.starts_with(['/', '\\'])
        || value.contains(['/', '\\', ':'])
        || value.chars().any(char::is_control)
        || value.trim() != value
    {
        return Err(malformed(part, "attachment filename is unsafe"));
    }
    Ok(())
}

pub(super) fn canonical_cid(value: &str) -> Result<String, ConversionError> {
    let value = value.trim();
    let value = value.strip_prefix('<').and_then(|inner| inner.strip_suffix('>')).unwrap_or(value);
    if value.is_empty()
        || value.len() > 998
        || !value.is_ascii()
        || value.bytes().any(|byte| {
            byte.is_ascii_control()
                || byte.is_ascii_whitespace()
                || matches!(byte, b'<' | b'>' | b'"')
        })
    {
        return Err(malformed("msg/attachment/content-id", "attachment Content-ID is unsafe"));
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_media_type(value: &str, part: &str) -> Result<String, ConversionError> {
    let value = value.trim().to_ascii_lowercase();
    let Some((kind, subtype)) = value.split_once('/') else {
        return Err(malformed(part, "attachment MIME type has no subtype"));
    };
    let token = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-')
            })
    };
    if !token(kind) || !token(subtype) {
        return Err(malformed(part, "attachment MIME type is unsafe"));
    }
    Ok(value)
}

fn infer_media_type(filename: &str) -> &'static str {
    match filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "msg" => "application/vnd.ms-outlook",
        _ => "application/octet-stream",
    }
}

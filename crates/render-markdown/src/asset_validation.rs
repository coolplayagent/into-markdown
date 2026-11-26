use super::{asset_plan_error, validate_external_uri};
use into_markdown_core::{Asset, AssetMode, ConversionError, ConversionOptions, Document};

pub(super) fn validate_document(document: &Document) -> Result<(), ConversionError> {
    document.validate().map_err(|error| ConversionError::Internal {
        detail: format!(
            "asset planner received invalid document IR ({} at {}): {}",
            error.code.as_str(),
            error.path,
            error.detail
        ),
    })
}

pub(super) fn validate_empty_asset(
    asset: &Asset,
    options: &ConversionOptions,
    id: &str,
) -> Result<(), ConversionError> {
    if options.output.asset_mode == AssetMode::Omit {
        Ok(())
    } else {
        validate_external_uri(asset.external_uri.as_deref(), id)
    }
}

fn valid_media_type(value: &str) -> bool {
    let Some((kind, subtype)) = value.split_once('/') else {
        return false;
    };
    !kind.is_empty()
        && !subtype.is_empty()
        && !subtype.contains('/')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-' | b'/'
                )
        })
}

pub(super) fn normalize_media_type(value: &str) -> Result<String, ConversionError> {
    if !valid_media_type(value) {
        return Err(asset_plan_error(
            "invalidAssetMediaType",
            format!("invalid asset media type {value}"),
        ));
    }
    Ok(value.to_ascii_lowercase())
}

pub(super) fn media_type_extension(media_type: &str) -> Option<String> {
    let extension = match media_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/tiff" => "tiff",
        "image/bmp" => "bmp",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/avif" => "avif",
        "image/emf" | "image/x-emf" => "emf",
        "application/pdf" => "pdf",
        "application/x-openoffice-gdimetafile" => "svm",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "application/json" => "json",
        "application/zip" => "zip",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.ms-excel.sheet.binary.macroenabled.12" => "xlsb",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.oasis.opendocument.text" => "odt",
        "application/vnd.oasis.opendocument.spreadsheet" => "ods",
        "application/vnd.oasis.opendocument.presentation" => "odp",
        "application/rtf" | "text/rtf" => "rtf",
        "application/epub+zip" => "epub",
        "text/markdown" => "md",
        "text/html" => "html",
        "text/tab-separated-values" => "tsv",
        "application/xml" | "text/xml" => "xml",
        "application/vnd.jgraph.mxfile" => "drawio",
        "application/x-ipynb+json" => "ipynb",
        "application/vnd.rar" => "rar",
        "application/vnd.ms-outlook" => "msg",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/ogg" => "ogg",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/x-matroska" => "mkv",
        "video/webm" => "webm",
        _ => return None,
    };
    Some(extension.into())
}

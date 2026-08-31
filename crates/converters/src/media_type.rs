//! Exact MIME hints shared by format support admission and source detection.
use into_markdown_core::InputFormat;

pub(super) fn format_from_media_type(media_type: &str) -> Option<InputFormat> {
    Some(match media_type.split(';').next()?.trim().to_ascii_lowercase().as_str() {
        "application/pdf" => InputFormat::Pdf,
        "application/rtf" | "text/rtf" => InputFormat::Rtf,
        "application/epub+zip" => InputFormat::Epub,
        "application/msword" => InputFormat::Doc,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.ms-word.document.macroenabled.12" => InputFormat::Docx,
        "application/vnd.ms-powerpoint" => InputFormat::Ppt,
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.ms-powerpoint.presentation.macroenabled.12"
        | "application/vnd.openxmlformats-officedocument.presentationml.slideshow"
        | "application/vnd.ms-powerpoint.slideshow.macroenabled.12"
        | "application/vnd.openxmlformats-officedocument.presentationml.template" => {
            InputFormat::Pptx
        }
        "application/vnd.ms-excel" => InputFormat::Xls,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.ms-excel.sheet.macroenabled.12"
        | "application/vnd.ms-excel.sheet.binary.macroenabled.12" => InputFormat::Xlsx,
        "application/vnd.oasis.opendocument.text" => InputFormat::Odt,
        "application/vnd.oasis.opendocument.spreadsheet" => InputFormat::Ods,
        "application/vnd.oasis.opendocument.presentation" => InputFormat::Odp,
        "application/vnd.ms-outlook" => InputFormat::OutlookMsg,
        "application/vnd.jgraph.mxfile" => InputFormat::Drawio,
        "application/json" => InputFormat::Json,
        "application/xml" | "text/xml" => InputFormat::Xml,
        "text/html" | "application/xhtml+xml" => InputFormat::Html,
        "application/rss+xml" | "application/atom+xml" => InputFormat::Feed,
        "application/x-ipynb+json" => InputFormat::Ipynb,
        "text/csv" => InputFormat::Csv,
        "text/tab-separated-values" => InputFormat::Tsv,
        "text/markdown" => InputFormat::Markdown,
        "text/plain" => InputFormat::Text,
        "application/zip" => InputFormat::Zip,
        "image/png" | "image/jpeg" | "image/tiff" | "image/webp" | "image/bmp"
        | "image/x-ms-bmp" => InputFormat::Image,
        "audio/wav" | "audio/x-wav" | "audio/vnd.wave" | "audio/mpeg" | "audio/mp4"
        | "audio/x-m4a" | "audio/flac" | "audio/x-flac" | "audio/ogg" => InputFormat::Audio,
        "video/mp4" | "video/quicktime" | "video/x-matroska" | "video/webm" | "video/x-msvideo" => {
            InputFormat::Video
        }
        _ => return None,
    })
}

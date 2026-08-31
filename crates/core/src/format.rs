use serde::{Deserialize, Serialize};
use std::fmt;

/// Input formats represented by the long-term converter matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum InputFormat {
    /// Portable Document Format.
    Pdf,
    /// Legacy Word binary document.
    Doc,
    /// Office Open XML Word document, including macro-enabled variants.
    Docx,
    /// Legacy `PowerPoint` binary presentation and slideshow variants.
    Ppt,
    /// Office Open XML presentation and slideshow variants.
    Pptx,
    /// Legacy Excel binary workbook.
    Xls,
    /// Office Open XML or binary workbook.
    Xlsx,
    /// `OpenDocument` text.
    Odt,
    /// `OpenDocument` spreadsheet.
    Ods,
    /// `OpenDocument` presentation.
    Odp,
    /// Rich Text Format.
    Rtf,
    /// EPUB publication.
    Epub,
    /// Plain text.
    Text,
    /// Markdown source.
    Markdown,
    /// HTML document.
    Html,
    /// Comma-separated values.
    Csv,
    /// Tab-separated values.
    Tsv,
    /// JSON data.
    Json,
    /// XML data.
    Xml,
    /// Drawio diagrams with text, groups and connections.
    Drawio,
    /// RSS or Atom feed.
    Feed,
    /// Jupyter notebook.
    Ipynb,
    /// Raster image.
    Image,
    /// Audio media.
    Audio,
    /// Video media.
    Video,
    /// ZIP archive.
    Zip,
    /// Outlook MSG message.
    OutlookMsg,
    /// `YouTube` resource identified by URI.
    YouTube,
    /// Wikipedia page identified by URI.
    Wikipedia,
}

impl InputFormat {
    /// Stable lowercase identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Doc => "doc",
            Self::Docx => "docx",
            Self::Ppt => "ppt",
            Self::Pptx => "pptx",
            Self::Xls => "xls",
            Self::Xlsx => "xlsx",
            Self::Odt => "odt",
            Self::Ods => "ods",
            Self::Odp => "odp",
            Self::Rtf => "rtf",
            Self::Epub => "epub",
            Self::Text => "text",
            Self::Markdown => "markdown",
            Self::Html => "html",
            Self::Csv => "csv",
            Self::Tsv => "tsv",
            Self::Json => "json",
            Self::Xml => "xml",
            Self::Drawio => "drawio",
            Self::Feed => "feed",
            Self::Ipynb => "ipynb",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Zip => "zip",
            Self::OutlookMsg => "outlook-msg",
            Self::YouTube => "youtube",
            Self::Wikipedia => "wikipedia",
        }
    }

    /// Map a filename extension to its format family.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        let extension = extension.trim_start_matches('.').to_ascii_lowercase();
        Some(match extension.as_str() {
            "pdf" => Self::Pdf,
            "doc" => Self::Doc,
            "docx" | "docm" => Self::Docx,
            "ppt" | "pps" | "pot" => Self::Ppt,
            "pptx" | "pptm" | "ppsx" | "ppsm" | "potx" => Self::Pptx,
            "xls" => Self::Xls,
            "xlsx" | "xlsm" | "xlsb" => Self::Xlsx,
            "odt" => Self::Odt,
            "ods" => Self::Ods,
            "odp" => Self::Odp,
            "rtf" => Self::Rtf,
            "epub" => Self::Epub,
            "txt" | "text" | "log" => Self::Text,
            "md" | "markdown" | "mdown" => Self::Markdown,
            "html" | "htm" => Self::Html,
            "csv" => Self::Csv,
            "tsv" => Self::Tsv,
            "json" => Self::Json,
            "xml" => Self::Xml,
            "drawio" => Self::Drawio,
            "rss" | "atom" => Self::Feed,
            "ipynb" => Self::Ipynb,
            "png" | "jpg" | "jpeg" | "tif" | "tiff" | "webp" | "bmp" => Self::Image,
            "wav" | "mp3" | "m4a" | "flac" | "ogg" => Self::Audio,
            "mp4" | "mov" | "mkv" | "webm" => Self::Video,
            "zip" => Self::Zip,
            "msg" => Self::OutlookMsg,
            _ => return None,
        })
    }
}

impl fmt::Display for InputFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A format hypothesis emitted by a detector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormatCandidate {
    /// Candidate format.
    pub format: InputFormat,
    /// Confidence in the inclusive range `0.0..=1.0`.
    pub confidence: f32,
    /// Human-readable evidence, useful in diagnostics.
    pub evidence: String,
    /// Stable ID of the detector that produced this hypothesis.
    #[serde(default)]
    pub detector_id: String,
    /// Detector priority used as a deterministic confidence tie breaker.
    #[serde(default)]
    pub detector_priority: i32,
    /// Non-fatal details about conflicts, limits, or partial inspection.
    #[serde(default)]
    pub diagnostics: Vec<String>,
    /// Whether the caller explicitly selected this format.
    pub explicit: bool,
}

impl FormatCandidate {
    /// Create a candidate while clamping confidence to a valid finite value.
    #[must_use]
    pub fn new(format: InputFormat, confidence: f32, evidence: impl Into<String>) -> Self {
        let confidence = if confidence.is_finite() { confidence.clamp(0.0, 1.0) } else { 0.0 };
        Self {
            format,
            confidence,
            evidence: evidence.into(),
            detector_id: String::new(),
            detector_priority: 0,
            diagnostics: Vec::new(),
            explicit: false,
        }
    }

    /// Mark a candidate as explicitly selected by the caller.
    #[must_use]
    pub fn explicit(format: InputFormat) -> Self {
        Self {
            format,
            confidence: 1.0,
            evidence: "explicit format hint".into(),
            detector_id: "builtin.detector.explicit".into(),
            detector_priority: i32::MAX,
            diagnostics: Vec::new(),
            explicit: true,
        }
    }

    /// Attach one non-fatal diagnostic to this hypothesis.
    #[must_use]
    pub fn with_diagnostic(mut self, diagnostic: impl Into<String>) -> Self {
        self.diagnostics.push(diagnostic.into());
        self
    }
}

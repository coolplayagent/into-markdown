use serde::{Deserialize, Serialize};

/// Local OCR routing policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OcrPolicy {
    /// Never invoke OCR.
    Off,
    /// Invoke OCR for images, scanned pages, or insufficient native text.
    Auto,
    /// Invoke OCR for every eligible visual region.
    Always,
}

/// Local OCR settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrOptions {
    /// Routing policy.
    pub policy: OcrPolicy,
    /// Optional model bundle ID; the packaged default is used when absent.
    pub model_bundle: Option<String>,
    /// Lowest confidence accepted without fallback.
    pub minimum_confidence: f32,
}

impl Default for OcrOptions {
    fn default() -> Self {
        Self { policy: OcrPolicy::Auto, model_bundle: None, minimum_confidence: 0.70 }
    }
}

/// Local speech-recognition settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AsrOptions {
    /// Installed multilingual Whisper bundle.
    pub model_bundle: String,
    /// Optional BCP-47 language hint; absence enables model detection.
    pub language: Option<String>,
    /// Maximum native decoder threads.
    pub max_threads: u16,
    /// Maximum decoded media duration accepted by ASR.
    pub max_duration_ms: u64,
    /// Maximum timed transcript segments retained in the IR.
    pub max_segments: u32,
    /// Conservative native model and decoder memory reservation.
    pub max_native_memory_bytes: u64,
}

impl Default for AsrOptions {
    fn default() -> Self {
        Self {
            model_bundle: "whisper-small-multilingual".into(),
            language: None,
            max_threads: 4,
            max_duration_ms: 600_000,
            max_segments: 10_000,
            max_native_memory_bytes: 900 * 1024 * 1024,
        }
    }
}

/// Routing mode for each optional AI capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiMode {
    /// Do not use the capability.
    Off,
    /// Use it only after deterministic/local processing is insufficient.
    Fallback,
    /// Prefer it but retain a deterministic/local fallback.
    Prefer,
    /// Require it and fail when unavailable.
    Only,
}

/// Per-capability AI routing configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiOptions {
    /// Vision-based OCR.
    pub vision_ocr: AiMode,
    /// Non-text image description.
    pub image_description: AiMode,
    /// Reading order and page-layout repair.
    pub layout_repair: AiMode,
    /// Table reconstruction.
    pub table_repair: AiMode,
    /// Formula reconstruction.
    pub formula_repair: AiMode,
    /// Audio transcription.
    pub audio_transcription: AiMode,
    /// Final Markdown post-processing.
    pub markdown_postprocess: AiMode,
}

impl Default for AiOptions {
    fn default() -> Self {
        Self {
            vision_ocr: AiMode::Off,
            image_description: AiMode::Off,
            layout_repair: AiMode::Off,
            table_repair: AiMode::Off,
            formula_repair: AiMode::Off,
            audio_transcription: AiMode::Off,
            markdown_postprocess: AiMode::Off,
        }
    }
}

/// Network security policy. Network access is denied by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkOptions {
    /// Master network permission.
    pub enabled: bool,
    /// Maximum redirects per request.
    pub max_redirects: u8,
    /// Reject private, loopback, link-local, and metadata-service addresses.
    pub deny_private_networks: bool,
    /// Optional hostname allowlist. Empty means any public hostname.
    pub allowed_hosts: Vec<String>,
}

impl Default for NetworkOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            max_redirects: 3,
            deny_private_networks: true,
            allowed_hosts: vec![],
        }
    }
}

/// Fixed resource budgets used by every implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceLimits {
    /// Maximum source bytes.
    pub max_input_bytes: u64,
    /// Maximum decompressed bytes across an archive.
    pub max_decompressed_bytes: u64,
    /// Maximum archive entry count.
    pub max_archive_entries: u32,
    /// Maximum nested archive depth, counting the outer archive as one.
    pub max_archive_depth: u16,
    /// Maximum decompressed bytes retained for one archive member.
    pub max_archive_entry_bytes: u64,
    /// Maximum declared decompressed-to-compressed ratio for one archive member.
    pub max_archive_compression_ratio: u32,
    /// Maximum XML/container nesting.
    pub max_nesting_depth: u16,
    /// Maximum PDF/page-like units.
    pub max_pages: u32,
    /// Maximum bytes retained by one asset.
    pub max_asset_bytes: u64,
    /// Maximum bytes retained across all assets before content deduplication.
    pub max_total_asset_bytes: u64,
    /// Maximum request-scoped memory explicitly reserved by implementations.
    pub max_memory_bytes: u64,
    /// Maximum bytes written to request-scoped temporary files.
    pub max_temporary_bytes: u64,
    /// Maximum records in a delimited-text table.
    pub max_table_rows: u64,
    /// Maximum columns in a delimited-text table.
    pub max_table_columns: u64,
    /// Maximum cells in a delimited-text table.
    pub max_table_cells: u64,
    /// Maximum decoded UTF-8 bytes in one delimited-text field.
    pub max_field_bytes: u64,
    /// Maximum entries accepted from one RSS or Atom feed.
    pub max_feed_entries: u32,
    /// Maximum decoded text bytes retained across one feed.
    pub max_feed_text_bytes: u64,
    /// Maximum source bytes passed to nested HTML extraction across one feed.
    pub max_feed_html_bytes: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 512 * 1024 * 1024,
            max_decompressed_bytes: 1024 * 1024 * 1024,
            max_archive_entries: 100_000,
            max_archive_depth: 16,
            max_archive_entry_bytes: 256 * 1024 * 1024,
            max_archive_compression_ratio: 100,
            max_nesting_depth: 256,
            max_pages: 10_000,
            max_asset_bytes: 256 * 1024 * 1024,
            max_total_asset_bytes: 1024 * 1024 * 1024,
            max_memory_bytes: 1024 * 1024 * 1024,
            max_temporary_bytes: 1024 * 1024 * 1024,
            max_table_rows: 100_000,
            max_table_columns: 16_384,
            max_table_cells: 1_000_000,
            max_field_bytes: 16 * 1024 * 1024,
            max_feed_entries: 10_000,
            max_feed_text_bytes: 64 * 1024 * 1024,
            max_feed_html_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Markdown and asset output policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputOptions {
    /// Emit GitHub-Flavored Markdown.
    pub flavor: String,
    /// Suggested CLI asset directory suffix.
    pub asset_directory_suffix: String,
    /// Preserve provenance in the structured result.
    pub include_provenance: bool,
    /// How visual and embedded assets are represented in Markdown.
    pub asset_mode: AssetMode,
    /// URI prefix used by the renderer for extracted assets.
    pub asset_uri_prefix: Option<String>,
}

/// Invalid-byte handling used by plain-text conversion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextDecodingMode {
    /// Reject the first malformed or truncated byte sequence.
    #[default]
    Strict,
    /// Insert U+FFFD and emit one byte-ranged diagnostic for every recovery.
    Replace,
}

/// Plain-text decoding policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextOptions {
    /// Explicit normalized or alias encoding label. Normally populated from `FormatHint`.
    pub charset: Option<String>,
    /// Invalid-byte handling. Strict is the safe default.
    pub decoding_mode: TextDecodingMode,
}

/// Header-row selection for delimited text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TableHeaderMode {
    /// Use the conservative, deterministic header heuristic.
    #[default]
    Auto,
    /// Treat the first record as a header row.
    Always,
    /// Treat every source record as data; the renderer supplies the required empty GFM header.
    Never,
}

/// Handling of records whose field count differs from the table width.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RaggedRowsMode {
    /// Reject the malformed table.
    #[default]
    Strict,
    /// Pad short records with empty cells; records wider than the first remain invalid.
    Pad,
}

/// CSV and TSV parsing policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DelimitedTextOptions {
    /// Header detection or override.
    pub header: TableHeaderMode,
    /// Ragged-record recovery policy.
    pub ragged_rows: RaggedRowsMode,
}

/// Markdown representation policy for assets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetMode {
    /// Reference assets written separately by the caller.
    Extract,
    /// Encode asset bytes as data URIs where the renderer supports it.
    Embed,
    /// Omit binary resources while retaining text alternatives.
    Omit,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            flavor: "gfm".into(),
            asset_directory_suffix: "_assets".into(),
            include_provenance: true,
            asset_mode: AssetMode::Extract,
            asset_uri_prefix: None,
        }
    }
}

/// Complete policy passed through the conversion pipeline.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversionOptions {
    /// Plain-text decoding policy.
    #[serde(default)]
    pub text: TextOptions,
    /// CSV and TSV parsing policy.
    #[serde(default)]
    pub delimited_text: DelimitedTextOptions,
    /// Local OCR policy.
    pub ocr: OcrOptions,
    /// Local speech-recognition policy.
    #[serde(default)]
    pub asr: AsrOptions,
    /// Optional AI capability policies.
    pub ai: AiOptions,
    /// Explicit network permissions and SSRF controls.
    pub network: NetworkOptions,
    /// Resource budgets.
    pub limits: ResourceLimits,
    /// Output policy.
    pub output: OutputOptions,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_offline_and_ai_free() {
        let defaults = ConversionOptions::default();
        assert!(!defaults.network.enabled);
        assert_eq!(defaults.ocr.policy, OcrPolicy::Auto);
        assert_eq!(defaults.ai.vision_ocr, AiMode::Off);
        assert_eq!(defaults.ai.markdown_postprocess, AiMode::Off);
    }

    #[test]
    fn additive_asr_options_default_when_older_payloads_omit_them() {
        let mut value = serde_json::to_value(ConversionOptions::default()).unwrap();
        value.as_object_mut().unwrap().remove("asr");

        let decoded: ConversionOptions = serde_json::from_value(value).unwrap();

        assert_eq!(decoded.asr, AsrOptions::default());
    }
}

use crate::msg::ole::CompoundBudget;
use image::{ImageDecoder as _, ImageFormat, ImageReader, Limits as ImageLimits};
use into_markdown_core::{
    ConversionError, ConversionOptions, ExecutionContext, ResourceReservation,
};
use std::io::Cursor;

const MAX_RASTER_DIMENSION: u32 = 32_768;
const MAX_RASTER_PIXELS: u64 = 16_000_000;

/// One request-wide budget shared by the CFB envelope and legacy format parser.
pub(super) struct LegacyBudget<'a> {
    options: &'a ConversionOptions,
    context: &'a ExecutionContext,
    entries: u32,
    expanded_bytes: u64,
    work: u64,
    assets: u64,
}

impl<'a> LegacyBudget<'a> {
    pub(super) fn new(
        input_bytes: usize,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let input_bytes = u64::try_from(input_bytes).unwrap_or(u64::MAX);
        if input_bytes > options.limits.max_input_bytes {
            return Err(limit(
                "max_input_bytes",
                format!(
                    "legacy Office source has {input_bytes} bytes; limit is {}",
                    options.limits.max_input_bytes
                ),
            ));
        }
        context.checkpoint()?;
        Ok(Self { options, context, entries: 0, expanded_bytes: 0, work: 0, assets: 0 })
    }

    pub(super) fn checkpoint(&self) -> Result<(), ConversionError> {
        self.context.checkpoint()
    }

    pub(super) fn max_field_bytes(&self) -> u64 {
        self.options.limits.max_field_bytes
    }

    pub(super) fn work(&mut self, units: u64, part: &str) -> Result<(), ConversionError> {
        let maximum = self
            .options
            .limits
            .max_input_bytes
            .saturating_mul(32)
            .saturating_add(u64::from(self.options.limits.max_archive_entries) * 4096);
        self.work = self
            .work
            .checked_add(units)
            .ok_or_else(|| limit("legacy_office_work", "parser work count overflowed"))?;
        if self.work > maximum {
            return Err(limit(
                "legacy_office_work",
                format!("{part} parser work exceeds {maximum} units"),
            ));
        }
        self.checkpoint()
    }

    pub(super) fn depth(&self, depth: u16, part: &str) -> Result<(), ConversionError> {
        if depth > self.options.limits.max_nesting_depth {
            return Err(limit(
                "max_nesting_depth",
                format!(
                    "{part} nesting reached {depth}; limit is {}",
                    self.options.limits.max_nesting_depth
                ),
            ));
        }
        self.checkpoint()
    }

    pub(super) fn pages(&self, count: usize, part: &str) -> Result<(), ConversionError> {
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        if count > self.options.limits.max_pages {
            return Err(limit(
                "max_pages",
                format!(
                    "{part} contains {count} page-like units; limit is {}",
                    self.options.limits.max_pages
                ),
            ));
        }
        self.checkpoint()
    }

    pub(super) fn table_shape(&self, rows: usize, columns: usize) -> Result<(), ConversionError> {
        let rows = u64::try_from(rows).unwrap_or(u64::MAX);
        let columns = u64::try_from(columns).unwrap_or(u64::MAX);
        let cells = rows.saturating_mul(columns);
        if rows > self.options.limits.max_table_rows {
            return Err(limit("max_table_rows", format!("{rows} rows exceed the limit")));
        }
        if columns > self.options.limits.max_table_columns {
            return Err(limit("max_table_columns", format!("{columns} columns exceed the limit")));
        }
        if cells > self.options.limits.max_table_cells {
            return Err(limit("max_table_cells", format!("{cells} cells exceed the limit")));
        }
        self.checkpoint()
    }

    pub(super) fn asset(&mut self, bytes: usize, part: &str) -> Result<(), ConversionError> {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        if bytes > self.options.limits.max_asset_bytes {
            return Err(limit(
                "max_asset_bytes",
                format!(
                    "{part} retains {bytes} bytes; limit is {}",
                    self.options.limits.max_asset_bytes
                ),
            ));
        }
        self.assets = self
            .assets
            .checked_add(bytes)
            .ok_or_else(|| limit("max_total_asset_bytes", "asset byte count overflowed"))?;
        if self.assets > self.options.limits.max_total_asset_bytes {
            return Err(limit(
                "max_total_asset_bytes",
                format!(
                    "legacy Office assets retain {} bytes; limit is {}",
                    self.assets, self.options.limits.max_total_asset_bytes
                ),
            ));
        }
        self.checkpoint()
    }

    /// Fully authenticate retained raster payloads under the request's decode budget.
    pub(super) fn raster(
        &self,
        bytes: &[u8],
        media_type: &str,
        part: &str,
    ) -> Result<(), ConversionError> {
        let format = ImageFormat::from_mime_type(media_type)
            .filter(|format| matches!(format, ImageFormat::Png | ImageFormat::Jpeg))
            .ok_or_else(|| malformed(part, "unsupported embedded raster media type"))?;
        if image::guess_format(bytes).map_err(|_| malformed(part, "image signature is invalid"))?
            != format
        {
            return Err(malformed(part, "image media type and sniffed bytes disagree"));
        }
        let decode_ceiling =
            self.options.limits.max_decompressed_bytes.min(self.options.limits.max_memory_bytes);
        let mut limits = ImageLimits::default();
        limits.max_image_width = Some(MAX_RASTER_DIMENSION);
        limits.max_image_height = Some(MAX_RASTER_DIMENSION);
        limits.max_alloc = Some(decode_ceiling);
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        reader.limits(limits.clone());
        let mut decoder = reader
            .into_decoder()
            .map_err(|_| malformed(part, "image decoder rejected the header"))?;
        let (width, height) = decoder.dimensions();
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| limit("image_pixels", format!("{part} dimensions overflow")))?;
        if width == 0
            || height == 0
            || width > MAX_RASTER_DIMENSION
            || height > MAX_RASTER_DIMENSION
            || pixels > MAX_RASTER_PIXELS
        {
            return Err(limit(
                "image_pixels",
                format!("{part} has unsafe dimensions {width}x{height}"),
            ));
        }
        decoder
            .set_limits(limits)
            .map_err(|_| limit("image_decode_memory", format!("decoder limits rejected {part}")))?;
        let decoded_bytes = decoder.total_bytes();
        if decoded_bytes > decode_ceiling {
            return Err(limit(
                "max_decompressed_bytes",
                format!("{part} expands to {decoded_bytes} bytes; limit is {decode_ceiling}"),
            ));
        }
        let decoded_bytes = usize::try_from(decoded_bytes).map_err(|_| {
            limit("image_decode_memory", "decoded raster size is not representable")
        })?;
        let mut raster_buffer = Vec::new();
        raster_buffer.try_reserve_exact(decoded_bytes).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve raster decode buffer: {error}"))
        })?;
        raster_buffer.resize(decoded_bytes, 0);
        self.checkpoint()?;
        decoder
            .read_image(&mut raster_buffer)
            .map_err(|_| malformed(part, "image codec payload is not decodable"))?;
        self.checkpoint()
    }
}

impl CompoundBudget for LegacyBudget<'_> {
    fn cfb_memory(&self, bytes: u64) -> Result<ResourceReservation, ConversionError> {
        self.context.reserve_memory(bytes)
    }

    fn cfb_entry(&mut self) -> Result<(), ConversionError> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| limit("max_archive_entries", "CFB entry count overflowed"))?;
        if self.entries > self.options.limits.max_archive_entries {
            return Err(limit(
                "max_archive_entries",
                format!(
                    "CFB contains more than {} entries",
                    self.options.limits.max_archive_entries
                ),
            ));
        }
        self.checkpoint()
    }

    fn cfb_expanded(&mut self, bytes: u64) -> Result<(), ConversionError> {
        self.expanded_bytes = self
            .expanded_bytes
            .checked_add(bytes)
            .ok_or_else(|| limit("max_decompressed_bytes", "CFB stream byte count overflowed"))?;
        if self.expanded_bytes > self.options.limits.max_decompressed_bytes {
            return Err(limit(
                "max_decompressed_bytes",
                format!(
                    "CFB streams retain {} bytes; limit is {}",
                    self.expanded_bytes, self.options.limits.max_decompressed_bytes
                ),
            ));
        }
        self.checkpoint()
    }

    fn cfb_depth(&self, depth: u16, part: &str) -> Result<(), ConversionError> {
        self.depth(depth, part)
    }

    fn cfb_work(&mut self, units: u64) -> Result<(), ConversionError> {
        self.work(units, "cfb")
    }
}

pub(super) fn malformed(part: impl Into<String>, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some(part.into()), detail: detail.into() }
}

pub(super) fn limit(name: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}

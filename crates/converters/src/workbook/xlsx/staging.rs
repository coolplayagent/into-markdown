use crate::workbook::error::{limit, malformed};
use crate::workbook::xlsx::sheet_index::{CellToken, CellValueToken};
use into_markdown_core::{ConversionError, ExecutionContext, TemporaryFile};
use std::io::{BufReader, Read, Seek, SeekFrom};

const WRITE_BATCH_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct StagingTelemetry {
    pub(super) writes: u64,
    pub(super) flushes: u64,
    pub(super) reads: u64,
    pub(super) seeks: u64,
    pub(super) staged_bytes: u64,
    pub(super) temporary_high_water: u64,
}

pub(super) struct StagedCells {
    file: TemporaryFile,
    count: u64,
    telemetry: StagingTelemetry,
}

pub(super) struct StagingWriter {
    file: TemporaryFile,
    batch: Vec<u8>,
    count: u64,
    telemetry: StagingTelemetry,
}

pub(super) struct StagedReader {
    _owner: TemporaryFile,
    reader: BufReader<std::fs::File>,
    remaining: u64,
}

impl StagingWriter {
    pub(super) fn new(context: &ExecutionContext) -> Result<Self, ConversionError> {
        Ok(Self {
            file: context.temporary_file("into-md-xlsx-cells")?,
            batch: Vec::with_capacity(WRITE_BATCH_BYTES),
            count: 0,
            telemetry: StagingTelemetry::default(),
        })
    }

    pub(super) fn push(
        &mut self,
        cell: &CellToken,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        encode_cell(cell, &mut self.batch)?;
        self.count = self.count.saturating_add(1);
        if self.batch.len() >= WRITE_BATCH_BYTES {
            flush_batch(&mut self.file, &mut self.batch, &mut self.telemetry, context)?;
        }
        Ok(())
    }

    pub(super) fn finish(
        mut self,
        context: &ExecutionContext,
    ) -> Result<StagedCells, ConversionError> {
        flush_batch(&mut self.file, &mut self.batch, &mut self.telemetry, context)?;
        self.file.flush()?;
        self.telemetry.flushes = self.telemetry.flushes.saturating_add(1);
        Ok(StagedCells { file: self.file, count: self.count, telemetry: self.telemetry })
    }
}

#[cfg(test)]
pub(super) fn stage(
    cells: &[CellToken],
    context: &ExecutionContext,
) -> Result<StagedCells, ConversionError> {
    let mut writer = StagingWriter::new(context)?;
    for cell in cells {
        context.checkpoint()?;
        writer.push(cell, context)?;
    }
    writer.finish(context)
}

impl StagedCells {
    pub(super) fn telemetry(&self) -> StagingTelemetry {
        StagingTelemetry { reads: self.count, seeks: 1, ..self.telemetry }
    }

    pub(super) fn into_reader(mut self) -> Result<StagedReader, ConversionError> {
        self.file.seek(SeekFrom::Start(0)).map_err(ConversionError::from)?;
        let reader = self.file.as_file()?.try_clone().map_err(ConversionError::from)?;
        Ok(StagedReader {
            _owner: self.file,
            reader: BufReader::new(reader),
            remaining: self.count,
        })
    }

    #[cfg(test)]
    pub(super) fn read_all(
        mut self,
        context: &ExecutionContext,
    ) -> Result<(Vec<CellToken>, StagingTelemetry), ConversionError> {
        self.file.seek(SeekFrom::Start(0)).map_err(ConversionError::from)?;
        self.telemetry.seeks = self.telemetry.seeks.saturating_add(1);
        let mut output = Vec::new();
        output
            .try_reserve_exact(usize::try_from(self.count).unwrap_or(usize::MAX))
            .map_err(|_| limit("max_memory_bytes", "cannot reserve staged worksheet cells"))?;
        let mut reader = self.file.as_file()?;
        for _ in 0..self.count {
            context.checkpoint()?;
            output.push(decode_cell(&mut reader)?);
            self.telemetry.reads = self.telemetry.reads.saturating_add(1);
        }
        Ok((output, self.telemetry))
    }
}

impl StagedReader {
    pub(super) fn next(&mut self) -> Result<Option<CellToken>, ConversionError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let cell = decode_cell(&mut self.reader)?;
        self.remaining -= 1;
        Ok(Some(cell))
    }
}

fn flush_batch(
    file: &mut TemporaryFile,
    batch: &mut Vec<u8>,
    telemetry: &mut StagingTelemetry,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    if batch.is_empty() {
        return Ok(());
    }
    file.write_all_checked(batch)?;
    telemetry.writes = telemetry.writes.saturating_add(1);
    telemetry.staged_bytes =
        telemetry.staged_bytes.saturating_add(u64::try_from(batch.len()).unwrap_or(u64::MAX));
    telemetry.temporary_high_water =
        telemetry.temporary_high_water.max(context.reserved_temporary_bytes());
    batch.clear();
    Ok(())
}

fn encode_cell(cell: &CellToken, output: &mut Vec<u8>) -> Result<(), ConversionError> {
    output.extend_from_slice(&cell.coordinate.0.to_le_bytes());
    output.extend_from_slice(&cell.coordinate.1.to_le_bytes());
    match &cell.value {
        CellValueToken::Shared(index) => {
            output.push(1);
            output.extend_from_slice(&index.to_le_bytes());
        }
        CellValueToken::Raw(value) => {
            output.push(0);
            encode_string(value, output)?;
        }
    }
    encode_string(&cell.formula, output)?;
    encode_string(&cell.cell_type, output)?;
    match cell.style_index {
        Some(index) => {
            output.push(1);
            output.extend_from_slice(&index.to_le_bytes());
        }
        None => output.push(0),
    }
    Ok(())
}

fn encode_string(value: &str, output: &mut Vec<u8>) -> Result<(), ConversionError> {
    let length = u32::try_from(value.len())
        .map_err(|_| limit("max_field_bytes", "staged string is too large"))?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn decode_cell(reader: &mut impl Read) -> Result<CellToken, ConversionError> {
    let row = read_u32(reader)?;
    let column = read_u32(reader)?;
    let value = match read_u8(reader)? {
        0 => CellValueToken::Raw(read_string(reader)?),
        1 => CellValueToken::Shared(read_u64(reader)?),
        _ => return Err(malformed(None, "invalid staged cell value tag")),
    };
    let formula = read_string(reader)?;
    let cell_type = read_string(reader)?;
    let style_index = match read_u8(reader)? {
        0 => None,
        1 => Some(read_u64(reader)?),
        _ => return Err(malformed(None, "invalid staged style tag")),
    };
    Ok(CellToken { coordinate: (row, column), value, formula, cell_type, style_index })
}

fn read_u8(reader: &mut impl Read) -> Result<u8, ConversionError> {
    let mut bytes = [0_u8; 1];
    reader.read_exact(&mut bytes).map_err(ConversionError::from)?;
    Ok(bytes[0])
}

fn read_u32(reader: &mut impl Read) -> Result<u32, ConversionError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes).map_err(ConversionError::from)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> Result<u64, ConversionError> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes).map_err(ConversionError::from)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_string(reader: &mut impl Read) -> Result<String, ConversionError> {
    let length = usize::try_from(read_u32(reader)?).unwrap_or(usize::MAX);
    let mut bytes = vec![0_u8; length];
    reader.read_exact(&mut bytes).map_err(ConversionError::from)?;
    String::from_utf8(bytes).map_err(|_| malformed(None, "staged cell text is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::stage;
    use crate::workbook::xlsx::sheet_index::{CellToken, CellValueToken};
    use into_markdown_core::{ExecutionContext, ExecutionOptions, ResourceLimits};

    #[test]
    fn batches_writes_and_releases_temporary_budget() {
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let cells = (0..16_384)
            .map(|index| CellToken {
                coordinate: (index / 128, index % 128),
                value: CellValueToken::Raw(index.to_string()),
                formula: String::new(),
                cell_type: "n".into(),
                style_index: None,
            })
            .collect::<Vec<_>>();
        let staged = stage(&cells, &context).unwrap();
        let (decoded, telemetry) = staged.read_all(&context).unwrap();
        assert_eq!(decoded, cells);
        assert!(telemetry.writes < 512);
        assert_eq!(telemetry.seeks, 1);
        assert_eq!(context.reserved_temporary_bytes(), 0);
    }
}

use crate::workbook::error::{limit, malformed};
use crate::workbook::xlsx::sheet_index::{CellToken, CellValueToken};
use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation, TemporaryFile};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

const WRITE_BATCH_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct StagingTelemetry {
    pub(super) data_passes: u64,
    pub(super) writes: u64,
    pub(super) flushes: u64,
    pub(super) reads: u64,
    pub(super) seeks: u64,
    pub(super) staged_bytes: u64,
    pub(super) temporary_high_water: u64,
}

pub(super) struct StagedCells {
    storage: CellStorage,
    count: u64,
    telemetry: Arc<Mutex<StagingTelemetry>>,
}

pub(super) struct StagingWriter {
    storage: CellStorage,
    batch: Vec<u8>,
    count: u64,
    telemetry: StagingTelemetry,
}

pub(super) struct StagedReader {
    source: CellSource,
    remaining: u64,
    telemetry: Arc<Mutex<StagingTelemetry>>,
}

enum CellStorage {
    File(TemporaryFile),
    Memory { cells: Vec<CellToken>, reservation: ResourceReservation },
}

enum CellSource {
    File { reader: BufReader<std::fs::File>, _owner: TemporaryFile },
    Memory { cells: std::vec::IntoIter<CellToken>, _reservation: ResourceReservation },
}

impl StagingWriter {
    pub(super) fn new(
        context: &ExecutionContext,
        populated_cells: u64,
    ) -> Result<Self, ConversionError> {
        let (storage, batch) = match context.temporary_file("into-md-xlsx-cells") {
            Ok(file) => (CellStorage::File(file), Vec::with_capacity(WRITE_BATCH_BYTES)),
            Err(ConversionError::Io { .. }) => {
                let count = usize::try_from(populated_cells).map_err(|_| {
                    limit("max_memory_bytes", "worksheet cell count does not fit in memory")
                })?;
                let bytes = populated_cells
                    .checked_mul(size_of::<CellToken>() as u64)
                    .ok_or_else(|| limit("max_memory_bytes", "worksheet staging size overflow"))?;
                let mut reservation = context.reserve_memory(bytes)?;
                let mut cells = Vec::new();
                cells.try_reserve_exact(count).map_err(|_| {
                    limit("max_memory_bytes", "cannot reserve resident worksheet cells")
                })?;
                reservation.grow(((cells.capacity() - count) * size_of::<CellToken>()) as u64)?;
                (CellStorage::Memory { cells, reservation }, Vec::new())
            }
            Err(error) => return Err(error),
        };
        Ok(Self { storage, batch, count: 0, telemetry: StagingTelemetry::default() })
    }

    pub(super) fn push(
        &mut self,
        cell: CellToken,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        match &mut self.storage {
            CellStorage::File(file) => {
                encode_cell(&cell, &mut self.batch)?;
                if self.batch.len() >= WRITE_BATCH_BYTES {
                    flush_batch(file, &mut self.batch, &mut self.telemetry, context)?;
                }
            }
            CellStorage::Memory { cells, reservation } => {
                if cells.len() == cells.capacity() {
                    return Err(malformed(
                        None,
                        "worksheet data exceeds the staged cell allocation",
                    ));
                }
                let value_bytes = match &cell.value {
                    CellValueToken::Raw(value) => value.capacity(),
                    CellValueToken::Shared(_) => 0,
                };
                reservation.grow(
                    (value_bytes + cell.formula.capacity() + cell.cell_type.capacity()) as u64,
                )?;
                cells.push(cell);
            }
        }
        self.count = self.count.saturating_add(1);
        Ok(())
    }

    pub(super) fn finish(
        mut self,
        context: &ExecutionContext,
    ) -> Result<StagedCells, ConversionError> {
        if let CellStorage::File(file) = &mut self.storage {
            flush_batch(file, &mut self.batch, &mut self.telemetry, context)?;
            file.flush()?;
            self.telemetry.flushes = self.telemetry.flushes.saturating_add(1);
        }
        self.telemetry.data_passes = self.telemetry.data_passes.saturating_add(1);
        Ok(StagedCells {
            storage: self.storage,
            count: self.count,
            telemetry: Arc::new(Mutex::new(self.telemetry)),
        })
    }
}

#[cfg(test)]
pub(super) fn stage(
    cells: &[CellToken],
    context: &ExecutionContext,
) -> Result<StagedCells, ConversionError> {
    let mut writer = StagingWriter::new(context, cells.len() as u64)?;
    for cell in cells {
        context.checkpoint()?;
        writer.push(cell.clone(), context)?;
    }
    writer.finish(context)
}

impl StagedCells {
    pub(super) fn telemetry_handle(&self) -> Arc<Mutex<StagingTelemetry>> {
        Arc::clone(&self.telemetry)
    }

    pub(super) fn into_reader(self) -> Result<StagedReader, ConversionError> {
        let source = match self.storage {
            CellStorage::File(mut file) => {
                file.seek(SeekFrom::Start(0))?;
                self.telemetry
                    .lock()
                    .map_err(|_| malformed(None, "staging telemetry lock is poisoned"))?
                    .seeks += 1;
                let reader = BufReader::new(file.as_file()?.try_clone()?);
                CellSource::File { reader, _owner: file }
            }
            CellStorage::Memory { cells, reservation } => {
                CellSource::Memory { cells: cells.into_iter(), _reservation: reservation }
            }
        };
        Ok(StagedReader { source, remaining: self.count, telemetry: self.telemetry })
    }

    #[cfg(test)]
    pub(super) fn read_all(
        self,
        context: &ExecutionContext,
    ) -> Result<(Vec<CellToken>, StagingTelemetry), ConversionError> {
        let mut reader = self.into_reader()?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(usize::try_from(reader.remaining).unwrap_or(usize::MAX))
            .map_err(|_| limit("max_memory_bytes", "cannot reserve staged worksheet cells"))?;
        while let Some(cell) = reader.next()? {
            context.checkpoint()?;
            output.push(cell);
        }
        let telemetry = *reader
            .telemetry
            .lock()
            .map_err(|_| malformed(None, "staging telemetry lock is poisoned"))?;
        Ok((output, telemetry))
    }
}

impl StagedReader {
    pub(super) fn next(&mut self) -> Result<Option<CellToken>, ConversionError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let cell = match &mut self.source {
            CellSource::File { reader, .. } => decode_cell(reader)?,
            CellSource::Memory { cells, .. } => {
                cells.next().ok_or_else(|| malformed(None, "staged worksheet ended early"))?
            }
        };
        self.remaining -= 1;
        self.telemetry
            .lock()
            .map_err(|_| malformed(None, "staging telemetry lock is poisoned"))?
            .reads += 1;
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
    use super::{StagingWriter, stage};
    use crate::workbook::xlsx::sheet_index::{CellToken, CellValueToken};
    use into_markdown_core::{ConversionError, ExecutionContext, ExecutionOptions, ResourceLimits};

    #[test]
    fn resident_staging_owns_its_budget_through_readback_and_releases_it_on_error() {
        let token = || CellToken {
            coordinate: (1, 1),
            value: CellValueToken::Raw(String::with_capacity(200)),
            formula: "SUM(A1)".into(),
            cell_type: "n".into(),
            style_index: None,
        };
        let required = (size_of::<CellToken>() + 200 + "SUM(A1)".len() + 1) as u64;
        let unavailable = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        for budget in [required, required - 1] {
            let context = ExecutionContext::new_with_temporary_directory(
                ExecutionOptions::default(),
                ResourceLimits { max_memory_bytes: budget, ..Default::default() },
                unavailable.clone(),
            );
            let mut writer = StagingWriter::new(&context, 1).unwrap();
            let result = writer.push(token(), &context);
            if budget == required {
                result.unwrap();
                let staged = writer.finish(&context).unwrap();
                assert_eq!(context.reserved_memory_bytes(), required);
                let mut reader = staged.into_reader().unwrap();
                assert_eq!(reader.next().unwrap(), Some(token()));
                assert_eq!(context.reserved_memory_bytes(), required);
                assert_eq!(reader.next().unwrap(), None);
            } else {
                assert!(matches!(
                    result,
                    Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
                ));
                drop(writer);
            }
            assert_eq!(context.reserved_memory_bytes(), 0);
            assert_eq!(context.reserved_temporary_bytes(), 0);
        }
    }

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
        assert_eq!(telemetry.data_passes, 1);
        assert_eq!(telemetry.reads, 16_384);
        assert!(telemetry.writes < 512);
        assert_eq!(telemetry.seeks, 1);
        assert_eq!(context.reserved_temporary_bytes(), 0);
    }
}

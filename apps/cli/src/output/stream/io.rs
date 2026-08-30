//! Accounted file replay and serialization writers.

use super::*;

pub(super) struct IndentingWriter<W> {
    inner: W,
    indent: &'static [u8],
}

impl<W> IndentingWriter<W> {
    pub(super) const fn new(inner: W, indent: &'static [u8]) -> Self {
        Self { inner, indent }
    }
}

impl<W: Write> Write for IndentingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let mut start = 0;
        for (index, byte) in bytes.iter().copied().enumerate() {
            if byte == b'\n' {
                self.inner.write_all(&bytes[start..=index])?;
                self.inner.write_all(self.indent)?;
                start = index + 1;
            }
        }
        self.inner.write_all(&bytes[start..])
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub(super) fn copy_spool<W: Write>(
    context: &ExecutionContext,
    spool: &TemporaryFile,
    destination: &mut W,
) -> Result<(), CliError> {
    let mut reader = spool.as_file().map_err(CliError::from)?.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    copy_reader(context, &mut reader, destination)
}

pub(super) struct ChunkRecordingWriter<'a> {
    pub(super) destination: &'a mut TemporaryFile,
    pub(super) chunks: &'a mut Vec<usize>,
    pub(super) lease: &'a mut ResourceReservation,
}

impl Write for ChunkRecordingWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.reserve_slot()?;
        match self.destination.write(bytes) {
            Ok(written) => {
                self.chunks.push(written);
                Ok(written)
            }
            Err(error) => Err(error),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Write::flush(self.destination)
    }
}

impl ChunkRecordingWriter<'_> {
    fn reserve_slot(&mut self) -> std::io::Result<()> {
        if self.chunks.len() < self.chunks.capacity() {
            return Ok(());
        }
        let old_capacity = self.chunks.capacity();
        let target = old_capacity.saturating_mul(2).max(64);
        let slot_bytes = std::mem::size_of::<usize>();
        let planned_bytes = target
            .checked_sub(old_capacity)
            .and_then(|slots| slots.checked_mul(slot_bytes))
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(|| std::io::Error::other("IR replay index capacity overflowed"))?;
        self.lease.grow(planned_bytes).map_err(std::io::Error::other)?;
        if let Err(error) = self.chunks.try_reserve_exact(target - self.chunks.len()) {
            self.lease.shrink(planned_bytes).map_err(std::io::Error::other)?;
            return Err(std::io::Error::other(error));
        }
        let actual_capacity = self.chunks.capacity();
        if actual_capacity < target {
            *self.chunks = Vec::new();
            let target_bytes = target
                .checked_mul(slot_bytes)
                .and_then(|bytes| u64::try_from(bytes).ok())
                .ok_or_else(|| std::io::Error::other("IR replay index capacity overflowed"))?;
            self.lease.shrink(target_bytes).map_err(std::io::Error::other)?;
            return Err(std::io::Error::other(
                "IR replay index reserve returned less than requested capacity",
            ));
        }
        if actual_capacity > target {
            let extra_bytes = actual_capacity
                .checked_sub(target)
                .and_then(|slots| slots.checked_mul(slot_bytes))
                .and_then(|bytes| u64::try_from(bytes).ok())
                .ok_or_else(|| std::io::Error::other("IR replay index capacity overflowed"))?;
            if let Err(error) = self.lease.grow(extra_bytes) {
                *self.chunks = Vec::new();
                let target_bytes = target
                    .checked_mul(slot_bytes)
                    .and_then(|bytes| u64::try_from(bytes).ok())
                    .ok_or_else(|| std::io::Error::other("IR replay index capacity overflowed"))?;
                self.lease.shrink(target_bytes).map_err(std::io::Error::other)?;
                return Err(std::io::Error::other(error));
            }
        }
        Ok(())
    }
}

pub(super) fn replay_spool_chunks<W: Write>(
    context: &ExecutionContext,
    spool: &TemporaryFile,
    chunks: &[usize],
    destination: &mut W,
) -> Result<(), CliError> {
    let maximum = chunks.iter().copied().max().unwrap_or(0);
    let memory = context
        .reserve_memory(u64::try_from(maximum).unwrap_or(u64::MAX))
        .map_err(CliError::from)?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(maximum)
        .map_err(|error| CliError::internal(format!("reserve IR replay buffer: {error}")))?;
    buffer.resize(maximum, 0);
    let mut reader = spool.as_file().map_err(CliError::from)?.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    for &length in chunks {
        context.checkpoint().map_err(CliError::from)?;
        reader.read_exact(&mut buffer[..length])?;
        destination.write_all(&buffer[..length])?;
    }
    drop(memory);
    Ok(())
}

pub(super) fn copy_reader<R: Read, W: Write>(
    context: &ExecutionContext,
    reader: &mut R,
    destination: &mut W,
) -> Result<(), CliError> {
    let _buffer_lease = context
        .reserve_memory(u64::try_from(COPY_BUFFER_BYTES).unwrap_or(u64::MAX))
        .map_err(CliError::from)?;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES].into_boxed_slice();
    loop {
        context.checkpoint().map_err(CliError::from)?;
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        destination.write_all(&buffer[..read])?;
    }
}

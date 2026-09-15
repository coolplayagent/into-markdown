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

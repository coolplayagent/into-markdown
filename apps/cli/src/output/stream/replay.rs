//! Replay completed artifacts without requiring writable scratch storage.
use super::*;

pub(crate) enum ReplaySpool {
    File(TemporaryFile),
    Memory {
        data: std::io::Cursor<Vec<u8>>,
        memory: ResourceReservation,
        context: ExecutionContext,
    },
}

impl ReplaySpool {
    pub(in crate::output) fn new(
        context: &ExecutionContext,
        prefix: &str,
    ) -> Result<Self, CliError> {
        match context.temporary_file(prefix) {
            Ok(file) => Ok(Self::File(file)),
            Err(ConversionError::Io { .. }) => Ok(Self::Memory {
                data: Default::default(),
                context: context.clone(),
                memory: context.reserve_memory(0).map_err(CliError::from)?,
            }),
            Err(error) => Err(error.into()),
        }
    }

    pub(in crate::output) fn write_all_checked(
        &mut self,
        chunk: &[u8],
    ) -> Result<(), ConversionError> {
        match self {
            Self::File(file) => file.write_all_checked(chunk),
            Self::Memory { data, memory, context } => {
                context.checkpoint()?;
                let position = usize::try_from(data.position()).map_err(|_| memory_limit())?;
                let required = position.checked_add(chunk.len()).ok_or_else(memory_limit)?;
                let bytes = data.get_mut();
                if required > bytes.capacity() {
                    let preferred = required.max(bytes.capacity().saturating_mul(2));
                    let reserve = |memory: &mut ResourceReservation, target: usize| {
                        memory.grow((target - bytes.capacity()) as u64)
                    };
                    let target = if preferred != required && reserve(memory, preferred).is_ok() {
                        preferred
                    } else {
                        reserve(memory, required)?;
                        required
                    };
                    bytes.try_reserve_exact(target - bytes.len()).map_err(|_| memory_limit())?;
                    if bytes.capacity() > target {
                        memory.grow((bytes.capacity() - target) as u64)?;
                    }
                }
                data.write_all(chunk).map_err(Into::into)
            }
        }
    }

    pub(super) fn reader(&self) -> Result<Box<dyn Read + '_>, CliError> {
        match self {
            Self::File(file) => {
                let mut reader = file.as_file().map_err(CliError::from)?.try_clone()?;
                reader.rewind()?;
                Ok(Box::new(reader))
            }
            Self::Memory { data, .. } => Ok(Box::new(data.get_ref().as_slice())),
        }
    }

    pub(super) fn len(&self) -> Result<u64, CliError> {
        match self {
            Self::File(file) => Ok(file.as_file().map_err(CliError::from)?.metadata()?.len()),
            Self::Memory { data, .. } => Ok(data.get_ref().len() as u64),
        }
    }

    pub(in crate::output) fn sync_all(&mut self) -> Result<(), CliError> {
        if let Self::File(file) = self {
            file.sync_all().map_err(CliError::from)?;
        }
        Ok(())
    }

    pub(in crate::output) fn transaction_content(
        &self,
    ) -> Result<crate::transaction::MixedContent<'_>, CliError> {
        use crate::transaction::MixedContent;
        match self {
            Self::File(file) => Ok(MixedContent::File(file.as_file()?)),
            Self::Memory { data, .. } => Ok(MixedContent::Bytes(data.get_ref())),
        }
    }

    pub(crate) fn copy_to<W: Write + ?Sized>(
        &self,
        context: &ExecutionContext,
        destination: &mut W,
    ) -> Result<(), CliError> {
        match self {
            Self::File(file) => copy_spool(context, file, destination),
            Self::Memory { data, .. } => {
                context.checkpoint().map_err(CliError::from)?;
                for chunk in data.get_ref().chunks(COPY_BUFFER_BYTES) {
                    context.checkpoint().map_err(CliError::from)?;
                    destination.write_all(chunk)?;
                }
                Ok(())
            }
        }
    }
}

impl Write for ReplaySpool {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.write_all_checked(bytes).map_err(std::io::Error::other)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::File(file) => Write::flush(file),
            Self::Memory { context, .. } => context.checkpoint().map_err(std::io::Error::other),
        }
    }
}

impl Seek for ReplaySpool {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        match self {
            Self::File(file) => file.seek(position),
            Self::Memory { context, data, .. } => {
                context.checkpoint().map_err(std::io::Error::other)?;
                data.seek(position)
            }
        }
    }
}

fn memory_limit() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "could not allocate the artifact replay buffer".into(),
    }
}

impl StructuredSpool {
    pub(crate) fn take_stdout_primary(&mut self) -> Result<ReplaySpool, CliError> {
        if !self.finished {
            return Err(CliError::internal("structured spool is not finalized"));
        }
        if self.plan.emit == EmitKind::Markdown {
            return self
                .markdown
                .take()
                .ok_or_else(|| CliError::internal("raw Markdown representation is absent"));
        }
        if self.plan.emit == EmitKind::IrJson {
            return self
                .ir
                .take()
                .ok_or_else(|| CliError::internal("document IR representation is absent"));
        }
        let mut encoded = ReplaySpool::new(&self.context, "into-md-stdout")?;
        self.serialize(self.plan.emit, &mut encoded)?;
        encoded.sync_all()?;
        Ok(encoded)
    }
}

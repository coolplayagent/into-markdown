use super::{
    CliError, Digest, ExecutionContext, ExitClass, File, Path, PathBuf, Read, Seek, Sha256, Write,
    sha256_hex,
};

pub struct Target<'a> {
    pub path: PathBuf,
    pub bytes: &'a [u8],
}

/// One requested target whose staged contents come from a seekable file.
pub struct FileTarget<'a> {
    pub path: PathBuf,
    pub file: &'a File,
}

/// A single-target transaction whose authenticated stage file is the streaming writer.
///
/// The journal and parent leases are durable before this value is returned. A crash while the
/// stage is being written leaves the journal in `staging`, so recovery removes the incomplete
/// stage without publishing it. Sealing records the final size and digest and advances the same
/// transaction to `prepared`; the payload is never copied into a second staging file.
pub(in crate::transaction) trait TransactionSource {
    fn path(&self) -> &Path;
    fn size_and_sha256(&self, context: &ExecutionContext) -> Result<(u64, String), CliError>;
    fn write_to(&self, destination: &mut File, context: &ExecutionContext) -> Result<(), CliError>;
}

impl TransactionSource for Target<'_> {
    fn path(&self) -> &Path {
        &self.path
    }

    fn size_and_sha256(&self, _: &ExecutionContext) -> Result<(u64, String), CliError> {
        let size = u64::try_from(self.bytes.len()).map_err(|_| {
            CliError::new(ExitClass::Policy, "resourceLimit", "target size cannot be represented")
        })?;
        Ok((size, sha256_hex(self.bytes)))
    }

    fn write_to(&self, destination: &mut File, context: &ExecutionContext) -> Result<(), CliError> {
        context.checkpoint().map_err(CliError::from)?;
        destination.write_all(self.bytes).map_err(CliError::from)
    }
}

impl TransactionSource for FileTarget<'_> {
    fn path(&self) -> &Path {
        &self.path
    }

    fn size_and_sha256(&self, context: &ExecutionContext) -> Result<(u64, String), CliError> {
        let mut source = self.file.try_clone()?;
        source.rewind()?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
        loop {
            context.checkpoint().map_err(CliError::from)?;
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            size = size.checked_add(u64::try_from(read).unwrap_or(u64::MAX)).ok_or_else(|| {
                CliError::new(ExitClass::Policy, "resourceLimit", "target size overflowed")
            })?;
            digest.update(&buffer[..read]);
        }
        Ok((size, format!("{:x}", digest.finalize())))
    }

    fn write_to(&self, destination: &mut File, context: &ExecutionContext) -> Result<(), CliError> {
        let mut source = self.file.try_clone()?;
        source.rewind()?;
        let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
        loop {
            context.checkpoint().map_err(CliError::from)?;
            let read = source.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
            destination.write_all(&buffer[..read])?;
        }
    }
}

#[derive(Clone, Copy)]
pub(in crate::transaction) enum MixedContent<'a> {
    Bytes(&'a [u8]),
    File(&'a File),
}

pub(in crate::transaction) struct MixedTarget<'a> {
    pub(in crate::transaction) path: &'a Path,
    pub(in crate::transaction) content: MixedContent<'a>,
}

impl TransactionSource for MixedTarget<'_> {
    fn path(&self) -> &Path {
        self.path
    }

    fn size_and_sha256(&self, context: &ExecutionContext) -> Result<(u64, String), CliError> {
        match self.content {
            MixedContent::Bytes(bytes) => {
                Target { path: PathBuf::new(), bytes }.size_and_sha256(context)
            }
            MixedContent::File(file) => {
                FileTarget { path: PathBuf::new(), file }.size_and_sha256(context)
            }
        }
    }

    fn write_to(&self, destination: &mut File, context: &ExecutionContext) -> Result<(), CliError> {
        match self.content {
            MixedContent::Bytes(bytes) => {
                Target { path: PathBuf::new(), bytes }.write_to(destination, context)
            }
            MixedContent::File(file) => {
                FileTarget { path: PathBuf::new(), file }.write_to(destination, context)
            }
        }
    }
}

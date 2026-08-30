use super::*;

impl StructuredSpool {
    pub(super) fn write_result_json<W: Write>(&self, destination: &mut W) -> Result<(), CliError> {
        let markdown_json = self
            .markdown_json
            .as_ref()
            .ok_or_else(|| CliError::internal("escaped Markdown representation is absent"))?;
        let ir = self
            .ir
            .as_ref()
            .ok_or_else(|| CliError::internal("document IR representation is absent"))?;
        let diagnostics = self
            .diagnostics
            .as_ref()
            .ok_or_else(|| CliError::internal("diagnostics representation is absent"))?;
        let provenance = self
            .provenance
            .as_ref()
            .ok_or_else(|| CliError::internal("provenance representation is absent"))?;
        destination.write_all(b"{\n  \"schemaVersion\": 1,\n  \"markdown\": ")?;
        copy_spool(&self.context, &markdown_json.file, destination)?;
        destination.write_all(b",\n  \"document\": ")?;
        copy_spool_indented(&self.context, ir, destination, b"  ", true)?;
        destination.write_all(b"  \"assets\": [")?;
        for (index, asset) in self.asset_records.iter().enumerate() {
            if index == 0 {
                destination.write_all(b"\n")?;
            } else {
                destination.write_all(b",\n")?;
            }
            destination.write_all(b"    {\n      \"id\": ")?;
            write_json_value(destination, &asset.id)?;
            destination.write_all(b",\n      \"filename\": ")?;
            write_json_value(destination, &asset.wire_filename)?;
            destination.write_all(b",\n      \"mediaType\": ")?;
            write_json_value(destination, &asset.media_type)?;
            destination.write_all(b",\n      \"dataBase64\": \"")?;
            if let Some(payload_index) = asset.payload_index {
                self.write_payload_base64(payload_index, destination)?;
            }
            destination.write_all(b"\",\n      \"externalUri\": ")?;
            write_json_value(destination, &asset.external_uri)?;
            destination.write_all(b"\n    }")?;
        }
        if self.asset_records.is_empty() {
            destination.write_all(b"],\n  \"diagnostics\": ")?;
        } else {
            destination.write_all(b"\n  ],\n  \"diagnostics\": ")?;
        }
        copy_spool_indented(&self.context, diagnostics, destination, b"  ", true)?;
        destination.write_all(b"  \"provenance\": ")?;
        copy_spool_indented(&self.context, provenance, destination, b"  ", false)?;
        destination.write_all(b"}\n")?;
        Ok(())
    }

    fn write_payload_base64<W: Write>(
        &self,
        payload_index: usize,
        destination: &mut W,
    ) -> Result<(), CliError> {
        let payload = self
            .payload_records
            .get(payload_index)
            .ok_or_else(|| CliError::internal("asset payload index is inconsistent"))?;
        let mut reader = payload.file.as_file().map_err(CliError::from)?.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        let mut limited = reader.take(payload.size);
        let mut encoder = base64::write::EncoderWriter::new(destination, &STANDARD);
        copy_reader(&self.context, &mut limited, &mut encoder)?;
        encoder.finish()?;
        Ok(())
    }
}

fn write_json_value<W: Write, T: Serialize + ?Sized>(
    destination: &mut W,
    value: &T,
) -> Result<(), CliError> {
    serde_json::to_writer(destination, value)
        .map_err(|error| map_json_error(&error, "serialize result JSON field"))
}

fn copy_spool_indented<W: Write>(
    context: &ExecutionContext,
    spool: &TemporaryFile,
    destination: &mut W,
    indent: &[u8],
    trailing_comma: bool,
) -> Result<(), CliError> {
    let mut reader = spool.as_file().map_err(CliError::from)?.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let length = reader.metadata()?.len().saturating_sub(1);
    let mut reader = reader.take(length);
    let _buffer_lease = context
        .reserve_memory(u64::try_from(COPY_BUFFER_BYTES).unwrap_or(u64::MAX))
        .map_err(CliError::from)?;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES].into_boxed_slice();
    loop {
        context.checkpoint().map_err(CliError::from)?;
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let mut start = 0;
        for (index, byte) in buffer[..read].iter().copied().enumerate() {
            if byte == b'\n' {
                destination.write_all(&buffer[start..=index])?;
                destination.write_all(indent)?;
                start = index + 1;
            }
        }
        if start < read {
            destination.write_all(&buffer[start..read])?;
        }
    }
    if trailing_comma {
        destination.write_all(b",\n")?;
    } else {
        destination.write_all(b"\n")?;
    }
    Ok(())
}

pub(super) fn map_json_error(error: &serde_json::Error, operation: &str) -> CliError {
    if error.is_io() {
        let mut source = std::error::Error::source(&error);
        while let Some(current) = source {
            if let Some(io) = current.downcast_ref::<std::io::Error>() {
                if io.kind() == std::io::ErrorKind::BrokenPipe {
                    return CliError::from(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        io.to_string(),
                    ));
                }
                if let Some(conversion) =
                    io.get_ref().and_then(|inner| inner.downcast_ref::<ConversionError>())
                {
                    return CliError::from(conversion.clone());
                }
                return CliError::from(std::io::Error::new(io.kind(), io.to_string()));
            }
            source = current.source();
        }
    }
    CliError::internal(format!("{operation}: {error}"))
}

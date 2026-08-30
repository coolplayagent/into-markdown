use super::{CliError, ExecutionContext, TemporaryFile};

pub(super) struct JsonStringSpool {
    pub(super) file: TemporaryFile,
    carry: [u8; 3],
    carry_len: usize,
    finished: bool,
}

impl JsonStringSpool {
    pub(super) fn new(context: &ExecutionContext, prefix: &str) -> Result<Self, CliError> {
        let mut file = context.temporary_file(prefix).map_err(CliError::from)?;
        file.write_all_checked(b"\"").map_err(CliError::from)?;
        Ok(Self { file, carry: [0; 3], carry_len: 0, finished: false })
    }

    pub(super) fn write(&mut self, chunk: &[u8]) -> Result<(), CliError> {
        if self.finished {
            return Err(CliError::internal("JSON string spool was already finished"));
        }
        if self.carry_len == 0 {
            return self.write_valid_prefix(chunk);
        }
        let width = utf8_width(self.carry[0])
            .ok_or_else(|| CliError::internal("JSON string spool has invalid UTF-8 state"))?;
        let needed = width.saturating_sub(self.carry_len);
        if chunk.len() < needed {
            let end = self.carry_len + chunk.len();
            self.carry[self.carry_len..end].copy_from_slice(chunk);
            self.carry_len = end;
            return Ok(());
        }
        let mut sequence = [0_u8; 4];
        sequence[..self.carry_len].copy_from_slice(&self.carry[..self.carry_len]);
        sequence[self.carry_len..width].copy_from_slice(&chunk[..needed]);
        std::str::from_utf8(&sequence[..width])
            .map_err(|_| CliError::internal("streamed Markdown is not valid UTF-8"))?;
        self.file.write_all_checked(&sequence[..width]).map_err(CliError::from)?;
        self.carry_len = 0;
        self.write_valid_prefix(&chunk[needed..])
    }

    fn write_valid_prefix(&mut self, chunk: &[u8]) -> Result<(), CliError> {
        match std::str::from_utf8(chunk) {
            Ok(_) => write_json_string_bytes(&mut self.file, chunk),
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                write_json_string_bytes(&mut self.file, &chunk[..valid])?;
                let tail = &chunk[valid..];
                if tail.len() > self.carry.len()
                    || tail.first().copied().and_then(utf8_width).is_none()
                {
                    return Err(CliError::internal("streamed Markdown is not valid UTF-8"));
                }
                self.carry[..tail.len()].copy_from_slice(tail);
                self.carry_len = tail.len();
                Ok(())
            }
            Err(_) => Err(CliError::internal("streamed Markdown is not valid UTF-8")),
        }
    }

    pub(super) fn finish(&mut self) -> Result<(), CliError> {
        if self.finished {
            return Ok(());
        }
        if self.carry_len != 0 {
            return Err(CliError::internal("streamed Markdown ended within a UTF-8 sequence"));
        }
        self.file.write_all_checked(b"\"").map_err(CliError::from)?;
        self.finished = true;
        Ok(())
    }
}

fn write_json_string_bytes(file: &mut TemporaryFile, bytes: &[u8]) -> Result<(), CliError> {
    let mut start = 0;
    for (index, byte) in bytes.iter().copied().enumerate() {
        let escaped: Option<&[u8]> = match byte {
            b'"' => Some(b"\\\""),
            b'\\' => Some(b"\\\\"),
            0x08 => Some(b"\\b"),
            b'\t' => Some(b"\\t"),
            b'\n' => Some(b"\\n"),
            0x0c => Some(b"\\f"),
            b'\r' => Some(b"\\r"),
            0x00..=0x1f => None,
            _ => continue,
        };
        if index > start {
            file.write_all_checked(&bytes[start..index]).map_err(CliError::from)?;
        }
        if let Some(escaped) = escaped {
            file.write_all_checked(escaped).map_err(CliError::from)?;
        } else {
            let encoded = [b'\\', b'u', b'0', b'0', hex_digit(byte >> 4), hex_digit(byte & 0x0f)];
            file.write_all_checked(&encoded).map_err(CliError::from)?;
        }
        start = index + 1;
    }
    if start < bytes.len() {
        file.write_all_checked(&bytes[start..]).map_err(CliError::from)?;
    }
    Ok(())
}

const fn hex_digit(value: u8) -> u8 {
    b"0123456789abcdef"[value as usize]
}

const fn utf8_width(first: u8) -> Option<usize> {
    match first {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

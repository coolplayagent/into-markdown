//! Checked token reads. Unsupported syntax is retained, never guessed or evaluated.

pub(super) type Result<T> = std::result::Result<T, &'static str>;

pub(super) struct Tokens<'a> {
    bytes: &'a [u8],
    pub(super) position: usize,
}

impl<'a> Tokens<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub(super) fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    pub(super) fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self.position.checked_add(count).ok_or("invalid-token-length")?;
        let value = self.bytes.get(self.position..end).ok_or("truncated-token")?;
        self.position = end;
        Ok(value)
    }

    pub(super) fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(super) fn word(&mut self) -> Result<u16> {
        let value = self.take(2)?;
        Ok(u16::from_le_bytes([value[0], value[1]]))
    }

    pub(super) fn string(&mut self, biff8: bool) -> Result<String> {
        let count = usize::from(self.byte()?);
        let flags = if biff8 { self.byte()? } else { 0 };
        let value = match flags {
            0 => {
                let bytes = self.take(count)?;
                if !biff8 && !bytes.is_ascii() {
                    return Err("legacy-string-codepage");
                }
                bytes.iter().map(|byte| char::from(*byte)).collect::<String>()
            }
            1 => {
                let units = self
                    .take(count * 2)?
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect::<Vec<_>>();
                String::from_utf16(&units).map_err(|_| "invalid-string-encoding")?
            }
            _ => return Err("unsupported-string-flags"),
        };
        Ok(format!("\"{}\"", value.replace('"', "\"\"")))
    }
}

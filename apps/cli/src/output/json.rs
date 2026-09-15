//! Fixed-size writes keep JSON compression independent of serializer fragments.

use std::io::{self, Write};

const CHUNK_BYTES: usize = 64 * 1024;

pub(crate) struct ChunkWriter<W> {
    destination: W,
    buffer: Vec<u8>,
}

impl<W: Write> ChunkWriter<W> {
    pub(crate) fn new(destination: W) -> io::Result<Self> {
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(CHUNK_BYTES).map_err(io::Error::other)?;
        Ok(Self { destination, buffer })
    }

    fn drain(&mut self) -> io::Result<()> {
        self.destination.write_all(&self.buffer)?;
        self.buffer.clear();
        Ok(())
    }

    pub(crate) fn finish(mut self) -> io::Result<()> {
        // The owner finalizes the surrounding compression stream or file.
        self.drain()
    }
}

impl<W: Write> Write for ChunkWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(CHUNK_BYTES - self.buffer.len());
        self.buffer.extend_from_slice(&bytes[..count]);
        if self.buffer.len() == CHUNK_BYTES {
            self.drain()?;
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.drain()?;
        self.destination.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Writes(Vec<Vec<u8>>);
    impl Write for Writes {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.push(bytes.to_vec());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("JSON completion must preserve the surrounding compressor state")
        }
    }

    #[test]
    fn fragment_boundaries_do_not_change_compression_input_or_content() {
        let source = vec![b'x'; CHUNK_BYTES * 2 + 19];
        let mut whole = Writes::default();
        let mut writer = ChunkWriter::new(&mut whole).unwrap();
        writer.write_all(&source).unwrap();
        writer.finish().unwrap();
        let mut fragmented = Writes::default();
        let mut writer = ChunkWriter::new(&mut fragmented).unwrap();
        for fragment in source.chunks(137) {
            writer.write_all(fragment).unwrap();
        }
        writer.finish().unwrap();
        assert_eq!(whole.0, fragmented.0);
        assert_eq!(whole.0.concat(), source);
        assert_eq!(
            whole.0.iter().map(Vec::len).collect::<Vec<_>>(),
            [CHUNK_BYTES, CHUNK_BYTES, 19]
        );
    }
}

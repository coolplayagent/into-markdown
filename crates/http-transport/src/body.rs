use super::{
    Connection, ContentEncoding, ExecutionContext, FetchLimits, Framing, GzDecoder, IO_CHUNK_BYTES,
    Instant, RawResponse, ResourceReservation, TransportError, TransportErrorKind, WireBody,
    check_operation, map_context_error, parse_head, read_checked, read_head,
};
use std::io::{self, Read};
use std::sync::Arc;

pub(super) struct ChunkNode {
    used: usize,
    bytes: [u8; IO_CHUNK_BYTES],
    next: Option<Box<ChunkNode>>,
}

#[derive(Default)]
pub(super) struct ChunkChain {
    head: Option<Box<ChunkNode>>,
    len: usize,
    reserved_bytes: u64,
}

impl Drop for ChunkChain {
    fn drop(&mut self) {
        // Large transfers chain tens of thousands of 8 KiB nodes; the
        // compiler-generated recursive drop would overflow the stack.
        let mut current = self.head.take();
        while let Some(mut node) = current {
            current = node.next.take();
            drop(node);
        }
    }
}

impl ChunkChain {
    fn push(
        &mut self,
        bytes: &[u8],
        budget: &mut ResourceReservation,
    ) -> Result<(), TransportError> {
        let next_len = self
            .len
            .checked_add(bytes.len())
            .ok_or_else(|| TransportError::new(TransportErrorKind::ResourceLimit))?;
        let mut remaining = bytes;
        // Coalesce into the head node's unused capacity so that a proxy
        // forwarding many small chunks does not waste a full 8 KiB node
        // per chunk and inflate the memory budget past its limit.
        if !remaining.is_empty() {
            if let Some(node) = self.head.as_deref_mut() {
                let space = IO_CHUNK_BYTES - node.used;
                if space > 0 {
                    let take = remaining.len().min(space);
                    node.bytes[node.used..node.used + take].copy_from_slice(&remaining[..take]);
                    node.used += take;
                    remaining = &remaining[take..];
                }
            }
        }
        for part in remaining.chunks(IO_CHUNK_BYTES) {
            let node_bytes = u64::try_from(std::mem::size_of::<ChunkNode>())
                .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
            budget.grow(node_bytes).map_err(map_context_error)?;
            self.reserved_bytes = self
                .reserved_bytes
                .checked_add(node_bytes)
                .ok_or_else(|| TransportError::new(TransportErrorKind::ResourceLimit))?;
            let mut node =
                Box::new(ChunkNode { used: part.len(), bytes: [0_u8; IO_CHUNK_BYTES], next: None });
            node.bytes[..part.len()].copy_from_slice(part);
            node.next = self.head.take();
            self.head = Some(node);
        }
        self.len = next_len;
        Ok(())
    }

    const fn reserved_bytes(&self) -> u64 {
        self.reserved_bytes
    }

    fn copy_to(&self, output: &mut [u8]) -> Result<(), TransportError> {
        if output.len() != self.len {
            return Err(TransportError::new(TransportErrorKind::ResourceLimit));
        }
        let mut offset = self.len;
        let mut current = self.head.as_deref();
        while let Some(node) = current {
            offset = offset
                .checked_sub(node.used)
                .ok_or_else(|| TransportError::new(TransportErrorKind::ResourceLimit))?;
            output[offset..offset + node.used].copy_from_slice(&node.bytes[..node.used]);
            current = node.next.as_deref();
        }
        if offset == 0 {
            Ok(())
        } else {
            Err(TransportError::new(TransportErrorKind::ResourceLimit))
        }
    }
}

pub(super) fn read_response(
    stream: &mut dyn Connection,
    limits: FetchLimits,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<RawResponse, TransportError> {
    let mut memory = context.reserve_memory(0).map_err(map_context_error)?;
    let (head_bytes, head_end) = read_head(stream, context, deadline, &mut memory)?;
    let head = parse_head(&head_bytes[..head_end])?;
    if matches!(head.status, 301 | 302 | 303 | 307 | 308) {
        return Ok(RawResponse {
            status: head.status,
            location: head.location,
            media_type: None,
            filename: None,
            content_encoding: ContentEncoding::Identity,
            body: WireBody {
                chunks: ChunkChain::default(),
                memory: context.reserve_memory(0).map_err(map_context_error)?,
            },
        });
    }
    if head.status != 200 {
        return Ok(RawResponse {
            status: head.status,
            location: None,
            media_type: None,
            filename: None,
            content_encoding: ContentEncoding::Identity,
            body: WireBody {
                chunks: ChunkChain::default(),
                memory: context.reserve_memory(0).map_err(map_context_error)?,
            },
        });
    }
    let wire_limit = usize::try_from(limits.max_wire_bytes)
        .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
    let chunks = match head.framing {
        Framing::Length(length) => {
            let initial_body = &head_bytes[head_end..];
            if length > wire_limit || initial_body.len() > length {
                return Err(TransportError::new(TransportErrorKind::ResourceLimit));
            }
            let mut chain = ChunkChain::default();
            chain.push(initial_body, &mut memory)?;
            let mut remaining = length - initial_body.len();
            while remaining != 0 {
                check_operation(context, deadline)?;
                let mut buffer = [0_u8; IO_CHUNK_BYTES];
                let requested = remaining.min(buffer.len());
                let read = read_checked(stream, &mut buffer[..requested], context, deadline)?;
                if read == 0 {
                    return Err(TransportError::new(TransportErrorKind::InvalidMessage));
                }
                chain.push(&buffer[..read], &mut memory)?;
                remaining -= read;
            }
            chain
        }
        Framing::Close => {
            let initial_body = &head_bytes[head_end..];
            let mut chain = ChunkChain::default();
            if initial_body.len() > wire_limit {
                return Err(TransportError::new(TransportErrorKind::ResourceLimit));
            }
            chain.push(initial_body, &mut memory)?;
            loop {
                check_operation(context, deadline)?;
                let mut buffer = [0_u8; IO_CHUNK_BYTES];
                let read = read_checked(stream, &mut buffer, context, deadline)?;
                if read == 0 {
                    break;
                }
                if chain.len.checked_add(read).is_none_or(|size| size > wire_limit) {
                    return Err(TransportError::new(TransportErrorKind::ResourceLimit));
                }
                chain.push(&buffer[..read], &mut memory)?;
            }
            chain
        }
        Framing::Chunked => read_chunked(
            stream,
            &head_bytes[head_end..],
            wire_limit,
            context,
            deadline,
            &mut memory,
        )?,
    };
    let header_capacity = head_bytes.capacity();
    drop(head_bytes);
    memory
        .shrink(
            u64::try_from(header_capacity)
                .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?,
        )
        .map_err(map_context_error)?;
    Ok(RawResponse {
        status: head.status,
        location: None,
        media_type: head.media_type,
        filename: head.filename,
        content_encoding: head.content_encoding,
        body: WireBody { chunks, memory },
    })
}

pub(super) fn finalize_body(
    body: WireBody,
    encoding: ContentEncoding,
    limits: FetchLimits,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<(Arc<[u8]>, ResourceReservation), TransportError> {
    let WireBody { chunks, mut memory } = body;
    let wire_len = chunks.len;
    let wire_chunk_bytes = chunks.reserved_bytes();
    let wire_len_u64 = u64::try_from(wire_len)
        .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
    memory.grow(wire_len_u64).map_err(map_context_error)?;
    let mut wire = vec![0_u8; wire_len].into_boxed_slice();
    chunks.copy_to(&mut wire)?;
    drop(chunks);
    memory.shrink(wire_chunk_bytes).map_err(map_context_error)?;
    check_operation(context, deadline)?;
    let decoded = match encoding {
        ContentEncoding::Identity => wire,
        ContentEncoding::Gzip => {
            let limit = usize::try_from(limits.max_decoded_bytes)
                .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
            let mut decoder = GzDecoder::new(&*wire);
            let mut chain = ChunkChain::default();
            loop {
                check_operation(context, deadline)?;
                let mut buffer = [0_u8; IO_CHUNK_BYTES];
                let read = decoder
                    .read(&mut buffer)
                    .map_err(|_| TransportError::new(TransportErrorKind::InvalidMessage))?;
                if read == 0 {
                    break;
                }
                if chain.len.checked_add(read).is_none_or(|size| size > limit) {
                    return Err(TransportError::new(TransportErrorKind::ResourceLimit));
                }
                chain.push(&buffer[..read], &mut memory)?;
            }
            drop(decoder);
            drop(wire);
            memory.shrink(wire_len_u64).map_err(map_context_error)?;
            memory
                .grow(
                    u64::try_from(chain.len)
                        .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?,
                )
                .map_err(map_context_error)?;
            let mut output = vec![0_u8; chain.len].into_boxed_slice();
            chain.copy_to(&mut output)?;
            let decoded_chunk_bytes = chain.reserved_bytes();
            drop(chain);
            memory.shrink(decoded_chunk_bytes).map_err(map_context_error)?;
            output
        }
    };
    if u64::try_from(decoded.len()).map_or(true, |length| length > limits.max_decoded_bytes) {
        return Err(TransportError::new(TransportErrorKind::ResourceLimit));
    }
    let final_len = u64::try_from(decoded.len())
        .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
    let reservation = context.reserve_memory(final_len).map_err(map_context_error)?;
    let bytes = Arc::<[u8]>::from(decoded);
    drop(memory);
    Ok((bytes, reservation))
}

pub(super) fn read_chunked(
    stream: &mut dyn Connection,
    initial: &[u8],
    limit: usize,
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut ResourceReservation,
) -> Result<ChunkChain, TransportError> {
    if initial.len() > limit {
        return Err(TransportError::new(TransportErrorKind::ResourceLimit));
    }
    let mut reader = PrefixedReader::new(initial, stream);
    // `initial` was already physically read from the socket together with the
    // header. Charge it once now; consuming the prefix below must not charge it twice.
    let mut wire_bytes = initial.len();
    let mut output = ChunkChain::default();
    loop {
        let line = read_crlf_line(&mut reader, 32, limit, &mut wire_bytes, context, deadline)?;
        if line.is_empty()
            || line.as_slice().contains(&b';')
            || !line.as_slice().iter().all(u8::is_ascii_hexdigit)
        {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
        let text = std::str::from_utf8(line.as_slice())
            .map_err(|_| TransportError::new(TransportErrorKind::InvalidMessage))?;
        let size = usize::from_str_radix(text, 16)
            .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
        if size == 0 {
            if !read_crlf_line(&mut reader, 2, limit, &mut wire_bytes, context, deadline)?
                .is_empty()
            {
                return Err(TransportError::new(TransportErrorKind::InvalidMessage));
            }
            return Ok(output);
        }
        if output.len.checked_add(size).is_none_or(|length| length > limit) {
            return Err(TransportError::new(TransportErrorKind::ResourceLimit));
        }
        let mut remaining = size;
        while remaining != 0 {
            let mut buffer = [0_u8; IO_CHUNK_BYTES];
            let requested = remaining.min(buffer.len());
            let read = read_wire_checked(
                &mut reader,
                &mut buffer[..requested],
                limit,
                &mut wire_bytes,
                context,
                deadline,
            )?;
            if read == 0 {
                return Err(TransportError::new(TransportErrorKind::InvalidMessage));
            }
            output.push(&buffer[..read], memory)?;
            remaining -= read;
        }
        if !read_crlf_line(&mut reader, 2, limit, &mut wire_bytes, context, deadline)?.is_empty() {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
    }
}

pub(super) struct PrefixedReader<'a> {
    prefix: std::io::Cursor<&'a [u8]>,
    stream: &'a mut dyn Connection,
}

impl<'a> PrefixedReader<'a> {
    fn new(prefix: &'a [u8], stream: &'a mut dyn Connection) -> Self {
        Self { prefix: std::io::Cursor::new(prefix), stream }
    }

    fn has_prefetched_bytes(&self) -> bool {
        self.prefix.position() < u64::try_from(self.prefix.get_ref().len()).unwrap_or(u64::MAX)
    }
}

impl Read for PrefixedReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.prefix.read(buffer)?;
        if read != 0 { Ok(read) } else { self.stream.read(buffer) }
    }
}

pub(super) struct FixedLine {
    bytes: [u8; 32],
    len: usize,
}

impl FixedLine {
    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub(super) fn read_crlf_line(
    reader: &mut PrefixedReader<'_>,
    limit: usize,
    wire_limit: usize,
    wire_bytes: &mut usize,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<FixedLine, TransportError> {
    if limit > 32 {
        return Err(TransportError::new(TransportErrorKind::ResourceLimit));
    }
    let mut line = FixedLine { bytes: [0; 32], len: 0 };
    while line.len < limit {
        let mut byte = [0_u8; 1];
        if read_wire_checked(reader, &mut byte, wire_limit, wire_bytes, context, deadline)? == 0 {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
        line.bytes[line.len] = byte[0];
        line.len += 1;
        if line.as_slice().ends_with(b"\r\n") {
            line.len -= 2;
            return Ok(line);
        }
    }
    Err(TransportError::new(TransportErrorKind::ResourceLimit))
}

pub(super) fn read_wire_checked(
    reader: &mut PrefixedReader<'_>,
    bytes: &mut [u8],
    limit: usize,
    consumed: &mut usize,
    context: &ExecutionContext,
    deadline: Instant,
) -> Result<usize, TransportError> {
    if reader.has_prefetched_bytes() {
        return read_checked(reader, bytes, context, deadline);
    }
    let remaining = limit
        .checked_sub(*consumed)
        .ok_or_else(|| TransportError::new(TransportErrorKind::ResourceLimit))?;
    if remaining == 0 {
        return Err(TransportError::new(TransportErrorKind::ResourceLimit));
    }
    let requested = bytes.len().min(remaining);
    let read = read_checked(reader, &mut bytes[..requested], context, deadline)?;
    *consumed = consumed
        .checked_add(read)
        .ok_or_else(|| TransportError::new(TransportErrorKind::ResourceLimit))?;
    Ok(read)
}

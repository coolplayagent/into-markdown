use super::*;

pub(super) fn read_head(
    stream: &mut dyn Connection,
    context: &ExecutionContext,
    deadline: Instant,
    memory: &mut ResourceReservation,
) -> Result<(Vec<u8>, Vec<u8>), TransportError> {
    memory
        .grow(
            u64::try_from(MAX_HEADER_BYTES + IO_CHUNK_BYTES)
                .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?,
        )
        .map_err(map_context_error)?;
    let mut bytes = Vec::with_capacity(MAX_HEADER_BYTES + IO_CHUNK_BYTES);
    loop {
        if bytes.len() >= MAX_HEADER_BYTES + IO_CHUNK_BYTES {
            return Err(TransportError::new(TransportErrorKind::ResourceLimit));
        }
        let old_len = bytes.len();
        bytes.resize((old_len + IO_CHUNK_BYTES).min(MAX_HEADER_BYTES + IO_CHUNK_BYTES), 0);
        let read = read_checked(stream, &mut bytes[old_len..], context, deadline)?;
        bytes.truncate(old_len + read);
        if let Some(index) = find_bytes(&bytes, b"\r\n\r\n") {
            let end = index + 4;
            if end > MAX_HEADER_BYTES {
                return Err(TransportError::new(TransportErrorKind::ResourceLimit));
            }
            let body = bytes.split_off(end);
            return Ok((bytes, body));
        }
        if read == 0 {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
    }
}

pub(super) fn parse_head(bytes: &[u8]) -> Result<ParsedHead, TransportError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| TransportError::new(TransportErrorKind::InvalidMessage))?;
    let mut lines = text
        .strip_suffix("\r\n\r\n")
        .ok_or_else(|| TransportError::new(TransportErrorKind::InvalidMessage))?
        .split("\r\n");
    let status_line =
        lines.next().ok_or_else(|| TransportError::new(TransportErrorKind::InvalidMessage))?;
    let mut status_parts = status_line.split(' ');
    let version = status_parts.next().unwrap_or_default();
    let status_text = status_parts.next().unwrap_or_default();
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || status_text.len() != 3
        || !status_text.bytes().all(|byte| byte.is_ascii_digit())
        || status_parts.next().is_some_and(|reason| reason.bytes().any(invalid_header_value_byte))
    {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    let status = status_text
        .parse::<u16>()
        .map_err(|_| TransportError::new(TransportErrorKind::InvalidMessage))?;
    if !(200..=599).contains(&status) {
        return Err(TransportError::new(TransportErrorKind::Http));
    }
    let mut headers = Vec::new();
    for line in lines {
        if headers.len() >= MAX_HEADER_COUNT || line.is_empty() || line.starts_with([' ', '\t']) {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| TransportError::new(TransportErrorKind::InvalidMessage))?;
        if name.is_empty()
            || !name.bytes().all(is_token)
            || value.bytes().any(invalid_header_value_byte)
        {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
        headers.push((name.to_ascii_lowercase(), value.trim_matches([' ', '\t']).to_owned()));
    }
    let content_length = unique_header(&headers, "content-length")?
        .map(|value| {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(TransportError::new(TransportErrorKind::InvalidMessage));
            }
            value
                .parse::<usize>()
                .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))
        })
        .transpose()?;
    let transfer = unique_header(&headers, "transfer-encoding")?;
    if content_length.is_some() && transfer.is_some() {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    let framing = match (content_length, transfer) {
        (Some(length), None) => Framing::Length(length),
        (Some(_), Some(_)) => {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
        (None, Some(value)) if value.eq_ignore_ascii_case("chunked") => Framing::Chunked,
        (None, Some(_)) => return Err(TransportError::new(TransportErrorKind::InvalidMessage)),
        (None, None) => Framing::Close,
    };
    let content_encoding = match unique_header(&headers, "content-encoding")? {
        None => ContentEncoding::Identity,
        Some(value) if value.eq_ignore_ascii_case("identity") => ContentEncoding::Identity,
        Some(value) if value.eq_ignore_ascii_case("gzip") => ContentEncoding::Gzip,
        Some(_) => return Err(TransportError::new(TransportErrorKind::InvalidMessage)),
    };
    let location = unique_header(&headers, "location")?.map(str::to_owned);
    if location.as_ref().is_some_and(|value| value.is_empty()) {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    let media_type =
        unique_header(&headers, "content-type")?.map(parse_content_type).transpose()?;
    let filename = unique_header(&headers, "content-disposition")?
        .map(parse_content_disposition)
        .transpose()?
        .flatten();
    for sensitive in ["connection", "host", "trailer"] {
        let _ = unique_header(&headers, sensitive)?;
    }
    Ok(ParsedHead { status, framing, location, media_type, filename, content_encoding })
}

pub(super) fn unique_header<'a>(
    headers: &'a [(String, String)],
    name: &str,
) -> Result<Option<&'a str>, TransportError> {
    let mut found = None;
    for (_, value) in headers.iter().filter(|(header, _)| header == name) {
        if found.replace(value.as_str()).is_some() {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
    }
    Ok(found)
}

use super::*;

const FIXED_REQUEST_BYTES: usize =
    "GET  HTTP/1.1\r\nHost: \r\nAccept: */*\r\nAccept-Encoding: gzip\r\nConnection: close\r\nUser-Agent: into-md/0\r\n\r\n".len();

pub(super) fn canonical_url(value: &str) -> Result<Url, TransportError> {
    if value.len() > MAX_URL_BYTES || value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    let mut url =
        Url::parse(value).map_err(|_| TransportError::new(TransportErrorKind::InvalidMessage))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host().is_none()
    {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    url.set_fragment(None);
    Ok(url)
}

pub(super) fn canonical_redirect(base: &Url, location: &str) -> Result<Url, TransportError> {
    if location.len() > MAX_URL_BYTES || location.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    let joined =
        base.join(location).map_err(|_| TransportError::new(TransportErrorKind::InvalidMessage))?;
    canonical_url(joined.as_str())
}

pub(super) fn redacted_url(url: &Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    redacted.set_query(None);
    redacted.set_fragment(None);
    redacted.to_string()
}

pub(super) fn canonical_host(url: &Url) -> Result<String, TransportError> {
    url.host()
        .map(|host| match host {
            Host::Domain(domain) => domain.trim_end_matches('.').to_ascii_lowercase(),
            Host::Ipv4(address) => address.to_string(),
            Host::Ipv6(address) => address.to_string(),
        })
        .filter(|host| !host.is_empty())
        .ok_or_else(|| TransportError::new(TransportErrorKind::InvalidMessage))
}

pub(super) fn normalize_allowlist(values: &[String]) -> Result<BTreeSet<String>, TransportError> {
    if values.len() > MAX_ALLOWED_HOSTS {
        return Err(TransportError::new(TransportErrorKind::ResourceLimit));
    }
    values
        .iter()
        .map(|value| {
            if value.is_empty() || value.len() > MAX_HOST_BYTES || value.trim() != value {
                return Err(TransportError::new(TransportErrorKind::InvalidMessage));
            }
            let value = value.strip_suffix('.').unwrap_or(value);
            if value.is_empty() || value.ends_with('.') {
                return Err(TransportError::new(TransportErrorKind::InvalidMessage));
            }
            Host::parse(value)
                .map(|host| host.to_string().to_ascii_lowercase())
                .map_err(|_| TransportError::new(TransportErrorKind::InvalidMessage))
        })
        .collect()
}

pub(super) fn encode_get_request(
    url: &Url,
    host: &str,
    context: &ExecutionContext,
) -> Result<(String, ResourceReservation), TransportError> {
    let path = url.path();
    let query = url.query();
    if path.is_empty()
        || path.bytes().any(|byte| byte.is_ascii_control())
        || query.is_some_and(|value| value.bytes().any(|byte| byte.is_ascii_control()))
    {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    let default_port = match url.scheme() {
        "http" => 80,
        "https" => 443,
        _ => return Err(TransportError::new(TransportErrorKind::InvalidMessage)),
    };
    let ipv6 = host.parse::<Ipv6Addr>().is_ok();
    let explicit_port = url.port().filter(|port| *port != default_port);
    let port_digits = explicit_port.map_or(0, |port| port.to_string().len() + 1);
    let required = FIXED_REQUEST_BYTES
        .checked_add(path.len())
        .and_then(|size| size.checked_add(query.map_or(0, |value| value.len() + 1)))
        .and_then(|size| size.checked_add(host.len()))
        .and_then(|size| size.checked_add(usize::from(ipv6) * 2))
        .and_then(|size| size.checked_add(port_digits))
        .filter(|size| *size <= MAX_URL_BYTES + FIXED_REQUEST_BYTES + MAX_HOST_BYTES + 8)
        .ok_or_else(|| TransportError::new(TransportErrorKind::ResourceLimit))?;
    let mut memory = context
        .reserve_memory(
            u64::try_from(required)
                .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?,
        )
        .map_err(map_context_error)?;
    let mut request = String::with_capacity(required);
    write!(request, "GET {path}")
        .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
    if let Some(query) = query {
        write!(request, "?{query}")
            .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
    }
    write!(request, " HTTP/1.1\r\nHost: ")
        .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
    if ipv6 {
        write!(request, "[{host}]")
            .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
    } else {
        request.push_str(host);
    }
    if let Some(port) = explicit_port {
        write!(request, ":{port}")
            .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?;
    }
    request.push_str("\r\nAccept: */*\r\nAccept-Encoding: gzip\r\nConnection: close\r\nUser-Agent: into-md/0\r\n\r\n");
    if request.capacity() > required {
        memory
            .grow(
                u64::try_from(request.capacity() - required)
                    .map_err(|_| TransportError::new(TransportErrorKind::ResourceLimit))?,
            )
            .map_err(map_context_error)?;
    }
    if request.len() != required {
        return Err(TransportError::new(TransportErrorKind::ResourceLimit));
    }
    Ok((request, memory))
}

pub(super) fn parse_content_type(value: &str) -> Result<String, TransportError> {
    let media = value.split(';').next().unwrap_or_default().trim();
    let (kind, subtype) = media
        .split_once('/')
        .ok_or_else(|| TransportError::new(TransportErrorKind::InvalidMessage))?;
    if kind.is_empty()
        || subtype.is_empty()
        || !kind.bytes().all(is_token)
        || !subtype.bytes().all(is_token)
    {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    for parameter in value.split(';').skip(1) {
        let (name, raw) = parameter
            .trim()
            .split_once('=')
            .ok_or_else(|| TransportError::new(TransportErrorKind::InvalidMessage))?;
        if name.is_empty() || !name.bytes().all(is_token) || !valid_parameter_value(raw) {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
    }
    Ok(media.to_ascii_lowercase())
}

pub(super) fn parse_content_disposition(value: &str) -> Result<Option<String>, TransportError> {
    let mut parts = split_parameters(value)?;
    let disposition = parts.next().unwrap_or_default().trim();
    if !matches!(disposition.to_ascii_lowercase().as_str(), "attachment" | "inline") {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    let mut plain = None;
    let mut extended = None;
    let mut names = BTreeSet::new();
    for parameter in parts {
        let (name, raw) = parameter
            .trim()
            .split_once('=')
            .ok_or_else(|| TransportError::new(TransportErrorKind::InvalidMessage))?;
        let name = name.to_ascii_lowercase();
        if !names.insert(name.clone()) || !name.bytes().all(is_token) {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
        if name == "filename*" {
            extended = Some(decode_rfc5987(raw)?);
        } else if name == "filename" {
            plain = Some(decode_quoted_or_token(raw)?);
        } else if !valid_parameter_value(raw) {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
    }
    extended.or(plain).map(|name| portable_filename(&name)).transpose()
}

pub(super) fn split_parameters(value: &str) -> Result<impl Iterator<Item = &str>, TransportError> {
    let mut quoted = false;
    let mut escaped = false;
    for byte in value.bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if quoted => escaped = true,
            b'"' => quoted = !quoted,
            byte if invalid_header_value_byte(byte) => {
                return Err(TransportError::new(TransportErrorKind::InvalidMessage));
            }
            _ => {}
        }
    }
    if quoted || escaped {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    // Content-Disposition filenames containing a semicolon are deliberately rejected;
    // accepting them would require retaining untrusted parser state for no source benefit.
    Ok(value.split(';'))
}

pub(super) fn decode_rfc5987(raw: &str) -> Result<String, TransportError> {
    let (charset, rest) = raw
        .split_once('\'')
        .ok_or_else(|| TransportError::new(TransportErrorKind::InvalidMessage))?;
    let (_, encoded) = rest
        .split_once('\'')
        .ok_or_else(|| TransportError::new(TransportErrorKind::InvalidMessage))?;
    if !charset.eq_ignore_ascii_case("utf-8") {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        let byte = encoded.as_bytes()[index];
        if byte == b'%' {
            if index + 2 >= encoded.len() {
                return Err(TransportError::new(TransportErrorKind::InvalidMessage));
            }
            let high = hex(encoded.as_bytes()[index + 1])?;
            let low = hex(encoded.as_bytes()[index + 2])?;
            bytes.push(high * 16 + low);
            index += 3;
        } else if is_attr_char(byte) {
            bytes.push(byte);
            index += 1;
        } else {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
    }
    String::from_utf8(bytes).map_err(|_| TransportError::new(TransportErrorKind::InvalidMessage))
}

pub(super) fn decode_quoted_or_token(raw: &str) -> Result<String, TransportError> {
    if let Some(inner) = raw.strip_prefix('"').and_then(|value| value.strip_suffix('"')) {
        let mut output = String::with_capacity(inner.len());
        let mut escaped = false;
        for character in inner.chars() {
            if escaped {
                output.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' || character.is_control() {
                return Err(TransportError::new(TransportErrorKind::InvalidMessage));
            } else {
                output.push(character);
            }
        }
        if escaped {
            return Err(TransportError::new(TransportErrorKind::InvalidMessage));
        }
        Ok(output)
    } else if !raw.is_empty() && raw.bytes().all(is_token) {
        Ok(raw.to_owned())
    } else {
        Err(TransportError::new(TransportErrorKind::InvalidMessage))
    }
}

pub(super) fn portable_filename(value: &str) -> Result<String, TransportError> {
    let normalized = value.nfc().collect::<String>();
    if normalized.is_empty()
        || normalized.len() > MAX_FILENAME_BYTES
        || normalized != value
        || normalized == "."
        || normalized == ".."
        || normalized.starts_with([' ', '.'])
        || normalized.ends_with([' ', '.'])
        || normalized
            .chars()
            .any(|character| character.is_control() || "<>:\"/\\|?*".contains(character))
        || is_windows_device_name(&normalized)
    {
        return Err(TransportError::new(TransportErrorKind::InvalidMessage));
    }
    Ok(normalized)
}

pub(super) fn is_windows_device_name(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or_default().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || stem.strip_prefix("COM").is_some_and(|number| {
            matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || stem.strip_prefix("LPT").is_some_and(|number| {
            matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
}

pub(super) fn valid_parameter_value(raw: &str) -> bool {
    (!raw.is_empty() && raw.bytes().all(is_token))
        || raw
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .is_some_and(|inner| !inner.bytes().any(invalid_header_value_byte))
}

pub(super) fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

pub(super) fn invalid_header_value_byte(byte: u8) -> bool {
    byte == 0x7f || (byte < 0x20 && byte != b'\t')
}

pub(super) fn is_attr_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte)
}

pub(super) fn hex(byte: u8) -> Result<u8, TransportError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(TransportError::new(TransportErrorKind::InvalidMessage)),
    }
}

pub(super) fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

pub(super) fn is_localhost_name(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost")
}

/// Return whether an address is globally routable under the resolver SSRF policy.
#[must_use]
pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => {
            address.to_ipv4_mapped().map_or_else(|| is_public_ipv6(address), is_public_ipv4)
        }
    }
}

pub(super) fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let value = u32::from(address);
    !in_v4(value, 0x0000_0000, 8)
        && !in_v4(value, 0x0a00_0000, 8)
        && !in_v4(value, 0x6440_0000, 10)
        && !in_v4(value, 0x7f00_0000, 8)
        && !in_v4(value, 0xa9fe_0000, 16)
        && !in_v4(value, 0xac10_0000, 12)
        && !in_v4(value, 0xc000_0000, 24)
        && !in_v4(value, 0xc000_0200, 24)
        && !in_v4(value, 0xc0a8_0000, 16)
        && !in_v4(value, 0xc612_0000, 15)
        && !in_v4(value, 0xc633_6400, 24)
        && !in_v4(value, 0xcb00_7100, 24)
        && !in_v4(value, 0xe000_0000, 4)
        && !in_v4(value, 0xf000_0000, 4)
}

pub(super) fn in_v4(value: u32, network: u32, prefix: u32) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    value & mask == network & mask
}

pub(super) fn is_public_ipv6(address: Ipv6Addr) -> bool {
    let value = u128::from(address);
    in_v6(value, u128::from(Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0)), 3)
        && !in_v6(value, u128::from(Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0)), 23)
        && !in_v6(value, u128::from(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0)), 32)
        && !in_v6(value, u128::from(Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0)), 16)
        && !in_v6(value, u128::from(Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0)), 20)
}

pub(super) fn in_v6(value: u128, network: u128, prefix: u32) -> bool {
    let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
    value & mask == network & mask
}

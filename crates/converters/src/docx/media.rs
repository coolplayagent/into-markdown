fn append_related_parts(
    package: &mut Package,
    options: &ConversionOptions,
    context: &ExecutionContext,
    state: &mut ParseState,
) -> Result<(), ConversionError> {
    let mut seen = BTreeSet::new();
    for (part, label) in std::mem::take(&mut state.related_parts) {
        if !seen.insert(part.clone()) {
            continue;
        }
        let profile = if label == "Header" { XmlProfile::Header } else { XmlProfile::Footer };
        let expected_content_type = if label == "Header" {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"
        } else {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"
        };
        if package.content_types.content_type(&part) != Some(expected_content_type) {
            return Err(malformed(
                Some("[Content_Types].xml"),
                format!("{label} relationship targets a part with the wrong content type"),
            ));
        }
        state.add_inlines(1)?;
        let heading = state.node(
            Block::Heading {
                level: 6,
                content: vec![Inline::Text { value: label.into(), marks: Vec::new() }],
            },
            &part,
        )?;
        state.document.blocks.push(heading);
        let rels = parse_relationships(
            package.parts.get(&relationship_part(&part)).map(Vec::as_slice),
            &part,
            options,
            context,
        )?;
        let part_bytes = package.take_required(&part)?;
        parse_word_part(
            &part_bytes,
            &part,
            profile,
            &rels,
            &BTreeMap::new(),
            &BTreeMap::new(),
            package,
            options,
            context,
            state,
        )?;
    }
    Ok(())
}
fn recoverable_office_media_error(error: &ConversionError) -> bool {
    matches!(error, ConversionError::Malformed { .. } | ConversionError::Unsupported { .. })
}

fn push_word_media_placeholder(
    state: &mut ParseState,
    table: Option<&mut TableBuild>,
    part: &str,
    alt: Option<&str>,
    message: &str,
    code: &str,
) -> Result<(), ConversionError> {
    state.warning(code, message, part);
    let label = alt.filter(|value| !value.is_empty()).unwrap_or("Unsupported media");
    state.add_inlines(1)?;
    let node = state.node(
        Block::Paragraph(vec![Inline::Text { value: format!("[{label}]"), marks: Vec::new() }]),
        part,
    )?;
    if let Some(table) = table {
        table.cell_blocks.push(node);
    } else {
        state.document.blocks.push(node);
    }
    Ok(())
}

fn validate_image_dimensions(width: u32, height: u32, part: &str) -> Result<(), ConversionError> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| malformed(Some(part), "image dimensions overflow"))?;
    if width == 0 || height == 0 {
        return Err(malformed(Some(part), "image dimensions must be non-zero"));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_png(
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if !bytes.starts_with(SIGNATURE) {
        return Err(malformed(Some(part), "image/png target lacks the PNG signature"));
    }
    let mut cursor = SIGNATURE.len();
    let mut chunks = 0_u32;
    let mut saw_header = false;
    let mut saw_data = false;
    let mut data_ended = false;
    let mut saw_palette = false;
    let mut layout = None::<(u32, u32, u8, u8)>;
    let mut idat_bytes = 0_u64;
    loop {
        chunks = chunks
            .checked_add(1)
            .ok_or_else(|| malformed(Some(part), "PNG chunk count overflow"))?;
        if chunks > 100_000 {
            return Err(malformed(Some(part), "PNG has too many chunks"));
        }
        let header_end = cursor
            .checked_add(8)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| malformed(Some(part), "truncated PNG chunk header"))?;
        let length = u32::from_be_bytes(
            bytes[cursor..cursor + 4]
                .try_into()
                .map_err(|_| malformed(Some(part), "truncated PNG length"))?,
        ) as usize;
        let kind = &bytes[cursor + 4..header_end];
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            return Err(malformed(Some(part), "PNG chunk type is invalid"));
        }
        let data_end = header_end
            .checked_add(length)
            .filter(|end| end.checked_add(4).is_some_and(|crc_end| crc_end <= bytes.len()))
            .ok_or_else(|| malformed(Some(part), "truncated or oversized PNG chunk"))?;
        let crc_end = data_end + 4;
        let expected_crc = u32::from_be_bytes(
            bytes[data_end..crc_end]
                .try_into()
                .map_err(|_| malformed(Some(part), "truncated PNG CRC"))?,
        );
        if png_crc32(&bytes[cursor + 4..data_end]) != expected_crc {
            return Err(malformed(Some(part), "PNG chunk CRC mismatch"));
        }
        match kind {
            b"IHDR" => {
                if saw_header || chunks != 1 || length != 13 {
                    return Err(malformed(
                        Some(part),
                        "PNG IHDR is missing, duplicated, or invalid",
                    ));
                }
                let data = &bytes[header_end..data_end];
                let width = u32::from_be_bytes(data[0..4].try_into().expect("fixed IHDR width"));
                let height = u32::from_be_bytes(data[4..8].try_into().expect("fixed IHDR height"));
                validate_image_dimensions(width, height, part)?;
                let bits_per_pixel = match (data[8], data[9]) {
                    (depth @ (1 | 2 | 4 | 8 | 16), 0) | (depth @ (1 | 2 | 4 | 8), 3) => Some(depth),
                    (depth @ (8 | 16), 2) => depth.checked_mul(3),
                    (depth @ (8 | 16), 4) => depth.checked_mul(2),
                    (depth @ (8 | 16), 6) => depth.checked_mul(4),
                    _ => None,
                };
                if bits_per_pixel.is_none() || data[10] != 0 || data[11] != 0 || data[12] != 0 {
                    return Err(malformed(Some(part), "PNG IHDR uses an unsupported encoding"));
                }
                layout = Some((width, height, bits_per_pixel.expect("checked above"), data[9]));
                saw_header = true;
            }
            b"IDAT" => {
                if !saw_header || length == 0 || data_ended {
                    return Err(malformed(
                        Some(part),
                        "PNG IDAT is empty, misplaced, or non-contiguous",
                    ));
                }
                saw_data = true;
                idat_bytes = idat_bytes
                    .checked_add(u64::try_from(length).unwrap_or(u64::MAX))
                    .ok_or_else(|| malformed(Some(part), "PNG IDAT length overflow"))?;
            }
            b"IEND" => {
                if length != 0 || !saw_header || !saw_data || crc_end != bytes.len() {
                    return Err(malformed(Some(part), "PNG IEND or trailing data is invalid"));
                }
                if layout.is_some_and(|(_, _, _, color_type)| color_type == 3) && !saw_palette {
                    return Err(malformed(Some(part), "indexed PNG is missing its palette"));
                }
                break;
            }
            b"PLTE" => {
                if !saw_header
                    || saw_palette
                    || saw_data
                    || length == 0
                    || !length.is_multiple_of(3)
                    || length > 768
                    || layout.is_some_and(|(_, _, _, color_type)| matches!(color_type, 0 | 4))
                    || layout.is_some_and(|(_, _, depth, color_type)| {
                        color_type == 3 && length / 3 > (1_usize << depth)
                    })
                {
                    return Err(malformed(Some(part), "PNG palette is invalid"));
                }
                saw_palette = true;
            }
            _ if kind[0].is_ascii_uppercase() => {
                return Err(malformed(Some(part), "PNG contains an unsupported critical chunk"));
            }
            _ => {}
        }
        if saw_data && kind != b"IDAT" {
            data_ended = true;
        }
        cursor = crc_end;
    }
    let (width, height, bits_per_pixel, _) =
        layout.ok_or_else(|| malformed(Some(part), "PNG is missing IHDR"))?;
    validate_png_data(bytes, part, width, height, bits_per_pixel, idat_bytes, options, context)
}

struct PngIdatReader<'a> {
    bytes: &'a [u8],
    chunk_cursor: usize,
    data_cursor: usize,
    data_end: usize,
    finished: bool,
}

impl<'a> PngIdatReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, chunk_cursor: 8, data_cursor: 0, data_end: 0, finished: false }
    }

    fn next_data(&mut self) -> std::io::Result<bool> {
        while !self.finished {
            let header_end = self.chunk_cursor.checked_add(8).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "PNG chunk offset overflow")
            })?;
            if header_end > self.bytes.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated PNG chunk",
                ));
            }
            let length = usize::try_from(u32::from_be_bytes(
                self.bytes[self.chunk_cursor..self.chunk_cursor + 4].try_into().map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid PNG length")
                })?,
            ))
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "PNG length overflow")
            })?;
            let data_end = header_end.checked_add(length).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "PNG data offset overflow")
            })?;
            let crc_end = data_end.checked_add(4).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "PNG CRC offset overflow")
            })?;
            if crc_end > self.bytes.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated PNG data",
                ));
            }
            let kind = &self.bytes[self.chunk_cursor + 4..header_end];
            self.chunk_cursor = crc_end;
            if kind == b"IDAT" {
                self.data_cursor = header_end;
                self.data_end = data_end;
                return Ok(true);
            }
            if kind == b"IEND" {
                self.finished = true;
            }
        }
        Ok(false)
    }
}

impl Read for PngIdatReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let mut written = 0;
        while written < output.len() {
            if self.data_cursor == self.data_end && !self.next_data()? {
                break;
            }
            let count = (output.len() - written).min(self.data_end - self.data_cursor);
            output[written..written + count]
                .copy_from_slice(&self.bytes[self.data_cursor..self.data_cursor + count]);
            self.data_cursor += count;
            written += count;
        }
        Ok(written)
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_png_data(
    bytes: &[u8],
    part: &str,
    width: u32,
    height: u32,
    bits_per_pixel: u8,
    idat_bytes: u64,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let row_bytes = u64::from(width)
        .checked_mul(u64::from(bits_per_pixel))
        .and_then(|bits| bits.checked_add(7))
        .map(|bits| bits / 8)
        .ok_or_else(|| malformed(Some(part), "PNG row size overflow"))?;
    let row_stride =
        row_bytes.checked_add(1).ok_or_else(|| malformed(Some(part), "PNG row stride overflow"))?;
    let expected = row_stride
        .checked_mul(u64::from(height))
        .ok_or_else(|| malformed(Some(part), "PNG decoded size overflow"))?;
    if expected > options.limits.max_decompressed_bytes {
        return Err(limit(
            "max_decompressed_bytes",
            format!("decoded image {part}: {expected} > {}", options.limits.max_decompressed_bytes),
        ));
    }
    let _work_memory = context.reserve_memory(64 * 1024)?;
    let mut decoder =
        flate2::read::ZlibDecoder::new_with_buf(PngIdatReader::new(bytes), vec![0; 8 * 1024]);
    let mut buffer = [0_u8; 8 * 1024];
    let mut decompressed_bytes = 0_u64;
    let mut row_position = 0_u64;
    loop {
        context.checkpoint()?;
        let count = decoder
            .read(&mut buffer)
            .map_err(|error| malformed(Some(part), format!("invalid PNG pixel stream: {error}")))?;
        if count == 0 {
            break;
        }
        decompressed_bytes = decompressed_bytes
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or_else(|| malformed(Some(part), "PNG decoded length overflow"))?;
        if decompressed_bytes > expected {
            return Err(malformed(Some(part), "PNG pixel stream exceeds declared dimensions"));
        }
        for byte in &buffer[..count] {
            if row_position == 0 && *byte > 4 {
                return Err(malformed(Some(part), "PNG scanline filter is invalid"));
            }
            row_position += 1;
            if row_position == row_stride {
                row_position = 0;
            }
        }
    }
    if decompressed_bytes != expected
        || row_position != 0
        || decoder.total_out() != expected
        || decoder.total_in() != idat_bytes
    {
        return Err(malformed(
            Some(part),
            "PNG pixel stream does not match IHDR dimensions or IDAT bounds",
        ));
    }
    Ok(())
}

pub(super) fn png_crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 { crc >> 1 } else { (crc >> 1) ^ 0xedb8_8320 };
        }
    }
    !crc
}

fn validate_jpeg(bytes: &[u8], part: &str) -> Result<(u32, u32), ConversionError> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err(malformed(Some(part), "image/jpeg target lacks the JPEG SOI marker"));
    }
    let mut cursor = 2_usize;
    let mut quantization_tables = BTreeSet::<u8>::new();
    let mut huffman_tables = BTreeSet::<(u8, u8)>::new();
    let mut frame = None::<(u32, u32, BTreeMap<u8, u8>)>;
    while cursor < bytes.len() {
        if bytes[cursor] != 0xff {
            return Err(malformed(Some(part), "JPEG marker boundary is invalid"));
        }
        while cursor < bytes.len() && bytes[cursor] == 0xff {
            cursor += 1;
        }
        let marker =
            *bytes.get(cursor).ok_or_else(|| malformed(Some(part), "truncated JPEG marker"))?;
        cursor += 1;
        if marker == 0xd9 {
            return Err(malformed(Some(part), "JPEG has no scan data"));
        }
        if matches!(marker, 0x00 | 0x01 | 0xd0..=0xd8) {
            return Err(malformed(Some(part), "unexpected standalone JPEG marker"));
        }
        let length_end = cursor
            .checked_add(2)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| malformed(Some(part), "truncated JPEG segment length"))?;
        let length = usize::from(u16::from_be_bytes(
            bytes[cursor..length_end]
                .try_into()
                .map_err(|_| malformed(Some(part), "truncated JPEG segment length"))?,
        ));
        if length < 2 {
            return Err(malformed(Some(part), "JPEG segment length is invalid"));
        }
        let segment_end = cursor
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| malformed(Some(part), "truncated JPEG segment"))?;
        let data = &bytes[length_end..segment_end];
        match marker {
            0xdb => validate_jpeg_quantization(data, &mut quantization_tables, part)?,
            0xc4 => validate_jpeg_huffman(data, &mut huffman_tables, part)?,
            0xc0 => {
                if frame.is_some() {
                    return Err(malformed(Some(part), "JPEG has multiple baseline frames"));
                }
                if data.len() < 6 || data[0] != 8 {
                    return Err(malformed(Some(part), "JPEG baseline frame is invalid"));
                }
                let height = u32::from(u16::from_be_bytes([data[1], data[2]]));
                let width = u32::from(u16::from_be_bytes([data[3], data[4]]));
                let components = usize::from(data[5]);
                if !(1..=4).contains(&components) || data.len() != 6 + components * 3 {
                    return Err(malformed(Some(part), "JPEG component table is invalid"));
                }
                validate_image_dimensions(width, height, part)?;
                let mut component_tables = BTreeMap::new();
                for component in data[6..].chunks_exact(3) {
                    let sampling = component[1];
                    if sampling >> 4 == 0
                        || sampling >> 4 > 4
                        || sampling.is_multiple_of(16)
                        || sampling & 0x0f > 4
                        || component[2] > 3
                        || component_tables.insert(component[0], component[2]).is_some()
                    {
                        return Err(malformed(Some(part), "JPEG frame component is invalid"));
                    }
                }
                frame = Some((width, height, component_tables));
            }
            0xc1..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                return Err(malformed(Some(part), "only baseline JPEG frames are supported"));
            }
            0xda => {
                validate_jpeg_scan_header(
                    data,
                    frame.as_ref().map(|(_, _, components)| components),
                    &quantization_tables,
                    &huffman_tables,
                    part,
                )?;
                validate_jpeg_scan(&bytes[segment_end..], part)?;
                return frame
                    .map(|(width, height, _)| (width, height))
                    .ok_or_else(|| malformed(Some(part), "JPEG is missing its frame"));
            }
            0xdd if data.len() == 2 => {}
            0xe0..=0xef | 0xfe => {}
            _ => return Err(malformed(Some(part), "unsupported JPEG segment type")),
        }
        cursor = segment_end;
    }
    Err(malformed(Some(part), "JPEG is missing scan and EOI markers"))
}

fn validate_jpeg_pixels(
    bytes: &[u8],
    expected_dimensions: (u32, u32),
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let maximum_pixel_bytes = u64::from(expected_dimensions.0)
        .checked_mul(u64::from(expected_dimensions.1))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| limit("image_decode_memory", "JPEG decoded size overflow"))?;
    if maximum_pixel_bytes > options.limits.max_decompressed_bytes {
        return Err(limit(
            "max_decompressed_bytes",
            format!(
                "decoded image {part}: {maximum_pixel_bytes} > {}",
                options.limits.max_decompressed_bytes
            ),
        ));
    }
    let compressed_bytes = u64::try_from(bytes.len())
        .map_err(|_| limit("image_decode_memory", "JPEG compressed size overflow"))?;
    // Reserve before constructing the decoder. This covers the retained package buffer, the
    // decoder's private input copy, output pixels, component planes/upsampling scratch, and a
    // fixed codec-state allowance. Decoder allocation limits are defense in depth; this explicit
    // request reservation is the authoritative bound.
    let working_set = maximum_pixel_bytes
        .checked_mul(6)
        .and_then(|value| compressed_bytes.checked_mul(2).and_then(|size| value.checked_add(size)))
        .and_then(|value| value.checked_add(256 * 1024))
        .ok_or_else(|| limit("image_decode_memory", "JPEG decode working set overflow"))?;
    let _decode_memory = context.reserve_memory(working_set)?;
    context.checkpoint()?;

    let mut decoder = JpegDecoder::new(Cursor::new(bytes))
        .map_err(|_| malformed(Some(part), "image/jpeg decoder rejected the image header"))?;
    let mut limits = ImageLimits::default();
    limits.max_image_width = Some(expected_dimensions.0);
    limits.max_image_height = Some(expected_dimensions.1);
    limits.max_alloc = Some(working_set);
    decoder
        .set_limits(limits)
        .map_err(|_| malformed(Some(part), "image/jpeg decoder rejected the resource limits"))?;
    if decoder.dimensions() != expected_dimensions {
        return Err(malformed(
            Some(part),
            "image/jpeg decoder dimensions disagree with the validated frame",
        ));
    }
    let decoded_bytes = decoder.total_bytes();
    if decoded_bytes > maximum_pixel_bytes || decoded_bytes > options.limits.max_decompressed_bytes
    {
        return Err(limit(
            "max_decompressed_bytes",
            format!("decoded image/jpeg pixels in {part} exceed the configured budget"),
        ));
    }
    let decoded_length = usize::try_from(decoded_bytes)
        .map_err(|_| limit("max_decompressed_bytes", "JPEG decoded size cannot be represented"))?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(decoded_length).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve JPEG pixels: {error}"))
    })?;
    pixels.resize(decoded_length, 0);
    decoder
        .read_image(&mut pixels)
        .map_err(|_| malformed(Some(part), "image/jpeg entropy stream is not decodable"))?;
    context.checkpoint()?;
    Ok(())
}

fn validate_jpeg_quantization(
    data: &[u8],
    tables: &mut BTreeSet<u8>,
    part: &str,
) -> Result<(), ConversionError> {
    let mut cursor = 0;
    while cursor < data.len() {
        let selector = data[cursor];
        cursor += 1;
        let precision = selector >> 4;
        let table = selector & 0x0f;
        if precision > 1 || table > 3 || !tables.insert(table) {
            return Err(malformed(Some(part), "JPEG quantization table selector is invalid"));
        }
        let table_bytes = if precision == 0 { 64 } else { 128 };
        let table_end = cursor
            .checked_add(table_bytes)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| malformed(Some(part), "truncated JPEG quantization table"))?;
        let values_valid = if precision == 0 {
            data[cursor..table_end].iter().all(|value| *value != 0)
        } else {
            data[cursor..table_end].chunks_exact(2).all(|value| value != [0, 0])
        };
        if !values_valid {
            return Err(malformed(Some(part), "JPEG quantization value is zero"));
        }
        cursor = table_end;
    }
    if cursor == 0 {
        return Err(malformed(Some(part), "empty JPEG quantization segment"));
    }
    Ok(())
}

fn validate_jpeg_huffman(
    data: &[u8],
    tables: &mut BTreeSet<(u8, u8)>,
    part: &str,
) -> Result<(), ConversionError> {
    let mut cursor = 0;
    while cursor < data.len() {
        let header_end = cursor
            .checked_add(17)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| malformed(Some(part), "truncated JPEG Huffman table"))?;
        let selector = data[cursor];
        let class = selector >> 4;
        let table = selector & 0x0f;
        if class > 1 || table > 3 || !tables.insert((class, table)) {
            return Err(malformed(Some(part), "JPEG Huffman table selector is invalid"));
        }
        let counts = &data[cursor + 1..header_end];
        let symbols = counts
            .iter()
            .try_fold(0_usize, |count, value| count.checked_add(usize::from(*value)))
            .ok_or_else(|| malformed(Some(part), "JPEG Huffman symbol count overflow"))?;
        if symbols == 0 || symbols > 256 {
            return Err(malformed(Some(part), "JPEG Huffman symbol count is invalid"));
        }
        let mut code_space = 1_i32;
        for count in counts {
            code_space = code_space
                .checked_mul(2)
                .and_then(|space| space.checked_sub(i32::from(*count)))
                .ok_or_else(|| malformed(Some(part), "JPEG Huffman code space overflow"))?;
            if code_space < 0 {
                return Err(malformed(Some(part), "JPEG Huffman table is oversubscribed"));
            }
        }
        let symbols_end = header_end
            .checked_add(symbols)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| malformed(Some(part), "truncated JPEG Huffman symbols"))?;
        let symbols_valid = if class == 0 {
            data[header_end..symbols_end].iter().all(|symbol| *symbol <= 11)
        } else {
            data[header_end..symbols_end].iter().all(|symbol| {
                let run = symbol >> 4;
                let size = symbol & 0x0f;
                size <= 10 && (size != 0 || matches!(run, 0 | 15))
            })
        };
        if !symbols_valid {
            return Err(malformed(Some(part), "JPEG Huffman symbol is invalid"));
        }
        cursor = symbols_end;
    }
    if cursor == 0 {
        return Err(malformed(Some(part), "empty JPEG Huffman segment"));
    }
    Ok(())
}

fn validate_jpeg_scan_header(
    data: &[u8],
    frame: Option<&BTreeMap<u8, u8>>,
    quantization_tables: &BTreeSet<u8>,
    huffman_tables: &BTreeSet<(u8, u8)>,
    part: &str,
) -> Result<(), ConversionError> {
    let frame = frame.ok_or_else(|| malformed(Some(part), "JPEG scan precedes its frame"))?;
    let components = data.first().copied().map_or(0, usize::from);
    if components != frame.len() || data.len() != 4 + components * 2 {
        return Err(malformed(Some(part), "JPEG scan component table is invalid"));
    }
    let mut seen = BTreeSet::new();
    for component in data[1..=components * 2].chunks_exact(2) {
        let id = component[0];
        let dc = component[1] >> 4;
        let ac = component[1] & 0x0f;
        if !frame.contains_key(&id)
            || !seen.insert(id)
            || !huffman_tables.contains(&(0, dc))
            || !huffman_tables.contains(&(1, ac))
        {
            return Err(malformed(Some(part), "JPEG scan references an undefined table"));
        }
    }
    if frame.values().any(|table| !quantization_tables.contains(table))
        || data[data.len() - 3..] != [0, 63, 0]
    {
        return Err(malformed(Some(part), "JPEG baseline scan parameters are invalid"));
    }
    Ok(())
}

fn validate_jpeg_scan(bytes: &[u8], part: &str) -> Result<(), ConversionError> {
    let mut cursor = 0;
    let mut entropy_bytes = 0_usize;
    while cursor < bytes.len() {
        if bytes[cursor] != 0xff {
            entropy_bytes += 1;
            cursor += 1;
            continue;
        }
        let marker = *bytes
            .get(cursor + 1)
            .ok_or_else(|| malformed(Some(part), "truncated JPEG entropy marker"))?;
        match marker {
            0x00 => {
                entropy_bytes += 1;
                cursor += 2;
            }
            0xd0..=0xd7 => cursor += 2,
            0xd9 if entropy_bytes != 0 && cursor + 2 == bytes.len() => return Ok(()),
            _ => return Err(malformed(Some(part), "unsupported JPEG marker inside scan data")),
        }
    }
    Err(malformed(Some(part), "JPEG scan is missing EOI"))
}

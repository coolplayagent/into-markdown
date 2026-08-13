//! Bounded, versioned worker protocol.

use into_markdown_core::Tensor;
use into_markdown_ocr::{
    Dimension, MAX_TENSOR_NAME_BYTES, MAX_TENSOR_RANK, MAX_TENSORS, ModelContract, ModelMetadata,
    SessionOptions, TensorElementType, TensorSpec,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};

pub(crate) const VERSION: u16 = 1;
pub(crate) const INIT: u16 = 1;
pub(crate) const RUN: u16 = 2;
pub(crate) const SHUTDOWN: u16 = 3;
pub(crate) const INIT_OK: u16 = 101;
pub(crate) const RUN_OK: u16 = 102;
pub(crate) const ERROR: u16 = 255;
pub(crate) const ERROR_SESSION: u8 = 1;
pub(crate) const ERROR_INFERENCE: u8 = 2;
pub(crate) const ERROR_PROTOCOL: u8 = 3;
pub(crate) const ERROR_RESOURCE: u8 = 4;
pub(crate) const ERROR_ABI: u8 = 5;
pub(crate) const HEADER_BYTES: usize = 20;
pub(crate) const MAX_MESSAGES: u64 = 10_000;
const MAGIC: [u8; 4] = *b"IMOR";
const MAX_OPSETS: usize = 64;
const MAX_DOMAIN_BYTES: usize = 256;

#[derive(Debug)]
pub(crate) struct Frame {
    pub kind: u16,
    pub request_id: u64,
    pub payload: Vec<u8>,
}

pub(crate) fn read_frame(reader: &mut impl Read, max_payload: usize) -> Result<Frame, ()> {
    let mut header = [0_u8; HEADER_BYTES];
    reader.read_exact(&mut header).map_err(|_| ())?;
    if header[..4] != MAGIC || u16::from_le_bytes([header[4], header[5]]) != VERSION {
        return Err(());
    }
    let kind = u16::from_le_bytes([header[6], header[7]]);
    let request_id = u64::from_le_bytes(header[8..16].try_into().map_err(|_| ())?);
    let length = usize::try_from(u32::from_le_bytes(header[16..20].try_into().map_err(|_| ())?))
        .map_err(|_| ())?;
    if length > max_payload {
        return Err(());
    }
    let mut payload = Vec::new();
    payload.try_reserve_exact(length).map_err(|_| ())?;
    payload.resize(length, 0);
    reader.read_exact(&mut payload).map_err(|_| ())?;
    Ok(Frame { kind, request_id, payload })
}

pub(crate) fn write_frame(
    writer: &mut impl Write,
    kind: u16,
    request_id: u64,
    payload: &[u8],
) -> Result<(), ()> {
    let length = u32::try_from(payload.len()).map_err(|_| ())?;
    let mut header = [0_u8; HEADER_BYTES];
    header[..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&VERSION.to_le_bytes());
    header[6..8].copy_from_slice(&kind.to_le_bytes());
    header[8..16].copy_from_slice(&request_id.to_le_bytes());
    header[16..20].copy_from_slice(&length.to_le_bytes());
    writer.write_all(&header).map_err(|_| ())?;
    writer.write_all(payload).map_err(|_| ())?;
    writer.flush().map_err(|_| ())
}

#[derive(Default)]
struct Encoder(Vec<u8>);

impl Encoder {
    fn byte(&mut self, value: u8) {
        self.0.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.0.extend(value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.0.extend(value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.0.extend(value.to_le_bytes());
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), ()> {
        self.u64(u64::try_from(value.len()).map_err(|_| ())?);
        self.0.extend(value);
        Ok(())
    }

    fn string(&mut self, value: &str) -> Result<(), ()> {
        self.bytes(value.as_bytes())
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Decoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], ()> {
        let end = self.cursor.checked_add(length).ok_or(())?;
        let value = self.bytes.get(self.cursor..end).ok_or(())?;
        self.cursor = end;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, ()> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ()> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().map_err(|_| ())?))
    }

    fn u32(&mut self) -> Result<u32, ()> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| ())?))
    }

    fn u64(&mut self) -> Result<u64, ()> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| ())?))
    }

    fn bounded_bytes(&mut self, maximum: usize) -> Result<&'a [u8], ()> {
        let length = usize::try_from(self.u64()?).map_err(|_| ())?;
        if length > maximum {
            return Err(());
        }
        self.take(length)
    }

    fn string(&mut self, maximum: usize) -> Result<String, ()> {
        let bytes = self.bounded_bytes(maximum)?;
        if bytes.contains(&0) {
            return Err(());
        }
        std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| ())
    }

    fn finish(self) -> Result<(), ()> {
        if self.cursor == self.bytes.len() { Ok(()) } else { Err(()) }
    }
}

fn encode_specs(encoder: &mut Encoder, specs: &[TensorSpec]) -> Result<(), ()> {
    encoder.u16(u16::try_from(specs.len()).map_err(|_| ())?);
    for spec in specs {
        encoder.string(&spec.name)?;
        encoder.byte(match spec.element_type {
            TensorElementType::Float32 => 1,
        });
        encoder.byte(u8::try_from(spec.dimensions.len()).map_err(|_| ())?);
        for dimension in &spec.dimensions {
            match dimension {
                Dimension::Exact(value) => {
                    encoder.byte(1);
                    encoder.u64(u64::try_from(*value).map_err(|_| ())?);
                    encoder.u64(0);
                }
                Dimension::Dynamic { min, max } => {
                    encoder.byte(2);
                    encoder.u64(u64::try_from(*min).map_err(|_| ())?);
                    encoder.u64(u64::try_from(*max).map_err(|_| ())?);
                }
            }
        }
    }
    Ok(())
}

fn decode_specs(decoder: &mut Decoder<'_>, allow_empty: bool) -> Result<Vec<TensorSpec>, ()> {
    let count = usize::from(decoder.u16()?);
    if count > MAX_TENSORS || (!allow_empty && count == 0) {
        return Err(());
    }
    let mut specs = Vec::new();
    specs.try_reserve_exact(count).map_err(|_| ())?;
    for _ in 0..count {
        let name = decoder.string(MAX_TENSOR_NAME_BYTES)?;
        if name.is_empty() || decoder.byte()? != 1 {
            return Err(());
        }
        let rank = usize::from(decoder.byte()?);
        if rank == 0 || rank > MAX_TENSOR_RANK {
            return Err(());
        }
        let mut dimensions = Vec::new();
        dimensions.try_reserve_exact(rank).map_err(|_| ())?;
        for _ in 0..rank {
            let tag = decoder.byte()?;
            let first = usize::try_from(decoder.u64()?).map_err(|_| ())?;
            let second = usize::try_from(decoder.u64()?).map_err(|_| ())?;
            let dimension = match tag {
                1 if first > 0 && second == 0 => Dimension::Exact(first),
                2 if first > 0 && first <= second => Dimension::Dynamic { min: first, max: second },
                _ => return Err(()),
            };
            dimensions.push(dimension);
        }
        specs.push(TensorSpec { name, element_type: TensorElementType::Float32, dimensions });
    }
    Ok(specs)
}

pub(crate) fn encode_init(
    model: &[u8],
    contract: &ModelContract,
    options: &SessionOptions,
) -> Result<Vec<u8>, ()> {
    let mut encoder = Encoder::default();
    encoder.bytes(model)?;
    encoder.u16(options.intra_op_threads);
    encoder.u16(options.inter_op_threads);
    encoder.byte(u8::from(options.cpu_arena));
    encoder.u64(options.max_session_bytes);
    encoder.u64(contract.ir_version);
    encoder.u16(u16::try_from(contract.opsets.len()).map_err(|_| ())?);
    for (domain, version) in &contract.opsets {
        encoder.string(domain)?;
        encoder.u64(*version);
    }
    encode_specs(&mut encoder, &contract.inputs)?;
    encode_specs(&mut encoder, &contract.overridable_inputs)?;
    encode_specs(&mut encoder, &contract.outputs)?;
    encoder.u64(contract.session_memory_bytes);
    encoder.u64(contract.run_memory_bytes);
    Ok(encoder.0)
}

pub(crate) fn decode_init(
    payload: &[u8],
    maximum_model_bytes: usize,
) -> Result<(&[u8], ModelContract, SessionOptions), ()> {
    let mut decoder = Decoder::new(payload);
    let model = decoder.bounded_bytes(maximum_model_bytes)?;
    let options = SessionOptions {
        intra_op_threads: decoder.u16()?,
        inter_op_threads: decoder.u16()?,
        cpu_arena: match decoder.byte()? {
            0 => false,
            1 => true,
            _ => return Err(()),
        },
        max_session_bytes: decoder.u64()?,
    };
    let ir_version = decoder.u64()?;
    let opset_count = usize::from(decoder.u16()?);
    if opset_count == 0 || opset_count > MAX_OPSETS {
        return Err(());
    }
    let mut opsets = BTreeMap::new();
    for _ in 0..opset_count {
        let domain = decoder.string(MAX_DOMAIN_BYTES)?;
        let version = decoder.u64()?;
        if version == 0 || opsets.insert(domain, version).is_some() {
            return Err(());
        }
    }
    let inputs = decode_specs(&mut decoder, false)?;
    let overridable_inputs = decode_specs(&mut decoder, true)?;
    let outputs = decode_specs(&mut decoder, false)?;
    let contract = ModelContract {
        ir_version,
        opsets,
        inputs,
        overridable_inputs,
        outputs,
        session_memory_bytes: decoder.u64()?,
        run_memory_bytes: decoder.u64()?,
    };
    decoder.finish()?;
    Ok((model, contract, options))
}

pub(crate) fn encode_metadata(metadata: &ModelMetadata) -> Result<Vec<u8>, ()> {
    let mut encoder = Encoder::default();
    encoder.u64(metadata.ir_version);
    encoder.u16(u16::try_from(metadata.opsets.len()).map_err(|_| ())?);
    for (domain, version) in &metadata.opsets {
        encoder.string(domain)?;
        encoder.u64(*version);
    }
    encode_specs(&mut encoder, &metadata.inputs)?;
    encode_specs(&mut encoder, &metadata.overridable_inputs)?;
    encode_specs(&mut encoder, &metadata.outputs)?;
    Ok(encoder.0)
}

pub(crate) fn decode_metadata(payload: &[u8]) -> Result<ModelMetadata, ()> {
    let mut decoder = Decoder::new(payload);
    let ir_version = decoder.u64()?;
    let count = usize::from(decoder.u16()?);
    if count == 0 || count > MAX_OPSETS {
        return Err(());
    }
    let mut opsets = BTreeMap::new();
    for _ in 0..count {
        let domain = decoder.string(MAX_DOMAIN_BYTES)?;
        let version = decoder.u64()?;
        if version == 0 || opsets.insert(domain, version).is_some() {
            return Err(());
        }
    }
    let metadata = ModelMetadata {
        ir_version,
        opsets,
        inputs: decode_specs(&mut decoder, false)?,
        overridable_inputs: decode_specs(&mut decoder, true)?,
        outputs: decode_specs(&mut decoder, false)?,
    };
    decoder.finish()?;
    Ok(metadata)
}

pub(crate) fn encode_tensors(tensors: &[Tensor]) -> Result<Vec<u8>, ()> {
    let mut encoder = Encoder::default();
    encoder.u16(u16::try_from(tensors.len()).map_err(|_| ())?);
    for tensor in tensors {
        encoder.byte(u8::try_from(tensor.shape.len()).map_err(|_| ())?);
        for dimension in &tensor.shape {
            encoder.u64(u64::try_from(*dimension).map_err(|_| ())?);
        }
        encoder.u64(u64::try_from(tensor.values.len()).map_err(|_| ())?);
        for value in &tensor.values {
            encoder.u32(value.to_bits());
        }
    }
    Ok(encoder.0)
}

pub(crate) fn decode_tensors(payload: &[u8], specs: &[TensorSpec]) -> Result<Vec<Tensor>, ()> {
    let mut decoder = Decoder::new(payload);
    let count = usize::from(decoder.u16()?);
    if count != specs.len() || count > MAX_TENSORS {
        return Err(());
    }
    let mut tensors = Vec::new();
    tensors.try_reserve_exact(count).map_err(|_| ())?;
    for spec in specs {
        let rank = usize::from(decoder.byte()?);
        if rank == 0 || rank > MAX_TENSOR_RANK || rank != spec.dimensions.len() {
            return Err(());
        }
        let mut shape = Vec::new();
        shape.try_reserve_exact(rank).map_err(|_| ())?;
        let mut elements = 1_usize;
        for expected in &spec.dimensions {
            let actual = usize::try_from(decoder.u64()?).map_err(|_| ())?;
            let valid = match expected {
                Dimension::Exact(value) => actual == *value,
                Dimension::Dynamic { min, max } => actual >= *min && actual <= *max,
            };
            if !valid {
                return Err(());
            }
            elements = elements.checked_mul(actual).ok_or(())?;
            shape.push(actual);
        }
        let values = usize::try_from(decoder.u64()?).map_err(|_| ())?;
        if values != elements || values.checked_mul(size_of::<f32>()).ok_or(())? > payload.len() {
            return Err(());
        }
        let mut backing = Vec::new();
        backing.try_reserve_exact(values).map_err(|_| ())?;
        for _ in 0..values {
            backing.push(f32::from_bits(decoder.u32()?));
        }
        tensors.push(Tensor { shape, values: backing });
    }
    decoder.finish()?;
    Ok(tensors)
}

pub(crate) fn error_payload(code: u8) -> [u8; 1] {
    [code]
}

pub(crate) fn decode_error(payload: &[u8]) -> Result<u8, ()> {
    match payload {
        [
            code @ (ERROR_SESSION | ERROR_INFERENCE | ERROR_PROTOCOL | ERROR_RESOURCE | ERROR_ABI),
        ] => Ok(*code),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_frame_is_rejected_before_payload_read() {
        let mut bytes = Vec::new();
        let mut header = [0_u8; HEADER_BYTES];
        header[..4].copy_from_slice(&MAGIC);
        header[4..6].copy_from_slice(&VERSION.to_le_bytes());
        header[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend(header);
        assert!(read_frame(&mut bytes.as_slice(), 1024).is_err());
    }
}

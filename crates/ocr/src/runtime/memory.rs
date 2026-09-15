use super::*;

pub(super) fn tensor_storage_bytes(tensors: &[Tensor]) -> Result<u64, ConversionError> {
    tensors.iter().try_fold(0_u64, |total, tensor| {
        let values =
            u64::try_from(tensor.values.len()).map_err(|_| resource_error("tensorMemory"))?;
        let shape =
            u64::try_from(tensor.shape.len()).map_err(|_| resource_error("tensorMemory"))?;
        let bytes = values
            .checked_mul(u64::try_from(std::mem::size_of::<f32>()).unwrap())
            .and_then(|bytes| {
                shape
                    .checked_mul(u64::try_from(std::mem::size_of::<usize>()).unwrap())
                    .and_then(|shape_bytes| bytes.checked_add(shape_bytes))
            })
            .ok_or_else(|| resource_error("tensorMemory"))?;
        total.checked_add(bytes).ok_or_else(|| resource_error("tensorMemory"))
    })
}

pub(super) fn max_tensor_storage_bytes(specs: &[TensorSpec]) -> Result<u64, ConversionError> {
    specs.iter().try_fold(0_u64, |total, spec| {
        let elements = spec.dimensions.iter().try_fold(1_u64, |count, dimension| {
            let maximum = match dimension {
                Dimension::Exact(value) => *value,
                Dimension::Dynamic { max, .. } => *max,
            };
            count
                .checked_mul(u64::try_from(maximum).map_err(|_| resource_error("tensorMemory"))?)
                .ok_or_else(|| resource_error("tensorMemory"))
        })?;
        let shape_bytes = u64::try_from(spec.dimensions.len())
            .map_err(|_| resource_error("tensorMemory"))?
            .checked_mul(u64::try_from(std::mem::size_of::<usize>()).unwrap())
            .ok_or_else(|| resource_error("tensorMemory"))?;
        total
            .checked_add(
                elements
                    .checked_mul(u64::try_from(std::mem::size_of::<f32>()).unwrap())
                    .and_then(|bytes| bytes.checked_add(shape_bytes))
                    .ok_or_else(|| resource_error("tensorMemory"))?,
            )
            .ok_or_else(|| resource_error("tensorMemory"))
    })
}

pub(super) fn contract_metadata_bytes(contract: &ModelContract) -> Result<u64, ConversionError> {
    fn specs_bytes(specs: &[TensorSpec]) -> Result<u64, ConversionError> {
        specs.iter().try_fold(0_u64, |total, spec| {
            let name =
                u64::try_from(spec.name.len()).map_err(|_| resource_error("tensorMemory"))?;
            let dimensions = u64::try_from(spec.dimensions.len())
                .map_err(|_| resource_error("tensorMemory"))?
                .checked_mul(u64::try_from(std::mem::size_of::<Dimension>()).unwrap())
                .ok_or_else(|| resource_error("tensorMemory"))?;
            let structure = u64::try_from(std::mem::size_of::<TensorSpec>()).unwrap();
            total
                .checked_add(name)
                .and_then(|bytes| bytes.checked_add(dimensions))
                .and_then(|bytes| bytes.checked_add(structure))
                .ok_or_else(|| resource_error("tensorMemory"))
        })
    }
    let opsets = contract.opsets.iter().try_fold(0_u64, |total, (domain, _)| {
        let domain = u64::try_from(domain.len()).map_err(|_| resource_error("tensorMemory"))?;
        total
            .checked_add(domain)
            .and_then(|bytes| {
                bytes.checked_add(u64::try_from(std::mem::size_of::<(String, u64)>()).unwrap())
            })
            .ok_or_else(|| resource_error("tensorMemory"))
    })?;
    let specs = specs_bytes(&contract.inputs)?
        .checked_add(specs_bytes(&contract.overridable_inputs)?)
        .and_then(|bytes| bytes.checked_add(specs_bytes(&contract.outputs).ok()?))
        .ok_or_else(|| resource_error("tensorMemory"))?;
    specs
        .checked_add(opsets)
        .and_then(|bytes| {
            bytes.checked_add(u64::try_from(std::mem::size_of::<ModelMetadata>()).unwrap())
        })
        .ok_or_else(|| resource_error("tensorMemory"))
}

pub(super) fn run_memory_peak(
    inputs: &[Tensor],
    contract: &ModelContract,
) -> Result<u64, ConversionError> {
    run_memory_peak_with_output_storage(
        inputs,
        contract,
        max_tensor_storage_bytes(&contract.outputs)?,
    )
}

pub(super) fn run_memory_peak_with_output_storage(
    inputs: &[Tensor],
    contract: &ModelContract,
    output_storage: u64,
) -> Result<u64, ConversionError> {
    let input_clone = tensor_storage_bytes(inputs)?;
    let input_entries = u64::try_from(inputs.len())
        .map_err(|_| resource_error("tensorMemory"))?
        .checked_mul(u64::try_from(std::mem::size_of::<(String, Tensor)>()).unwrap())
        .ok_or_else(|| resource_error("tensorMemory"))?;
    let output_entries = u64::try_from(contract.outputs.len())
        .map_err(|_| resource_error("tensorMemory"))?
        .checked_mul(u64::try_from(std::mem::size_of::<Tensor>()).unwrap())
        .ok_or_else(|| resource_error("tensorMemory"))?;
    // Output storage is charged twice: once for ORT-owned tensor backing and
    // once for the checked Rust copy returned across the runtime boundary.
    input_clone
        .checked_add(input_entries)
        .and_then(|bytes| bytes.checked_add(output_entries))
        .and_then(|bytes| output_storage.checked_mul(2).and_then(|peak| bytes.checked_add(peak)))
        .and_then(|bytes| bytes.checked_add(contract.run_memory_bytes))
        .ok_or_else(|| resource_error("tensorMemory"))
}

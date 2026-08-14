use super::{
    Asset, BlockNode, ConversionError, Diagnostic, ExecutionContext, Inline, Provenance,
    ResourceReservation, allocation_capacity_bound, resource,
};

pub(super) fn materialize_after_reserve<T>(
    context: &ExecutionContext,
    allocation_bytes: u64,
    materialize: impl FnOnce() -> Result<T, ConversionError>,
) -> Result<(T, ResourceReservation), ConversionError> {
    let reservation = context.reserve_memory(allocation_bytes)?;
    let value = materialize()?;
    Ok((value, reservation))
}

pub(super) fn retain_existing_reservation(
    context: &ExecutionContext,
    retained: &mut Vec<ResourceReservation>,
    reservation: ResourceReservation,
) -> Result<(), ConversionError> {
    ensure_reservation_inventory(context, retained, 1)?;
    retained.push(reservation);
    Ok(())
}

pub(super) fn retain_output_bytes(
    context: &ExecutionContext,
    retained: &mut Vec<ResourceReservation>,
    bytes: u64,
) -> Result<(), ConversionError> {
    ensure_reservation_inventory(context, retained, 1)?;
    let reservation = context.reserve_memory(bytes)?;
    retained.push(reservation);
    Ok(())
}

pub(super) fn ensure_reservation_inventory(
    context: &ExecutionContext,
    retained: &mut Vec<ResourceReservation>,
    additional: usize,
) -> Result<(), ConversionError> {
    let required = retained
        .len()
        .checked_add(additional)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| resource("max_memory_bytes", "reservation inventory overflow"))?;
    if required <= retained.capacity() {
        return Ok(());
    }
    let planned = allocation_capacity_bound(required)?;
    let additional_capacity = planned.saturating_sub(retained.capacity());
    let bytes = additional_capacity
        .checked_mul(std::mem::size_of::<ResourceReservation>())
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| resource("max_memory_bytes", "reservation inventory memory overflow"))?;
    let inventory_reservation = context.reserve_memory(bytes)?;
    retained
        .try_reserve_exact(additional_capacity)
        .map_err(|_| resource("max_memory_bytes", "reservation inventory allocation failed"))?;
    if retained.capacity() > planned {
        return Err(resource(
            "max_memory_bytes",
            "reservation inventory capacity exceeded its plan",
        ));
    }
    retained.push(inventory_reservation);
    Ok(())
}

pub(super) fn output_block_overhead(nested_inlines: usize) -> Result<u64, ConversionError> {
    let block_slots = 2_usize
        .checked_mul(std::mem::size_of::<BlockNode>())
        .ok_or_else(|| resource("max_memory_bytes", "block slot memory overflow"))?;
    let inline_slots = allocation_capacity_bound(nested_inlines)?
        .checked_mul(std::mem::size_of::<Inline>())
        .ok_or_else(|| resource("max_memory_bytes", "inline slot memory overflow"))?;
    // Covers bounded node ID, provider, link/image-owned short strings, Box
    // backing, allocator metadata, and the per-page container header.
    let bytes = block_slots
        .checked_add(inline_slots)
        .and_then(|value| value.checked_add(std::mem::size_of::<Provenance>()))
        .and_then(|value| value.checked_add(1_024))
        .ok_or_else(|| resource("max_memory_bytes", "block output memory overflow"))?;
    u64::try_from(bytes).map_err(|_| resource("max_memory_bytes", "block memory does not fit u64"))
}

pub(super) fn asset_record_overhead() -> Result<u64, ConversionError> {
    let bytes = 2_usize
        .checked_mul(std::mem::size_of::<Asset>())
        .and_then(|value| value.checked_add(2 * std::mem::size_of::<String>()))
        // HashSet bucket/control storage, two deterministic IDs, filename,
        // media type, and allocator alignment.
        .and_then(|value| value.checked_add(2_048))
        .ok_or_else(|| resource("max_memory_bytes", "asset record memory overflow"))?;
    u64::try_from(bytes)
        .map_err(|_| resource("max_memory_bytes", "asset record memory does not fit u64"))
}

pub(super) fn diagnostic_overhead() -> Result<u64, ConversionError> {
    let bytes = 2_usize
        .checked_mul(std::mem::size_of::<Diagnostic>())
        .and_then(|value| value.checked_add(1_024))
        .ok_or_else(|| resource("max_memory_bytes", "diagnostic memory overflow"))?;
    u64::try_from(bytes)
        .map_err(|_| resource("max_memory_bytes", "diagnostic memory does not fit u64"))
}

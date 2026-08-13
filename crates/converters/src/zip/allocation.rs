use into_markdown_core::{ConversionError, ResourceReservation};
use std::fmt::{self, Write as _};
use std::mem::size_of;

pub(super) fn charged_format(
    memory: &mut ResourceReservation,
    arguments: fmt::Arguments<'_>,
) -> Result<String, ConversionError> {
    struct Counter(usize);
    impl fmt::Write for Counter {
        fn write_str(&mut self, value: &str) -> fmt::Result {
            self.0 = self.0.checked_add(value.len()).ok_or(fmt::Error)?;
            Ok(())
        }
    }

    let mut counter = Counter(0);
    counter.write_fmt(arguments).map_err(|_| memory_overflow())?;
    let planned = u64::try_from(counter.0).map_err(|_| memory_overflow())?;
    memory.grow(planned)?;
    let mut value = String::new();
    if let Err(error) = value.try_reserve_exact(counter.0) {
        memory.shrink(planned)?;
        return Err(ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: format!("ZIP string allocation failed: {error}"),
        });
    }
    let actual = u64::try_from(value.capacity()).map_err(|_| memory_overflow())?;
    if actual > planned {
        memory.grow(actual - planned)?;
    } else if planned > actual {
        memory.shrink(planned - actual)?;
    }
    value.write_fmt(arguments).map_err(|_| ConversionError::Internal {
        detail: "formatting a pre-sized ZIP string failed".into(),
    })?;
    Ok(value)
}

pub(super) fn reserve_append<T>(
    target: &mut Vec<T>,
    additional: usize,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    let required = target.len().checked_add(additional).ok_or_else(memory_overflow)?;
    if required <= target.capacity() {
        return Ok(());
    }
    let old_capacity = target.capacity();
    let requested_slots = required - old_capacity;
    let requested = requested_slots.checked_mul(size_of::<T>()).ok_or_else(memory_overflow)?;
    let requested = u64::try_from(requested).map_err(|_| memory_overflow())?;
    memory.grow(requested)?;
    if let Err(error) = target.try_reserve_exact(required - target.len()) {
        memory.shrink(requested)?;
        return Err(ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: format!("ZIP output vector allocation failed: {error}"),
        });
    }
    let actual_slots = target.capacity().saturating_sub(old_capacity);
    let actual = actual_slots.checked_mul(size_of::<T>()).ok_or_else(memory_overflow)?;
    let actual = u64::try_from(actual).map_err(|_| memory_overflow())?;
    if actual > requested {
        memory.grow(actual - requested)?;
    } else if requested > actual {
        memory.shrink(requested - actual)?;
    }
    Ok(())
}

fn memory_overflow() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "ZIP allocation size overflowed".into(),
    }
}

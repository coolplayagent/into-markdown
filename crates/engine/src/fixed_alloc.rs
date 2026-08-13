#![allow(unsafe_code)]

use into_markdown_core::ConversionError;
use std::mem::{ManuallyDrop, MaybeUninit};

/// Fallible fixed-layout storage. The allocation layout is exactly `len * size_of::<T>()`;
/// initialized-prefix tracking makes early-error destruction safe.
pub(crate) struct FixedSlots<T> {
    slots: Box<[MaybeUninit<T>]>,
    initialized: usize,
}

impl<T> FixedSlots<T> {
    pub(crate) fn new(length: usize, detail: &'static str) -> Result<Self, ConversionError> {
        let slots = if length == 0 {
            Box::new([])
        } else {
            let layout = std::alloc::Layout::array::<MaybeUninit<T>>(length)
                .map_err(|_| allocation(detail))?;
            // SAFETY: the valid, non-zero layout is transferred into the returned Box.
            let raw = unsafe { std::alloc::alloc(layout) }.cast::<MaybeUninit<T>>();
            if raw.is_null() {
                return Err(allocation(detail));
            }
            // SAFETY: this slice names exactly the allocation above.
            unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(raw, length)) }
        };
        Ok(Self { slots, initialized: 0 })
    }

    pub(crate) fn push(&mut self, value: T) -> Result<(), ConversionError> {
        let Some(slot) = self.slots.get_mut(self.initialized) else {
            return Err(ConversionError::Internal {
                detail: "fixed allocation received more values than planned".into(),
            });
        };
        slot.write(value);
        self.initialized += 1;
        Ok(())
    }

    pub(crate) fn into_vec(self) -> Result<Vec<T>, ConversionError> {
        if self.initialized != self.slots.len() {
            return Err(ConversionError::Internal {
                detail: "fixed allocation received fewer values than planned".into(),
            });
        }
        let this = ManuallyDrop::new(self);
        // SAFETY: ManuallyDrop owns the Box and the field is read exactly once.
        let raw = Box::into_raw(unsafe { std::ptr::read(&raw const this.slots) });
        // SAFETY: every slot is initialized and T has MaybeUninit<T>'s layout.
        Ok(unsafe { Box::from_raw(raw as *mut [T]) }.into_vec())
    }
}

impl<T> Drop for FixedSlots<T> {
    fn drop(&mut self) {
        for value in &mut self.slots[..self.initialized] {
            // SAFETY: only the initialized prefix was written.
            unsafe { value.assume_init_drop() };
        }
    }
}

pub(crate) fn try_clone_string(
    value: &str,
    detail: &'static str,
) -> Result<String, ConversionError> {
    if value.is_empty() {
        return Ok(String::new());
    }
    let layout = std::alloc::Layout::array::<u8>(value.len()).map_err(|_| allocation(detail))?;
    // SAFETY: the valid, non-zero layout is transferred into the returned Vec.
    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        return Err(allocation(detail));
    }
    // SAFETY: the equal-length source and destination do not overlap.
    unsafe { std::ptr::copy_nonoverlapping(value.as_ptr(), raw, value.len()) };
    // SAFETY: allocation, length, and capacity all exactly equal `value.len()`.
    let bytes = unsafe { Vec::from_raw_parts(raw, value.len(), value.len()) };
    String::from_utf8(bytes).map_err(|_| ConversionError::Internal {
        detail: "validated source string stopped being UTF-8".into(),
    })
}

fn allocation(detail: &'static str) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}

#![allow(unsafe_code)]

use into_markdown_core::ConversionError;
use std::mem::{ManuallyDrop, MaybeUninit};

/// Fallible fixed-layout storage whose allocation is exactly
/// `len * size_of::<T>()`. Initialized-prefix tracking makes error cleanup safe.
pub(crate) struct FixedSlots<T> {
    slots: Box<[MaybeUninit<T>]>,
    initialized: usize,
}

impl<T> FixedSlots<T> {
    pub(crate) fn new(length: usize, detail: &'static str) -> Result<Self, ConversionError> {
        let slots = exact_uninit(length, detail)?;
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

/// Exact-capacity UTF-8 builder. Callers calculate the complete byte length
/// before construction, so no allocator-dependent growth occurs while writing.
pub(crate) struct ExactString {
    bytes: Box<[MaybeUninit<u8>]>,
    written: usize,
}

impl ExactString {
    pub(crate) fn new(length: usize, detail: &'static str) -> Result<Self, ConversionError> {
        Ok(Self { bytes: exact_uninit(length, detail)?, written: 0 })
    }

    pub(crate) fn push_byte(&mut self, value: u8) -> Result<(), ConversionError> {
        let Some(slot) = self.bytes.get_mut(self.written) else {
            return Err(ConversionError::Internal {
                detail: "exact string received more bytes than planned".into(),
            });
        };
        slot.write(value);
        self.written += 1;
        Ok(())
    }

    pub(crate) fn push_str(&mut self, value: &str) -> Result<(), ConversionError> {
        let end = self.written.checked_add(value.len()).ok_or_else(allocation_overflow)?;
        let Some(destination) = self.bytes.get_mut(self.written..end) else {
            return Err(ConversionError::Internal {
                detail: "exact string received more bytes than planned".into(),
            });
        };
        // SAFETY: the equally sized source and destination slices do not overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                value.as_ptr(),
                destination.as_mut_ptr().cast::<u8>(),
                value.len(),
            );
        };
        self.written = end;
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<String, ConversionError> {
        if self.written != self.bytes.len() {
            return Err(ConversionError::Internal {
                detail: "exact string received fewer bytes than planned".into(),
            });
        }
        let this = ManuallyDrop::new(self);
        // SAFETY: ManuallyDrop owns the Box and the field is read exactly once.
        let raw = Box::into_raw(unsafe { std::ptr::read(&raw const this.bytes) });
        // SAFETY: every byte was initialized and u8 has MaybeUninit<u8>'s layout.
        let bytes = unsafe { Box::from_raw(raw as *mut [u8]) }.into_vec();
        String::from_utf8(bytes).map_err(|_| ConversionError::Internal {
            detail: "validated exact string stopped being UTF-8".into(),
        })
    }
}

fn exact_uninit<T>(
    length: usize,
    detail: &'static str,
) -> Result<Box<[MaybeUninit<T>]>, ConversionError> {
    if length == 0 {
        return Ok(Box::new([]));
    }
    if std::mem::size_of::<T>() == 0 {
        return Err(ConversionError::Internal {
            detail: "fixed allocation does not accept zero-sized element types".into(),
        });
    }
    let layout =
        std::alloc::Layout::array::<MaybeUninit<T>>(length).map_err(|_| allocation(detail))?;
    // SAFETY: the valid, non-zero layout is transferred into the returned Box.
    let raw = unsafe { std::alloc::alloc(layout) }.cast::<MaybeUninit<T>>();
    if raw.is_null() {
        return Err(allocation(detail));
    }
    // SAFETY: this slice names exactly the allocation above.
    Ok(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(raw, length)) })
}

fn allocation(detail: &'static str) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}

fn allocation_overflow() -> ConversionError {
    allocation("exact string allocation overflowed")
}

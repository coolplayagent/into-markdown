use crate::budget::LayoutBudget;
use crate::memory;
use into_markdown_core::ConversionError;
use std::cmp::Ordering;

#[cfg(test)]
type BeforeSortHook = Option<Box<dyn FnMut()>>;

#[cfg(test)]
std::thread_local! {
    static BEFORE_SORT_HOOK: std::cell::RefCell<BeforeSortHook> = std::cell::RefCell::new(None);
}

/// Sort without an auxiliary allocation after conservatively charging the
/// complete comparison envelope and checking cancellation/deadline.
pub(crate) fn by<T>(
    values: &mut [T],
    budget: &mut LayoutBudget<'_>,
    compare: impl FnMut(&T, &T) -> Ordering,
) -> Result<(), ConversionError> {
    #[cfg(test)]
    BEFORE_SORT_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
    budget.checkpoint_now()?;
    charge(values.len(), budget)?;
    values.sort_unstable_by(compare);
    Ok(())
}

pub(crate) fn comparison_charge(items: usize) -> Result<usize, ConversionError> {
    if items < 2 {
        return Ok(0);
    }
    let levels = usize::BITS - (items - 1).leading_zeros();
    items
        .checked_mul(levels as usize)
        .and_then(|value| value.checked_mul(2))
        .and_then(|value| value.checked_add(items))
        .ok_or_else(|| memory("layout sort work"))
}

fn charge(items: usize, budget: &mut LayoutBudget<'_>) -> Result<(), ConversionError> {
    for _ in 0..comparison_charge(items)? {
        budget.compare()?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn set_before_sort_hook(hook: BeforeSortHook) {
    BEFORE_SORT_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

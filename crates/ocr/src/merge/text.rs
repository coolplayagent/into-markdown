use super::budget::MergeBudget;
use into_markdown_core::ConversionError;

const CHECKPOINT_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextStage {
    Associate,
    NormalizePlan,
    NormalizeCopy,
    LineMaterialize,
}

#[cfg(test)]
type TextTestHook = Option<Box<dyn FnMut(TextStage, usize)>>;

#[cfg(test)]
std::thread_local! {
    static TEXT_TEST_HOOK: std::cell::RefCell<TextTestHook> = std::cell::RefCell::new(None);
}

pub(crate) struct TextMeter<'a, 'context> {
    budget: &'a MergeBudget<'context>,
    #[cfg(test)]
    stage: TextStage,
    pending: usize,
    total: usize,
}

impl<'a, 'context> TextMeter<'a, 'context> {
    pub(crate) const fn new(budget: &'a MergeBudget<'context>, stage: TextStage) -> Self {
        #[cfg(not(test))]
        let _ = stage;
        Self {
            budget,
            #[cfg(test)]
            stage,
            pending: 0,
            total: 0,
        }
    }

    pub(crate) fn consume(&mut self, bytes: usize) -> Result<(), ConversionError> {
        self.pending = self.pending.checked_add(bytes).ok_or_else(super::memory)?;
        self.total = self.total.checked_add(bytes).ok_or_else(super::memory)?;
        while self.pending >= CHECKPOINT_BYTES {
            self.pending -= CHECKPOINT_BYTES;
            #[cfg(test)]
            TEXT_TEST_HOOK.with(|hook| {
                if let Some(hook) = hook.borrow_mut().as_mut() {
                    hook(self.stage, self.total);
                }
            });
            self.budget.checkpoint()?;
        }
        Ok(())
    }
}

pub(crate) fn validated_copy(
    value: &str,
    budget: &MergeBudget<'_>,
) -> Result<Option<String>, ConversionError> {
    let mut output = String::new();
    output.try_reserve_exact(value.len()).map_err(|_| super::memory())?;
    let mut has_visible = false;
    let mut meter = TextMeter::new(budget, TextStage::Associate);
    for character in value.chars() {
        if character.is_control() && !character.is_whitespace() {
            return Err(super::ocr("invalidRecognitionText"));
        }
        has_visible |= !character.is_whitespace();
        output.push(character);
        meter.consume(character.len_utf8())?;
    }
    Ok(has_visible.then_some(output))
}

pub(crate) fn append(
    output: &mut String,
    value: &str,
    meter: &mut TextMeter<'_, '_>,
) -> Result<(), ConversionError> {
    for character in value.chars() {
        output.push(character);
        meter.consume(character.len_utf8())?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn set_test_hook(hook: TextTestHook) {
    TEXT_TEST_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

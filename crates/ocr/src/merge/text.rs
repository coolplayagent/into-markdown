use into_markdown_core::{ConversionError, ExecutionContext};

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

pub(crate) struct TextMeters {
    pub(crate) associate: TextMeter,
    pub(crate) normalize_plan: TextMeter,
    pub(crate) normalize_copy: TextMeter,
    pub(crate) line_materialize: TextMeter,
}

impl TextMeters {
    pub(crate) fn new(context: &ExecutionContext) -> Self {
        Self {
            associate: TextMeter::new(context, TextStage::Associate),
            normalize_plan: TextMeter::new(context, TextStage::NormalizePlan),
            normalize_copy: TextMeter::new(context, TextStage::NormalizeCopy),
            line_materialize: TextMeter::new(context, TextStage::LineMaterialize),
        }
    }
}

pub(crate) struct TextMeter {
    context: ExecutionContext,
    #[cfg(test)]
    stage: TextStage,
    pending: usize,
    total: usize,
}

impl TextMeter {
    fn new(context: &ExecutionContext, stage: TextStage) -> Self {
        #[cfg(not(test))]
        let _ = stage;
        Self {
            context: context.clone(),
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
            self.context.checkpoint()?;
        }
        Ok(())
    }
}

pub(crate) fn validated_copy(
    value: &str,
    meter: &mut TextMeter,
) -> Result<Option<String>, ConversionError> {
    let mut output = String::new();
    output.try_reserve_exact(value.len()).map_err(|_| super::memory())?;
    let mut has_visible = false;
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
    meter: &mut TextMeter,
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

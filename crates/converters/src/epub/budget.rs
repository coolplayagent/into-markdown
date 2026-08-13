//! Request-wide EPUB parser counters.

use into_markdown_core::{
    ConversionError, ConversionOptions, ExecutionContext, ResourceReservation,
};

const MAX_XML_EVENTS: u64 = 1_000_000;
const MAX_ATTRIBUTES_PER_ELEMENT: usize = 4096;
const CHECKPOINT_INTERVAL: u64 = 1024;
const NAVIGATION_URL_CHECKPOINT_BYTES: usize = 4 * 1024;
const MAX_NAVIGATION_URL_VALUE_BYTES: usize = 64 * 1024;
const MAX_NAVIGATION_URL_TOKEN_BYTES: usize = 8 * 1024;
const MAX_NAVIGATION_URL_TOKENS: usize = 4096;

#[cfg(test)]
type NavigationUrlTestHook = Option<Box<dyn FnMut(usize)>>;

#[cfg(test)]
std::thread_local! {
    static NAVIGATION_URL_TEST_HOOK: std::cell::RefCell<NavigationUrlTestHook> =
        std::cell::RefCell::new(None);
}

pub(super) struct EpubBudget<'a> {
    context: &'a ExecutionContext,
    events: u64,
    max_depth: usize,
    max_items: usize,
    max_field_bytes: usize,
    navigation_url_pending_bytes: usize,
    navigation_url_total_bytes: usize,
    navigation_url_tokens: usize,
}

impl<'a> EpubBudget<'a> {
    pub(super) fn new(options: &ConversionOptions, context: &'a ExecutionContext) -> Self {
        Self {
            context,
            events: 0,
            max_depth: usize::from(options.limits.max_nesting_depth),
            max_items: usize::try_from(options.limits.max_archive_entries).unwrap_or(usize::MAX),
            max_field_bytes: usize::try_from(options.limits.max_field_bytes).unwrap_or(usize::MAX),
            navigation_url_pending_bytes: 0,
            navigation_url_total_bytes: 0,
            navigation_url_tokens: 0,
        }
    }

    pub(super) fn event(&mut self, depth: usize) -> Result<(), ConversionError> {
        self.events = self.events.checked_add(1).ok_or_else(|| limit("epub_xml_events"))?;
        if self.events > MAX_XML_EVENTS {
            return Err(limit("epub_xml_events"));
        }
        if depth > self.max_depth {
            return Err(ConversionError::ResourceLimit {
                limit: "max_nesting_depth",
                detail: format!("EPUB XML depth {depth} exceeds {}", self.max_depth),
            });
        }
        if self.events.is_multiple_of(CHECKPOINT_INTERVAL) {
            self.context.checkpoint()?;
        }
        Ok(())
    }

    pub(super) fn attributes(count: usize) -> Result<(), ConversionError> {
        if count > MAX_ATTRIBUTES_PER_ELEMENT {
            return Err(ConversionError::ResourceLimit {
                limit: "epub_xml_attributes",
                detail: format!(
                    "EPUB XML element has {count} attributes, maximum is {MAX_ATTRIBUTES_PER_ELEMENT}"
                ),
            });
        }
        Ok(())
    }

    pub(super) fn items(&self, label: &'static str, count: usize) -> Result<(), ConversionError> {
        if count > self.max_items {
            return Err(ConversionError::ResourceLimit {
                limit: "max_archive_entries",
                detail: format!("EPUB {label} count {count} exceeds {}", self.max_items),
            });
        }
        Ok(())
    }

    pub(super) fn checkpoint(&self) -> Result<(), ConversionError> {
        self.context.checkpoint()
    }

    pub(super) fn field(&self, label: &'static str, bytes: usize) -> Result<(), ConversionError> {
        if bytes > self.max_field_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_field_bytes",
                detail: format!("EPUB {label} exceeds {} bytes", self.max_field_bytes),
            });
        }
        Ok(())
    }

    /// Preflight one navigation URL/IRI attribute before URI parsing allocates.
    pub(super) fn navigation_url_value(
        &mut self,
        bytes: usize,
    ) -> Result<ResourceReservation, ConversionError> {
        self.field("navigation URL attribute", bytes)?;
        if bytes > MAX_NAVIGATION_URL_VALUE_BYTES {
            return Err(ConversionError::ResourceLimit {
                limit: "epub_navigation_url_bytes",
                detail: format!(
                    "EPUB navigation URL attribute has {bytes} bytes; maximum is {MAX_NAVIGATION_URL_VALUE_BYTES}"
                ),
            });
        }
        self.consume_navigation_url_bytes(bytes)?;
        let scratch = bytes
            .checked_mul(2)
            .and_then(|value| value.checked_add(256))
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "EPUB navigation URL scratch plan overflowed".into(),
            })?;
        self.context.reserve_memory(scratch)
    }

    /// Count a single URL, IRI, CURIE, or srcset/ping candidate request-wide.
    pub(super) fn navigation_url_token(&mut self, bytes: usize) -> Result<(), ConversionError> {
        if bytes == 0 || bytes > MAX_NAVIGATION_URL_TOKEN_BYTES {
            return Err(ConversionError::ResourceLimit {
                limit: "epub_navigation_url_token_bytes",
                detail: format!(
                    "EPUB navigation URL token has {bytes} bytes; maximum is {MAX_NAVIGATION_URL_TOKEN_BYTES}"
                ),
            });
        }
        self.navigation_url_tokens =
            self.navigation_url_tokens.checked_add(1).ok_or_else(|| {
                ConversionError::ResourceLimit {
                    limit: "epub_navigation_url_tokens",
                    detail: "EPUB navigation URL token count overflowed".into(),
                }
            })?;
        if self.navigation_url_tokens > MAX_NAVIGATION_URL_TOKENS
            || self.navigation_url_tokens > self.max_items
        {
            return Err(ConversionError::ResourceLimit {
                limit: "epub_navigation_url_tokens",
                detail: format!(
                    "EPUB navigation URL token count {} exceeds request limit {}",
                    self.navigation_url_tokens,
                    MAX_NAVIGATION_URL_TOKENS.min(self.max_items)
                ),
            });
        }
        Ok(())
    }

    fn consume_navigation_url_bytes(&mut self, bytes: usize) -> Result<(), ConversionError> {
        self.navigation_url_pending_bytes = self
            .navigation_url_pending_bytes
            .checked_add(bytes)
            .ok_or_else(|| limit("epub_navigation_url_bytes"))?;
        self.navigation_url_total_bytes = self
            .navigation_url_total_bytes
            .checked_add(bytes)
            .ok_or_else(|| limit("epub_navigation_url_bytes"))?;
        while self.navigation_url_pending_bytes >= NAVIGATION_URL_CHECKPOINT_BYTES {
            self.navigation_url_pending_bytes -= NAVIGATION_URL_CHECKPOINT_BYTES;
            #[cfg(test)]
            NAVIGATION_URL_TEST_HOOK.with(|hook| {
                if let Some(hook) = hook.borrow_mut().as_mut() {
                    hook(self.navigation_url_total_bytes);
                }
            });
            self.context.checkpoint()?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn set_navigation_url_test_hook(hook: NavigationUrlTestHook) {
    NAVIGATION_URL_TEST_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

fn limit(limit_name: &'static str) -> ConversionError {
    ConversionError::ResourceLimit {
        limit: limit_name,
        detail: "EPUB XML event budget exceeded".into(),
    }
}

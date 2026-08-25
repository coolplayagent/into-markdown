#![no_main]

use into_markdown_core::InputFormat;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    into_markdown_legacy_office_fuzz::fuzz(bytes, InputFormat::Xls);
});

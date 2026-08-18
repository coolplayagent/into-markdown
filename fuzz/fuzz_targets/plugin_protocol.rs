#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| into_markdown_cli::fuzz_plugin_protocol(data));

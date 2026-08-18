#![no_main]
mod support;
use into_markdown::InputFormat;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| support::convert(data, InputFormat::Xml, "input.xml"));

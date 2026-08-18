#![no_main]
mod support;
use into_markdown::InputFormat;
use libfuzzer_sys::fuzz_target;

const FORMATS: [(InputFormat, &str); 3] = [
    (InputFormat::Image, "input.png"),
    (InputFormat::Audio, "input.wav"),
    (InputFormat::Video, "input.mp4"),
];

fuzz_target!(|data: &[u8]| {
    let selector = data.first().copied().unwrap_or_default() as usize % FORMATS.len();
    let (format, name) = FORMATS[selector];
    support::convert(data.get(1..).unwrap_or_default(), format, name);
});

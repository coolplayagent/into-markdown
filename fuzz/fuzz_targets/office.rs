#![no_main]
mod support;
use into_markdown::InputFormat;
use libfuzzer_sys::fuzz_target;

const FORMATS: [(InputFormat, &str); 6] = [
    (InputFormat::Docx, "input.docx"),
    (InputFormat::Pptx, "input.pptx"),
    (InputFormat::Xlsx, "input.xlsx"),
    (InputFormat::Odt, "input.odt"),
    (InputFormat::Ods, "input.ods"),
    (InputFormat::Odp, "input.odp"),
];

fuzz_target!(|data: &[u8]| {
    let selector = data.first().copied().unwrap_or_default() as usize % FORMATS.len();
    let (format, name) = FORMATS[selector];
    support::convert(data.get(1..).unwrap_or_default(), format, name);
});

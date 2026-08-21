//! Official isolated PP-OCRv6 capability-provider process.

fn main() -> std::io::Result<()> {
    into_markdown_official_provider::serve_ocr()
}

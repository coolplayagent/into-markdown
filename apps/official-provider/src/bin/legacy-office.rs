//! Official isolated `LibreOffice` compatibility capability-provider process.

fn main() -> std::io::Result<()> {
    into_markdown_official_provider::serve_legacy_office()
}

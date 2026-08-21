//! Official isolated transcription and diarization capability-provider process.

fn main() -> std::io::Result<()> {
    into_markdown_official_provider::serve_media()
}

//! Offline consumer probe for the fixed CommonMark/GFM parser used by QA.
use std::{
    env, fs,
    io::{self, Write},
    process::ExitCode,
};

fn main() -> ExitCode {
    let result = (|| -> io::Result<()> {
        let path =
            env::args_os().nth(1).ok_or_else(|| io::Error::other("usage: inspect <markdown>"))?;
        let markdown = fs::read_to_string(path)?;
        let options = pulldown_cmark::Options::ENABLE_TABLES
            | pulldown_cmark::Options::ENABLE_STRIKETHROUGH
            | pulldown_cmark::Options::ENABLE_TASKLISTS
            | pulldown_cmark::Options::ENABLE_FOOTNOTES;
        let mut html = String::new();
        pulldown_cmark::html::push_html(
            &mut html,
            pulldown_cmark::Parser::new_ext(&markdown, options),
        );
        io::stdout().lock().write_all(html.as_bytes())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

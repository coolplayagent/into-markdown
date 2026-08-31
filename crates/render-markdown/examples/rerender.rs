//! Rerender a validated result DTO without repeating document extraction.
use into_markdown_core::{ConversionOptions, ConversionResult, ResultDto};
use std::{
    env, fs,
    io::{self, Write},
    process::ExitCode,
};

fn main() -> ExitCode {
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let path =
            env::args_os().nth(1).ok_or("usage: rerender <result.json> [asset-uri-prefix]")?;
        let result = ConversionResult::try_from(ResultDto::from_json(&fs::read_to_string(path)?)?)?;
        let mut options = ConversionOptions::default();
        options.output.asset_uri_prefix = env::args().nth(2);
        let markdown =
            into_markdown_render_markdown::render(&result.document, &result.assets, &options)?;
        io::stdout().lock().write_all(markdown.as_bytes())?;
        Ok(())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

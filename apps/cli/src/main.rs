//! `into-md` command-line shell.

use into_markdown::{ConversionRequest, FormatHint, InputFormat, InputRef};
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

const HELP: &str = "\
into-md - convert documents into GitHub-Flavored Markdown

USAGE:
    into-md convert <path|URI|-> [--format FORMAT] [-o OUTPUT]
    into-md formats
    into-md models
    into-md plugins
    into-md --help
    into-md --version

The current repository is an architecture scaffold. Format converters, OCR
inference, network resolution, and AI calls are intentionally not implemented.
";

#[derive(Debug)]
enum CliError {
    Usage(String),
    Conversion(into_markdown::ConversionError),
    Io(String),
}

impl CliError {
    fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            Self::Conversion(_) => 3,
            Self::Io(_) => 4,
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(message) | Self::Io(message) => formatter.write_str(message),
            Self::Conversion(error) => write!(formatter, "{}: {error}", error.code().as_str()),
        }
    }
}

fn main() {
    let mut stdout = std::io::stdout().lock();
    if let Err(error) = run(std::env::args_os().skip(1), &mut stdout) {
        eprintln!("into-md: {error}");
        std::process::exit(error.exit_code());
    }
}

fn run(
    arguments: impl IntoIterator<Item = OsString>,
    output: &mut dyn Write,
) -> Result<(), CliError> {
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        output.write_all(HELP.as_bytes()).map_err(CliError::from)?;
        return Ok(());
    };
    match command.to_string_lossy().as_ref() {
        "-h" | "--help" | "help" => output.write_all(HELP.as_bytes()).map_err(CliError::from),
        "-V" | "--version" | "version" => {
            writeln!(output, "into-md {}", env!("CARGO_PKG_VERSION")).map_err(CliError::from)
        }
        "formats" => list_formats(output),
        "models" => list_models(output),
        "plugins" => list_plugins(output),
        "convert" => convert(arguments.collect(), output),
        unknown => Err(CliError::Usage(format!("unknown command: {unknown}\n\n{HELP}"))),
    }
}

fn list_formats(output: &mut dyn Write) -> Result<(), CliError> {
    writeln!(output, "FORMAT\tFAMILY\tSTATUS\tEXTENSIONS").map_err(CliError::from)?;
    for descriptor in into_markdown::planned_formats() {
        writeln!(
            output,
            "{}\t{}\t{}\t{}",
            descriptor.format,
            descriptor.family,
            descriptor.status.as_str(),
            descriptor.extensions.join(",")
        )
        .map_err(CliError::from)?;
    }
    Ok(())
}

fn list_models(output: &mut dyn Write) -> Result<(), CliError> {
    let manifest = into_markdown::model_manifest().map_err(CliError::Conversion)?;
    writeln!(output, "MODEL\tDEFAULT\tRUNTIME\tLANGUAGES\tSTATUS").map_err(CliError::from)?;
    for bundle in manifest.bundles {
        writeln!(
            output,
            "{}\t{}\t{}\t{}\tplanned",
            bundle.id,
            bundle.id == manifest.default_bundle,
            bundle.runtime_format,
            bundle.languages.join(",")
        )
        .map_err(CliError::from)?;
    }
    Ok(())
}

fn list_plugins(output: &mut dyn Write) -> Result<(), CliError> {
    writeln!(output, "PLUGIN\tSTATUS").map_err(CliError::from)?;
    for provider in into_markdown::planned_ai_providers() {
        writeln!(output, "{}\t{}", provider.id, provider.status).map_err(CliError::from)?;
    }
    Ok(())
}

fn convert(arguments: Vec<OsString>, output: &mut dyn Write) -> Result<(), CliError> {
    let mut iterator = arguments.into_iter();
    let source = iterator
        .next()
        .ok_or_else(|| CliError::Usage("convert requires a path, URI, or '-'".into()))?;
    let mut format = None;
    let mut destination = None;
    while let Some(argument) = iterator.next() {
        match argument.to_string_lossy().as_ref() {
            "--format" => {
                let value = iterator
                    .next()
                    .ok_or_else(|| CliError::Usage("--format requires a value".into()))?;
                format = parse_format(&value.to_string_lossy());
                if format.is_none() {
                    return Err(CliError::Usage(format!(
                        "unknown format: {}",
                        value.to_string_lossy()
                    )));
                }
            }
            "-o" | "--output" => {
                destination = Some(PathBuf::from(
                    iterator
                        .next()
                        .ok_or_else(|| CliError::Usage("--output requires a path".into()))?,
                ));
            }
            unknown => return Err(CliError::Usage(format!("unknown convert option: {unknown}"))),
        }
    }

    let source_text = source.to_string_lossy();
    let input = if source_text == "-" {
        InputRef::Stdin
    } else if source_text.starts_with("http://") || source_text.starts_with("https://") {
        InputRef::Uri(source_text.into_owned())
    } else {
        InputRef::Path(PathBuf::from(source))
    };
    let mut request = ConversionRequest::new(input);
    request.hint = FormatHint { format, ..FormatHint::default() };
    let engine = into_markdown::default_engine().map_err(CliError::Conversion)?;
    let result =
        futures::executor::block_on(engine.convert(request)).map_err(CliError::Conversion)?;
    if let Some(path) = destination {
        std::fs::write(path, result.markdown).map_err(CliError::from)
    } else {
        output.write_all(result.markdown.as_bytes()).map_err(CliError::from)
    }
}

fn parse_format(value: &str) -> Option<InputFormat> {
    into_markdown::planned_formats()
        .iter()
        .find(|descriptor| descriptor.format.as_str() == value.to_ascii_lowercase())
        .map(|descriptor| descriptor.format)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invoke(arguments: &[&str]) -> Result<String, CliError> {
        let mut output = Vec::new();
        run(arguments.iter().map(OsString::from), &mut output)?;
        Ok(String::from_utf8(output).unwrap())
    }

    #[test]
    fn help_smoke_test() {
        assert!(invoke(&["--help"]).unwrap().contains("USAGE:"));
    }

    #[test]
    fn version_smoke_test() {
        assert!(invoke(&["--version"]).unwrap().starts_with("into-md "));
    }

    #[test]
    fn formats_are_explicitly_planned() {
        let output = invoke(&["formats"]).unwrap();
        assert!(output.contains("pdf\tdocument\tplanned"));
        assert!(output.contains("image\tmedia\tplanned"));
    }

    #[test]
    fn converter_absence_has_stable_exit_class() {
        let path = std::env::temp_dir()
            .join(format!("into-markdown-no-converter-{}.pdf", std::process::id()));
        std::fs::write(&path, b"%PDF-scaffold").unwrap();
        let mut output = Vec::new();
        let error = run([OsString::from("convert"), path.clone().into_os_string()], &mut output)
            .unwrap_err();
        std::fs::remove_file(path).unwrap();
        assert_eq!(error.exit_code(), 3);
        assert!(matches!(
            error,
            CliError::Conversion(ref source)
                if source.code() == into_markdown::ErrorCode::NoConverter
        ));
    }
}

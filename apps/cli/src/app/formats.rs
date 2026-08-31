use super::{CliError, find_format, write_json};
use serde::Serialize;
use std::io::Write;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FormatView<'a> {
    format: &'a str,
    family: &'a str,
    status: &'a str,
    source: &'a str,
    extensions: &'a [&'a str],
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime_component: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    install_hint: Option<&'a str>,
}

pub(super) fn list_formats(
    family: Option<&str>,
    status: Option<&str>,
    json: bool,
    stdout: &mut dyn Write,
) -> Result<(), CliError> {
    let views = into_markdown::format_catalog()
        .iter()
        .filter(|entry| family.is_none_or(|family| entry.descriptor.family == family))
        .filter(|entry| status.is_none_or(|status| entry.descriptor.status.as_str() == status))
        .map(|entry| FormatView {
            format: entry.descriptor.format.as_str(),
            family: entry.descriptor.family,
            status: entry.descriptor.status.as_str(),
            source: entry.source.as_str(),
            extensions: entry.descriptor.extensions,
            runtime_component: entry.runtime.map(|runtime| runtime.component),
            install_hint: entry.runtime.map(|runtime| runtime.install_hint),
        })
        .collect::<Vec<_>>();
    if json {
        write_json(stdout, &views)
    } else {
        writeln!(stdout, "FORMAT\tFAMILY\tSTATUS\tSOURCE\tRUNTIME\tEXTENSIONS")?;
        for view in views {
            writeln!(
                stdout,
                "{}\t{}\t{}\t{}\t{}\t{}",
                view.format,
                view.family,
                view.status,
                view.source,
                view.runtime_component.unwrap_or("-"),
                view.extensions.join(",")
            )?;
        }
        Ok(())
    }
}

pub(super) fn show_format(value: &str, json: bool, stdout: &mut dyn Write) -> Result<(), CliError> {
    let entry =
        find_format(value).ok_or_else(|| CliError::usage(format!("unknown format '{value}'")))?;
    let descriptor = entry.descriptor;
    let view = FormatView {
        format: descriptor.format.as_str(),
        family: descriptor.family,
        status: descriptor.status.as_str(),
        source: entry.source.as_str(),
        extensions: descriptor.extensions,
        runtime_component: entry.runtime.map(|runtime| runtime.component),
        install_hint: entry.runtime.map(|runtime| runtime.install_hint),
    };
    if json {
        write_json(stdout, &view)
    } else {
        writeln!(stdout, "format: {}", view.format)?;
        writeln!(stdout, "family: {}", view.family)?;
        writeln!(stdout, "status: {}", view.status)?;
        writeln!(stdout, "source: {}", view.source)?;
        if let Some(component) = view.runtime_component {
            writeln!(stdout, "runtime: {component}")?;
            writeln!(stdout, "install hint: {}", view.install_hint.unwrap_or_default())?;
        }
        if descriptor.status == into_markdown::FormatStatus::Unsupported {
            writeln!(
                stdout,
                "guidance: RAR 归档请先解压后再转换 / extract the archive before conversion"
            )?;
        }
        writeln!(stdout, "extensions: {}", view.extensions.join(", "))?;
        Ok(())
    }
}

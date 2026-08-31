//! Process-plugin execution shared with the CLI management command.
use super::*;

pub(super) fn run_process(
    manager: &PluginManager,
    id: &str,
    input: &Path,
    input_format: &str,
    source: &[u8],
    execution: &into_markdown::ExecutionContext,
) -> Result<Vec<u8>, CliError> {
    let prepared = manager
        .process_manifest(id, into_markdown_process_plugin::RuntimePolicy::default(), execution)
        .map_err(plugin_manager_error)?;
    let result = prepared
        .execute(
            into_markdown_process_plugin::PluginRequest {
                memory_limit: None,
                request_id: "cli-plugin-run",
                input_format,
                source_name: input.file_name().and_then(OsStr::to_str),
                parameters_json: None,
                source,
            },
            execution,
        )
        .map_err(process_plugin_error)?;
    output::encode_result(&result.result, EmitKind::ResultJson)
}

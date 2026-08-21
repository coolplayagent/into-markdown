//! Small embedded catalog for human-facing CLI text.

use crate::args::Language;
use std::ffi::OsString;

/// Resolve the requested language before clap handles `--help`.
pub fn requested_language(arguments: &[OsString]) -> Language {
    let mut values = arguments.iter().map(|value| value.to_string_lossy());
    while let Some(value) = values.next() {
        if value == "--language" {
            if values.next().as_deref() == Some("zh-CN") {
                return Language::ZhCn;
            }
        } else if value == "--language=zh-CN" {
            return Language::ZhCn;
        }
    }
    if std::env::var("INTO_MD_LANGUAGE").ok().as_deref() == Some("zh-CN") {
        Language::ZhCn
    } else {
        Language::En
    }
}

/// Resolve an explicit JSON diagnostic request before clap or configuration loading completes.
pub fn requested_json_log(arguments: &[OsString]) -> bool {
    let mut values = arguments.iter().map(|value| value.to_string_lossy());
    while let Some(value) = values.next() {
        if value == "--log-format" {
            if values.next().as_deref() == Some("json") {
                return true;
            }
        } else if value == "--log-format=json" {
            return true;
        }
    }
    false
}

/// Human-message catalog.
#[derive(Debug, Clone, Copy)]
pub struct Catalog {
    language: Language,
}

impl Catalog {
    pub const fn new(language: Language) -> Self {
        Self { language }
    }

    pub const fn error_prefix(self) -> &'static str {
        match self.language {
            Language::En => "into-md error",
            Language::ZhCn => "into-md 错误",
        }
    }

    pub const fn warning_prefix(self) -> &'static str {
        match self.language {
            Language::En => "warning",
            Language::ZhCn => "警告",
        }
    }
}

/// Return localized help when Chinese was explicitly selected.
pub fn localized_help(arguments: &[OsString], language: Language) -> Option<&'static str> {
    if language != Language::ZhCn
        || !arguments.iter().any(|value| matches!(value.to_str(), Some("-h" | "--help")))
    {
        return None;
    }
    let command = arguments.iter().filter_map(|value| value.to_str()).find(|value| {
        matches!(
            *value,
            "ui" | "formats"
                | "models"
                | "providers"
                | "plugins"
                | "config"
                | "doctor"
                | "completions"
                | "version"
        )
    });
    Some(match command {
        Some("ui") => ZH_UI_HELP,
        Some("formats") => ZH_FORMATS_HELP,
        Some("models") => ZH_MODELS_HELP,
        Some("providers") => ZH_PROVIDERS_HELP,
        Some("plugins") => ZH_PLUGINS_HELP,
        Some("config") => ZH_CONFIG_HELP,
        Some("doctor") => ZH_DOCTOR_HELP,
        Some("completions") => ZH_COMPLETIONS_HELP,
        Some("version") => ZH_VERSION_HELP,
        _ => ZH_ROOT_HELP,
    })
}

const ZH_ROOT_HELP: &str = "\
将文档转换为 Markdown、结构化 IR 或可移植资源包

用法：into-md [选项] [输入...]
      into-md <命令> [选项]

命令：
  ui           启动仅限本机的 Web 服务入口
  formats      查看格式与执行格式检测
  models       查看和管理本地 OCR 模型
  providers    配置和检查 AI 提供者
  plugins      查看和管理隔离插件
  config       查看和编辑分层配置
  doctor       检查本地配置与运行时
  completions  生成 Shell 补全
  version      显示详细版本信息

常用选项：
  -o, --output <路径>       单输入输出路径
      --output-dir <目录>   批量输出目录
  -r, --recursive          递归处理目录
      --emit <类型>        markdown、ir-json、result-json 或 bundle
      --ocr <策略>         off、auto 或 always
      --allow-network      本次调用显式允许联网
      --language <语言>    en 或 zh-CN
  -h, --help               显示帮助
  -V, --version            显示版本
";

const ZH_FORMATS_HELP: &str = "查看格式能力\n\n用法：into-md formats [--family <类别>] [--status <状态>] [--json]\n      into-md formats show <格式> [--json]\n      into-md formats detect <输入> [提示选项] [--json]\n";
const ZH_UI_HELP: &str = "启动仅限本机的安全 Web 服务入口\n\n用法：into-md ui [--port <端口>] [--no-open] [--data-dir <目录>]\n";
const ZH_MODELS_HELP: &str = "查看和管理 OCR 模型\n\n用法：into-md models [--json]\n      into-md models <show|install|verify|remove|path> ...\n";
const ZH_PROVIDERS_HELP: &str = "配置和检查 AI 提供者\n\n用法：into-md providers [--json]\n      into-md providers <show|add|remove|set-default|capabilities|test> ...\n";
const ZH_PLUGINS_HELP: &str = "查看和管理进程外或 WASI 插件\n\n用法：into-md plugins [--json]\n      into-md plugins <show|install|verify|enable|disable|remove> ...\n";
const ZH_CONFIG_HELP: &str = "查看和编辑分层配置\n\n用法：into-md config <paths|show|init|validate|get|set|unset|profile> ...\n";
const ZH_DOCTOR_HELP: &str =
    "检查配置、模型、运行时、插件和提供者环境\n\n用法：into-md doctor [--json] [--allow-network]\n";
const ZH_COMPLETIONS_HELP: &str =
    "生成 Shell 补全\n\n用法：into-md completions <bash|zsh|fish|powershell|elvish>\n";
const ZH_VERSION_HELP: &str = "显示版本和目标平台\n\n用法：into-md version [--json]\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_help_is_selected_before_clap() {
        let args = [
            OsString::from("formats"),
            OsString::from("--help"),
            OsString::from("--language=zh-CN"),
        ];
        assert!(localized_help(&args, requested_language(&args)).unwrap().contains("查看格式"));
    }

    #[test]
    fn json_log_is_detected_before_clap() {
        assert!(requested_json_log(&[OsString::from("--log-format=json")]));
        assert!(requested_json_log(&[OsString::from("--log-format"), OsString::from("json"),]));
    }
}

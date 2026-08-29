import type { Locale } from "./i18n";

const pluginNames: Record<string, [string, string]> = {
  "official.ocr.ppocrv6": ["本地 OCR（PP-OCR）", "Local OCR (PP-OCR)"],
  "official.media.whisper": ["本地语音（Whisper）", "Local speech (Whisper)"],
};

export function capabilitySourceLabel(source: string | undefined, locale: Locale,
  friendlyName?: string): string {
  if (!source || source === "off") return locale === "zh-CN" ? "未启用" : "Off";
  if (source === "core:ocr") return locale === "zh-CN" ? "内置 OCR" : "Built-in OCR";
  if (friendlyName && friendlyName !== source && !friendlyName.startsWith("plugin:")
    && !friendlyName.startsWith("provider:")) return friendlyName;
  const [, identity = source] = source.split(":", 2);
  const name = identity.split("/", 1)[0] ?? identity;
  if (source.startsWith("plugin:")) {
    return pluginNames[name]?.[locale === "zh-CN" ? 0 : 1]
      ?? (locale === "zh-CN" ? "本地插件" : "Local plugin");
  }
  return `${locale === "zh-CN" ? "远端 AI 服务" : "Remote AI service"} · ${name}`;
}

import {
  Archive, File, FileAudio, FileImage, FileJson, FileSpreadsheet, FileText, Presentation,
  type LucideIcon,
} from "lucide-react";
import type { ApiClient, ArtifactReference, InputFormat, TaskRecord } from "./api";
import type { Locale, MessageKey } from "./i18n";

export const TERMINAL = new Set(["succeeded", "failed", "interrupted", "cancelled"]);

export async function listAllTasks(api: ApiClient, signal?: AbortSignal): Promise<TaskRecord[]> {
  const tasks: TaskRecord[] = [];
  let after: { updatedAtMs: number; id: string } | undefined;
  const seen = new Set<string>();
  do {
    const page = await api.listTasks({ limit: 100, ...(after ? { after } : {}) }, signal);
    tasks.push(...page.tasks);
    if (!page.nextCursor) break;
    const key = `${page.nextCursor.updatedAtMs}:${page.nextCursor.id}`;
    if (seen.has(key)) break;
    seen.add(key);
    after = page.nextCursor;
  } while (!signal?.aborted);
  return tasks;
}

export const FORMATS: InputFormat[] = [
  "pdf", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "odt", "ods", "odp", "rtf",
  "epub", "text", "markdown", "html", "csv", "tsv", "json", "xml", "drawio", "feed", "ipynb",
  "image", "zip", "outlook-msg",
];

// Keep this allowlist aligned with the core format catalog. The file picker's
// `accept` attribute is only a hint, so drag-and-drop is checked against the
// same map before any task is created.
const FORMAT_BY_EXTENSION: Readonly<Record<string, InputFormat>> = {
  pdf: "pdf", doc: "doc", docx: "docx", docm: "docx",
  ppt: "ppt", pps: "ppt", pot: "ppt",
  pptx: "pptx", pptm: "pptx", ppsx: "pptx", ppsm: "pptx", potx: "pptx",
  xls: "xls", xlsx: "xlsx", xlsm: "xlsx", xlsb: "xlsx",
  odt: "odt", ods: "ods", odp: "odp", rtf: "rtf", epub: "epub",
  txt: "text", text: "text", log: "text",
  md: "markdown", markdown: "markdown", mdown: "markdown",
  html: "html", htm: "html", csv: "csv", tsv: "tsv", json: "json", xml: "xml",
  drawio: "drawio", rss: "feed", atom: "feed", ipynb: "ipynb",
  png: "image", jpg: "image", jpeg: "image", tif: "image", tiff: "image", webp: "image", bmp: "image",
  zip: "zip", rar: "rar", msg: "outlook-msg",
  wav: "audio", mp3: "audio", m4a: "audio", flac: "audio", ogg: "audio",
  mp4: "video", mkv: "video", webm: "video", avi: "video", mov: "video",
};

export const SUPPORTED_FILE_ACCEPT = Object.entries(FORMAT_BY_EXTENSION)
  .filter(([, format]) => format !== "audio" && format !== "video")
  .map(([extension]) => `.${extension}`)
  .join(",");

export const MEETING_FILE_ACCEPT = Object.entries(FORMAT_BY_EXTENSION)
  .filter(([, format]) => format === "audio" || format === "video")
  .map(([extension]) => `.${extension}`)
  .join(",");

export function supportsMeetingFile(name: string): boolean {
  const format = FORMAT_BY_EXTENSION[fileExtension(name)];
  return format === "audio" || format === "video";
}

const WORKBENCH_FORMATS = new Set<InputFormat>(FORMATS);

function fileExtension(name: string): string {
  const leaf = name.toLocaleLowerCase("en-US").split(/[\\/]/).pop() ?? "";
  const dot = leaf.lastIndexOf(".");
  return dot >= 0 && dot + 1 < leaf.length ? leaf.slice(dot + 1) : "";
}

export function supportsFileName(name: string, hint: InputFormat | null = null): boolean {
  return hint !== null ? WORKBENCH_FORMATS.has(hint) : (fileExtension(name) === "rar" || WORKBENCH_FORMATS.has(FORMAT_BY_EXTENSION[fileExtension(name)]!));
}

export function formatForName(name: string, hint: InputFormat | null = null): string {
  if (hint) return hint;
  return FORMAT_BY_EXTENSION[fileExtension(name)] ?? "auto";
}

const DIAGNOSTIC_MESSAGES: Readonly<Record<string, MessageKey>> = {
  archiveExtractionRequired: "archiveExtractionRequired",
  unsupported: "unsupportedFormatFailure",
  noConverter: "unsupportedFormatFailure",
  malformed: "malformedInputFailure",
  encrypted: "encryptedInputFailure",
  resourceLimit: "resourceLimitFailure",
  ocr: "ocrFailure",
  ocrRecognitionMemory: "ocrRecognitionMemoryFailure",
  "ocr.optionalRecognitionMemorySkipped": "ocrMemoryOmission",
  "ocr.optionalRecognitionResourceSkipped": "ocrResourceOmission",
  ai: "aiFailure",
  network: "networkFailure",
  networkAuthorizationRequired: "authorizationRequired",
  providerSecretMissing: "providerUnavailableFailure",
  privateNetworkDenied: "networkPolicyFailure",
  io: "ioFailure",
  componentUnavailable: "componentUnavailableFailure",
  cancelled: "cancelled",
  timeout: "timeoutFailure",
  recovery: "recoveryFailure",
  recoveryCheckpointMissing: "recoveryFailure",
  recoveryCheckpointInvalid: "recoveryFailure",
  recoveryCheckpointIncompatible: "recoveryFailure",
  internal: "internalFailure",
  conversionFailed: "conversionFailedReason",
  invalidTaskOptions: "invalidOptionsFailure",
  unreachable: "unreachableFailure",
};

export function diagnosticLabel(code: string, t: (key: MessageKey) => string): string {
  if (/^resource\.[A-Za-z0-9_]+\.(?:limitRaised|unitOmitted|sequenceTruncated)$/.test(code)) {
    return t(code.endsWith("limitRaised") ? "resourceLimitFailure" : "ocrResourceOmission");
  }
  return t(DIAGNOSTIC_MESSAGES[code] ?? "conversionFailedReason");
}

export function taskFailureLabel(task: TaskRecord, t: (key: MessageKey) => string): string {
  return diagnosticLabel(taskFailureCode(task), t);
}

export function executionStageLabel(stage: string, locale: Locale): string {
  const labels: Readonly<Record<string, [string, string]>> = {
    resolving: ["准备文件", "Preparing file"],
    detecting: ["识别格式", "Identifying format"],
    probing: ["选择转换方式", "Selecting converter"],
    converting: ["转换内容", "Converting content"],
    ocr: ["识别图片文字", "Reading image text"],
    ai: ["处理内容", "Processing content"],
    rendering: ["生成 Markdown", "Generating Markdown"],
    completed: ["完成", "Completed"],
  };
  return (labels[stage] ?? ["处理中", "Processing"])[locale === "zh-CN" ? 0 : 1];
}

export function iconForFormat(format: string): LucideIcon {
  if (["pdf", "doc", "docx", "rtf", "odt"].includes(format)) return FileText;
  if (["ppt", "pptx", "odp"].includes(format)) return Presentation;
  if (["xls", "xlsx", "ods", "csv", "tsv"].includes(format)) return FileSpreadsheet;
  if (format === "image") return FileImage;
  if (format === "audio" || format === "video") return FileAudio;
  if (["zip", "rar", "epub", "outlook-msg"].includes(format)) return Archive;
  if (format === "json" || format === "ipynb") return FileJson;
  return File;
}

export function taskName(task: TaskRecord, fallback: string): string {
  return task.displayName ?? fallback;
}

export function taskFormat(task: TaskRecord): string {
  return task.format ?? formatForName(task.displayName ?? "");
}

export function bytesLabel(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1048576).toFixed(1)} MiB`;
}

export function artifactLabel(artifact: ArtifactReference): string {
  return artifact.filename ?? ({
    markdown: "result.md",
    documentIr: "document-ir.json",
    diagnostics: "diagnostics.json",
    bundle: "result.zip",
    asset: artifact.assetId ?? "asset",
  } as const)[artifact.kind];
}

export async function downloadArtifact(api: ApiClient, task: TaskRecord, key: string): Promise<void> {
  const result = await api.download(task.id, key);
  const url = URL.createObjectURL(result.blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = result.filename;
  anchor.click();
  URL.revokeObjectURL(url);
}

export function createBatchId(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function taskFailureCode(task: TaskRecord): string {
  const reason = task.failure?.reasonCode;
  if (reason && Object.hasOwn(DIAGNOSTIC_MESSAGES, reason)) return reason;
  return task.failure?.code ?? task.diagnostics[0]?.code ?? "conversionFailed";
}

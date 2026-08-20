import {
  Archive, File, FileAudio, FileImage, FileJson, FileSpreadsheet, FileText, Presentation,
  type LucideIcon,
} from "lucide-react";
import type { ApiClient, ArtifactReference, InputFormat, TaskRecord } from "./api";

export const TERMINAL = new Set(["succeeded", "failed", "interrupted", "cancelled"]);

export const FORMATS: InputFormat[] = [
  "pdf", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "odt", "ods", "odp", "rtf",
  "epub", "text", "markdown", "html", "csv", "tsv", "json", "xml", "feed", "ipynb",
  "image", "audio", "video", "zip", "outlook-msg",
];

export function formatForName(name: string, hint: InputFormat | null = null): string {
  if (hint) return hint;
  const extension = name.toLocaleLowerCase("en-US").split(".").pop() ?? "";
  if (FORMATS.includes(extension as InputFormat)) return extension;
  if (["txt", "log"].includes(extension)) return "text";
  if (["md", "mdown"].includes(extension)) return "markdown";
  if (["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff"].includes(extension)) return "image";
  if (["wav", "mp3", "m4a", "flac", "ogg"].includes(extension)) return "audio";
  if (["mp4", "mkv", "webm", "avi", "mov"].includes(extension)) return "video";
  return "auto";
}

export function iconForFormat(format: string): LucideIcon {
  if (["pdf", "doc", "docx", "rtf", "odt"].includes(format)) return FileText;
  if (["ppt", "pptx", "odp"].includes(format)) return Presentation;
  if (["xls", "xlsx", "ods", "csv", "tsv"].includes(format)) return FileSpreadsheet;
  if (format === "image") return FileImage;
  if (format === "audio" || format === "video") return FileAudio;
  if (["zip", "epub", "outlook-msg"].includes(format)) return Archive;
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

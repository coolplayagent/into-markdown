const MAX_RESPONSE_BYTES = 1024 * 1024;
const MAX_EVENT_BYTES = 64 * 1024;
export const MAX_PREVIEW_BYTES = 256 * 1024;

export interface ComponentStatus { available: boolean; code: string; detail: string }
export interface StatusResponse { schemaVersion: 1; localApi: ComponentStatus; documentConsole: ComponentStatus }
export type TaskStatus = "pending" | "running" | "converted" | "succeeded" | "failed" | "interrupted" | "cancelled";
export interface TaskDiagnostic { code: string }
export interface ArtifactReference {
  storageKey: string;
  kind: "markdown" | "documentIr" | "diagnostics" | "asset" | "bundle";
  byteLen: number; sha256: string; assetId?: string | null; mediaType?: string | null;
  filename?: string | null;
}
export interface ArtifactPreview { text: string; truncated: boolean; contentType: string }
export interface ArtifactDownload { blob: Blob; filename: string }
export interface TaskRecord {
  id: string; createdAtMs: number; updatedAtMs: number; status: TaskStatus;
  progressMillionths: number; diagnostics: TaskDiagnostic[]; artifacts: ArtifactReference[];
  pinned: boolean;
  configuration: { schemaVersion: number; ocrEnabled: boolean; preserveLayout: boolean };
}
export interface TaskCursor { updatedAtMs: number; id: string }
export interface TaskPage { tasks: TaskRecord[]; nextCursor?: TaskCursor }
export interface TaskFilters { limit?: number; after?: TaskCursor; status?: TaskStatus; pinned?: boolean }
export interface CleanupSummary { schemaVersion: 1; deletedTasks: number; reclaimedBytes: number }
export interface TaskEvent {
  schemaVersion: 1; sequence: number; taskId: string; kind: "snapshot" | "progress";
  status: TaskStatus; progressMillionths: number; terminal: boolean;
  execution?: { stage: string; basisPoints: number; message?: string | null };
}
export type InputFormat = "pdf" | "doc" | "docx" | "ppt" | "pptx" | "xls" | "xlsx" | "odt" | "ods" | "odp" | "rtf" | "epub" | "text" | "markdown" | "html" | "csv" | "tsv" | "json" | "xml" | "feed" | "ipynb" | "image" | "audio" | "video" | "zip" | "outlook-msg";
export type OcrPolicy = "off" | "auto" | "always";
export type AiMode = "off" | "fallback" | "prefer" | "only";
export type AssetMode = "extract" | "embed" | "omit";
export type NetworkMode = "restricted" | "unrestricted";
export interface WorkbenchOptions {
  format: InputFormat | null; ocrPolicy: OcrPolicy; ocrConfidence: number; aiMode: AiMode;
  assetMode: AssetMode; includeProvenance: boolean; maxInputMiB: number; maxMemoryMiB: number;
  maxTemporaryMiB: number; maxPages: number; networkMode: NetworkMode; authorizeProvider: boolean;
  audioTranscription: boolean;
}
export const defaultWorkbenchOptions: WorkbenchOptions = {
  format: null, ocrPolicy: "auto", ocrConfidence: 0.7, aiMode: "off", assetMode: "extract",
  includeProvenance: true, maxInputMiB: 512, maxMemoryMiB: 256, maxTemporaryMiB: 256,
  maxPages: 10_000, networkMode: "restricted", authorizeProvider: false, audioTranscription: false,
};

export class ApiError extends Error {
  constructor(readonly code: string) { super("The local API request failed."); this.name = "ApiError"; }
}
function isObject(value: unknown): value is Record<string, unknown> { return typeof value === "object" && value !== null; }
function isComponent(value: unknown): value is ComponentStatus {
  return isObject(value) && typeof value.available === "boolean" && typeof value.code === "string" && typeof value.detail === "string";
}
function parseStatus(value: unknown): StatusResponse {
  if (!isObject(value) || value.schemaVersion !== 1 || !isComponent(value.localApi) || !isComponent(value.documentConsole)) throw new ApiError("invalidResponse");
  return value as unknown as StatusResponse;
}
const taskStatuses = new Set<TaskStatus>(["pending", "running", "converted", "succeeded", "failed", "interrupted", "cancelled"]);
function isArtifact(value: unknown): value is ArtifactReference {
  return isObject(value) && typeof value.storageKey === "string" && /^[0-9a-f]{32}$/.test(value.storageKey)
    && ["markdown", "documentIr", "diagnostics", "asset", "bundle"].includes(String(value.kind))
    && Number.isSafeInteger(value.byteLen) && Number(value.byteLen) >= 0 && typeof value.sha256 === "string" && /^[0-9a-f]{64}$/.test(value.sha256)
    && (value.assetId === undefined || value.assetId === null || typeof value.assetId === "string" && value.assetId.length <= 255)
    && (value.filename === undefined || value.filename === null || typeof value.filename === "string" && value.filename.length <= 255 && !/[\u0000-\u001f\u007f/\\]/.test(value.filename))
    && (value.mediaType === undefined || value.mediaType === null || typeof value.mediaType === "string" && value.mediaType.length <= 127 && /^[\x20-\x7e]+$/.test(value.mediaType));
}
export function parseTask(value: unknown): TaskRecord {
  if (!isObject(value) || typeof value.id !== "string" || !/^[0-9a-f]{32}$/.test(value.id)
    || !taskStatuses.has(value.status as TaskStatus) || !Number.isSafeInteger(value.progressMillionths)
    || !Number.isSafeInteger(value.createdAtMs) || !Number.isSafeInteger(value.updatedAtMs) || typeof value.pinned !== "boolean"
    || Number(value.progressMillionths) < 0 || Number(value.progressMillionths) > 1_000_000
    || !Array.isArray(value.diagnostics) || value.diagnostics.length > 1024 || value.diagnostics.some((item) => !isObject(item) || typeof item.code !== "string" || item.code.length > 128)
    || !Array.isArray(value.artifacts) || value.artifacts.length > 128 || value.artifacts.some((artifact) => !isArtifact(artifact))
    || !isObject(value.configuration)) throw new ApiError("invalidResponse");
  return value as unknown as TaskRecord;
}
function parseTaskList(value: unknown): TaskPage {
  if (!isObject(value) || value.schemaVersion !== 1 || !Array.isArray(value.tasks) || value.tasks.length > 100) throw new ApiError("invalidResponse");
  const next = value.nextCursor;
  if (next !== undefined && (!isObject(next) || !Number.isSafeInteger(next.updatedAtMs) || typeof next.id !== "string" || !/^[0-9a-f]{32}$/.test(next.id))) throw new ApiError("invalidResponse");
  return { tasks: value.tasks.map(parseTask), ...(next === undefined ? {} : { nextCursor: next as unknown as TaskCursor }) };
}
function parseTaskEvent(value: unknown): TaskEvent {
  if (!isObject(value) || value.schemaVersion !== 1 || typeof value.taskId !== "string" || !taskStatuses.has(value.status as TaskStatus)
    || (value.kind !== "snapshot" && value.kind !== "progress") || !Number.isSafeInteger(value.sequence)
    || !Number.isSafeInteger(value.progressMillionths) || typeof value.terminal !== "boolean") throw new ApiError("invalidEvent");
  return value as unknown as TaskEvent;
}
async function readBoundedJson(response: Response, limit = MAX_RESPONSE_BYTES): Promise<unknown> {
  const declared = Number(response.headers.get("content-length"));
  if (Number.isFinite(declared) && declared > limit) throw new ApiError("responseTooLarge");
  if (!response.body) throw new ApiError("invalidResponse");
  const reader = response.body.getReader(); const chunks: Uint8Array[] = []; let length = 0;
  try {
    while (true) { const result = await reader.read(); if (result.done) break; length += result.value.byteLength;
      if (length > limit) { await reader.cancel(); throw new ApiError("responseTooLarge"); } chunks.push(result.value); }
  } finally { reader.releaseLock(); }
  const bytes = new Uint8Array(length); let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
  try { return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)); } catch { throw new ApiError("invalidResponse"); }
}
function requestCode(value: unknown): string {
  return isObject(value) && typeof value.code === "string" && /^[A-Za-z][A-Za-z0-9]{0,63}$/.test(value.code) ? value.code : "requestFailed";
}
function base64UrlUtf8(value: string): string {
  const bytes = new TextEncoder().encode(value); let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}
function base64UrlJson(value: unknown): string { return base64UrlUtf8(JSON.stringify(value)); }
function mib(value: number): number { return Math.round(value * 1024 * 1024); }
export function taskRequest(options: WorkbenchOptions): unknown {
  const ai = options.aiMode;
  const unrestrictedNetwork = options.networkMode === "unrestricted";
  return { schemaVersion: 1, format: options.format, options: {
    text: { charset: null, decoding_mode: "strict" }, delimited_text: { header: "auto", ragged_rows: "strict" },
    ocr: { policy: options.ocrPolicy, model_bundle: null, minimum_confidence: options.ocrConfidence },
    asr: { model_bundle: "whisper-small-multilingual", language: null, max_threads: 4, max_duration_ms: 600_000,
      max_segments: 10_000, max_native_memory_bytes: 900 * 1024 * 1024 },
    ai: { vision_ocr: ai, image_description: ai, layout_repair: ai, table_repair: ai, formula_repair: ai, audio_transcription: options.audioTranscription ? "only" : "off", markdown_postprocess: ai },
    network: { enabled: unrestrictedNetwork, max_redirects: 3, deny_private_networks: !unrestrictedNetwork, allowed_hosts: [] },
    limits: { max_input_bytes: mib(options.maxInputMiB), max_decompressed_bytes: 1073741824, max_archive_entries: 100000,
      max_archive_depth: 16, max_archive_entry_bytes: 268435456, max_archive_compression_ratio: 100, max_nesting_depth: 256,
      max_pages: options.maxPages, max_asset_bytes: 67108864, max_total_asset_bytes: 134217728,
      max_memory_bytes: mib(options.maxMemoryMiB), max_temporary_bytes: mib(options.maxTemporaryMiB), max_table_rows: 100000,
      max_table_columns: 16384, max_table_cells: 1000000, max_field_bytes: 16777216, max_feed_entries: 10000,
      max_feed_text_bytes: 67108864, max_feed_html_bytes: 67108864 },
    output: { flavor: "gfm", asset_directory_suffix: "_assets", include_provenance: options.includeProvenance,
      asset_mode: options.assetMode, asset_uri_prefix: null },
  }, authorization: { network: unrestrictedNetwork, privateNetwork: unrestrictedNetwork, provider: options.authorizeProvider || options.audioTranscription } };
}

export interface ApiClient {
  status(signal?: AbortSignal): Promise<StatusResponse>; listTasks(filters?: TaskFilters, signal?: AbortSignal): Promise<TaskPage>;
  getTask(id: string, signal?: AbortSignal): Promise<TaskRecord>;
  upload(file: File, options: WorkbenchOptions, signal?: AbortSignal): Promise<TaskRecord>;
  cancel(id: string, signal?: AbortSignal): Promise<TaskRecord>;
  retry(id: string, signal?: AbortSignal): Promise<TaskRecord>;
  setPinned(id: string, pinned: boolean, signal?: AbortSignal): Promise<TaskRecord>;
  deleteTask(id: string, signal?: AbortSignal): Promise<void>;
  cleanup(signal?: AbortSignal): Promise<CleanupSummary>;
  watchTask(id: string, onEvent: (event: TaskEvent) => void, signal: AbortSignal): Promise<void>;
  preview(id: string, key: string, signal?: AbortSignal): Promise<ArtifactPreview>;
  download(id: string, key: string, signal?: AbortSignal): Promise<ArtifactDownload>;
}
async function readBoundedBytes(response: Response, limit: number): Promise<Uint8Array> {
  const declared = Number(response.headers.get("content-length"));
  if (Number.isFinite(declared) && declared > limit) throw new ApiError("responseTooLarge");
  if (!response.body) throw new ApiError("invalidResponse");
  const reader = response.body.getReader(); const chunks: Uint8Array[] = []; let length = 0;
  try { while (true) { const result = await reader.read(); if (result.done) break; length += result.value.byteLength; if (length > limit) { await reader.cancel(); throw new ApiError("responseTooLarge"); } chunks.push(result.value); } }
  finally { reader.releaseLock(); }
  const bytes = new Uint8Array(length); let offset = 0; for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; } return bytes;
}
export function createApiClient(session: string, fetcher: typeof fetch = fetch): ApiClient {
  const auth = (): Record<string, string> => ({ "X-Into-Md-Session": session });
  async function jsonRequest(path: string, init: RequestInit, limit?: number): Promise<unknown> {
    let response: Response;
    try { response = await fetcher(path, { cache: "no-store", credentials: "omit", redirect: "error", referrerPolicy: "no-referrer", ...init }); }
    catch (error) { if (error instanceof DOMException && error.name === "AbortError") throw error; throw new ApiError("unreachable"); }
    if (response.headers.get("content-type")?.split(";", 1)[0]?.trim() !== "application/json") throw new ApiError("invalidResponse");
    const value = await readBoundedJson(response, limit); if (!response.ok) throw new ApiError(requestCode(value)); return value;
  }
  const client: ApiClient = {
    async status(signal) { return parseStatus(await jsonRequest("/api/status", { method: "POST", headers: auth(), body: null, ...(signal ? { signal } : {}) }, 65536)); },
    async listTasks(filters = {}, signal) {
      const query = new URLSearchParams(); query.set("limit", String(filters.limit ?? 25));
      if (filters.after) { query.set("afterUpdatedAtMs", String(filters.after.updatedAtMs)); query.set("afterId", filters.after.id); }
      if (filters.status) query.set("status", filters.status); if (filters.pinned !== undefined) query.set("pinned", String(filters.pinned));
      return parseTaskList(await jsonRequest(`/api/tasks?${query}`, { method: "GET", headers: auth(), ...(signal ? { signal } : {}) }));
    },
    async getTask(id, signal) { return parseTask(await jsonRequest(`/api/tasks/${id}`, { method: "GET", headers: auth(), ...(signal ? { signal } : {}) })); },
    async upload(file, options, signal) {
      const headers = auth(); headers["X-Into-Md-Filename-B64"] = base64UrlUtf8(file.name); headers["X-Into-Md-Request"] = base64UrlJson(taskRequest(options));
      return parseTask(await jsonRequest("/api/tasks", { method: "POST", headers, body: file, ...(signal ? { signal } : {}) }));
    },
    async cancel(id, signal) { return parseTask(await jsonRequest(`/api/tasks/${id}`, { method: "DELETE", headers: auth(), body: null, ...(signal ? { signal } : {}) })); },
    async retry(id, signal) { return parseTask(await jsonRequest(`/api/tasks/${id}/retry`, { method: "POST", headers: auth(), body: null, ...(signal ? { signal } : {}) })); },
    async setPinned(id, pinned, signal) { const headers = auth(); headers["Content-Type"] = "application/json"; return parseTask(await jsonRequest(`/api/tasks/${id}/pin`, { method: "POST", headers, body: JSON.stringify({ pinned }), ...(signal ? { signal } : {}) })); },
    async deleteTask(id, signal) {
      let response: Response;
      try { response = await fetcher(`/api/tasks/${id}/history`, { method: "DELETE", headers: auth(), cache: "no-store", credentials: "omit", redirect: "error", referrerPolicy: "no-referrer", ...(signal ? { signal } : {}) }); }
      catch (error) { if (error instanceof DOMException && error.name === "AbortError") throw error; throw new ApiError("unreachable"); }
      if (response.status === 204) return;
      if (response.headers.get("content-type")?.split(";", 1)[0]?.trim() !== "application/json") throw new ApiError("invalidResponse");
      const value = await readBoundedJson(response, 65536);
      throw new ApiError(requestCode(value));
    },
    async cleanup(signal) {
      const value = await jsonRequest("/api/tasks/cleanup", { method: "POST", headers: auth(), body: null, ...(signal ? { signal } : {}) }, 65536);
      if (!isObject(value) || value.schemaVersion !== 1 || !Number.isSafeInteger(value.deletedTasks)
        || Number(value.deletedTasks) < 0 || !Number.isSafeInteger(value.reclaimedBytes)
        || Number(value.reclaimedBytes) < 0) throw new ApiError("invalidResponse");
      return value as unknown as CleanupSummary;
    },
    async watchTask(id, onEvent, signal) {
      let lastEventId: string | undefined;
      while (!signal.aborted) {
        const headers = auth(); headers.Accept = "text/event-stream"; if (lastEventId) headers["Last-Event-ID"] = lastEventId;
        let response: Response;
        try { response = await fetcher(`/api/tasks/${id}/events`, { method: "GET", headers, cache: "no-store", credentials: "omit", redirect: "error", referrerPolicy: "no-referrer", signal }); }
        catch { if (signal.aborted) return; await new Promise((resolve) => setTimeout(resolve, 250)); continue; }
        if (!response.ok || !response.body || response.headers.get("content-type")?.split(";", 1)[0] !== "text/event-stream") throw new ApiError("invalidEventStream");
        const reader = response.body.getReader(); const decoder = new TextDecoder("utf-8", { fatal: true }); let buffer = "";
        try {
          while (!signal.aborted) {
            const chunk = await reader.read(); if (chunk.done) break;
            buffer += decoder.decode(chunk.value, { stream: true }).replaceAll("\r\n", "\n");
            if (buffer.length > MAX_EVENT_BYTES) throw new ApiError("eventTooLarge");
            let boundary: number;
            while ((boundary = buffer.indexOf("\n\n")) >= 0) {
              const block = buffer.slice(0, boundary); buffer = buffer.slice(boundary + 2); let data = ""; let eventId: string | undefined;
              for (const line of block.split("\n")) { if (line.startsWith("data: ")) data += line.slice(6); else if (line.startsWith("id: ")) eventId = line.slice(4); }
              if (!data) continue; let parsed: unknown; try { parsed = JSON.parse(data); } catch { throw new ApiError("invalidEvent"); }
              const event = parseTaskEvent(parsed); if (event.taskId !== id || !eventId) throw new ApiError("invalidEvent");
              lastEventId = eventId; onEvent(event); if (event.terminal) return;
            }
          }
        } finally { reader.releaseLock(); }
      }
    },
    async preview(id, key, signal) {
      let response: Response;
      const headers = auth(); headers.Range = `bytes=0-${MAX_PREVIEW_BYTES - 1}`;
      try { response = await fetcher(`/api/tasks/${id}/artifacts/${key}`, { method: "GET", headers, cache: "no-store", credentials: "omit", redirect: "error", referrerPolicy: "no-referrer", ...(signal ? { signal } : {}) }); }
      catch { throw new ApiError("unreachable"); }
      if (response.status !== 200 && response.status !== 206) throw new ApiError("previewFailed");
      const bytes = await readBoundedBytes(response, MAX_PREVIEW_BYTES);
      let text: string; try { text = new TextDecoder("utf-8", { fatal: true }).decode(bytes); } catch { throw new ApiError("invalidPreview"); }
      const declared = Number(response.headers.get("content-range")?.split("/")[1]);
      return { text, truncated: response.status === 206 && Number.isFinite(declared) && declared > bytes.byteLength, contentType: response.headers.get("content-type")?.split(";", 1)[0] ?? "application/octet-stream" };
    },
    async download(id, key, signal) {
      let response: Response;
      try { response = await fetcher(`/api/tasks/${id}/artifacts/${key}`, { method: "GET", headers: auth(), cache: "no-store", credentials: "omit", redirect: "error", referrerPolicy: "no-referrer", ...(signal ? { signal } : {}) }); }
      catch { throw new ApiError("unreachable"); }
      if (!response.ok) throw new ApiError("downloadFailed");
      const disposition = response.headers.get("content-disposition") ?? "";
      const encoded = disposition.match(/filename\*=UTF-8''([^;]+)/i)?.[1];
      const quoted = disposition.match(/filename="([^"]+)"/i)?.[1];
      let filename = quoted ?? "download.bin";
      if (encoded) { try { filename = decodeURIComponent(encoded); } catch { /* keep safe fallback */ } }
      filename = filename.replaceAll("/", "_").replaceAll("\\", "_").replace(/[\u0000-\u001f\u007f]/g, "_").slice(0, 255) || "download.bin";
      return { blob: await response.blob(), filename };
    },
  };
  return Object.freeze(client);
}

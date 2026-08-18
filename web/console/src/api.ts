const MAX_RESPONSE_BYTES = 1024 * 1024;
const MAX_EVENT_BYTES = 64 * 1024;

export interface ComponentStatus { available: boolean; code: string; detail: string }
export interface StatusResponse { schemaVersion: 1; localApi: ComponentStatus; documentConsole: ComponentStatus }
export type TaskStatus = "pending" | "running" | "converted" | "succeeded" | "failed" | "interrupted" | "cancelled";
export interface TaskDiagnostic { code: string }
export interface ArtifactReference {
  storageKey: string;
  kind: "markdown" | "documentIr" | "diagnostics" | "asset" | "bundle";
  byteLen: number;
  filename?: string;
}
export interface TaskRecord {
  id: string; createdAtMs: number; updatedAtMs: number; status: TaskStatus;
  progressMillionths: number; diagnostics: TaskDiagnostic[]; artifacts: ArtifactReference[];
  configuration: { schemaVersion: number; ocrEnabled: boolean; preserveLayout: boolean };
}
export interface TaskEvent {
  schemaVersion: 1; sequence: number; taskId: string; kind: "snapshot" | "progress";
  status: TaskStatus; progressMillionths: number; terminal: boolean;
  execution?: { stage: string; basisPoints: number; message?: string | null };
}
export type InputFormat = "pdf" | "doc" | "docx" | "ppt" | "pptx" | "xls" | "xlsx" | "odt" | "ods" | "odp" | "rtf" | "epub" | "text" | "markdown" | "html" | "csv" | "tsv" | "json" | "xml" | "feed" | "ipynb" | "image" | "audio" | "video" | "zip" | "outlook-msg";
export type OcrPolicy = "off" | "auto" | "always";
export type AiMode = "off" | "fallback" | "prefer" | "only";
export type AssetMode = "extract" | "embed" | "omit";
export interface WorkbenchOptions {
  format: InputFormat | null; ocrPolicy: OcrPolicy; ocrConfidence: number; aiMode: AiMode;
  assetMode: AssetMode; includeProvenance: boolean; maxInputMiB: number; maxMemoryMiB: number;
  maxTemporaryMiB: number; maxPages: number; networkEnabled: boolean; privateNetworkEnabled: boolean;
  allowedHosts: string[]; authorizeNetwork: boolean; authorizePrivateNetwork: boolean; authorizeProvider: boolean;
}
export const defaultWorkbenchOptions: WorkbenchOptions = {
  format: null, ocrPolicy: "auto", ocrConfidence: 0.7, aiMode: "off", assetMode: "extract",
  includeProvenance: true, maxInputMiB: 512, maxMemoryMiB: 256, maxTemporaryMiB: 256,
  maxPages: 10_000, networkEnabled: false, privateNetworkEnabled: false, allowedHosts: [],
  authorizeNetwork: false, authorizePrivateNetwork: false, authorizeProvider: false,
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
export function parseTask(value: unknown): TaskRecord {
  if (!isObject(value) || typeof value.id !== "string" || !/^[0-9a-f]{32}$/.test(value.id)
    || !taskStatuses.has(value.status as TaskStatus) || !Number.isSafeInteger(value.progressMillionths)
    || Number(value.progressMillionths) < 0 || Number(value.progressMillionths) > 1_000_000
    || !Array.isArray(value.diagnostics) || !Array.isArray(value.artifacts) || !isObject(value.configuration)) throw new ApiError("invalidResponse");
  return value as unknown as TaskRecord;
}
function parseTaskList(value: unknown): TaskRecord[] {
  if (!isObject(value) || value.schemaVersion !== 1 || !Array.isArray(value.tasks) || value.tasks.length > 100) throw new ApiError("invalidResponse");
  return value.tasks.map(parseTask);
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
function requestCode(value: unknown): string { return isObject(value) && typeof value.code === "string" ? value.code : "requestFailed"; }
function base64UrlUtf8(value: string): string {
  const bytes = new TextEncoder().encode(value); let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}
function base64UrlJson(value: unknown): string { return base64UrlUtf8(JSON.stringify(value)); }
function mib(value: number): number { return Math.round(value * 1024 * 1024); }
export function taskRequest(options: WorkbenchOptions): unknown {
  const ai = options.aiMode;
  return { schemaVersion: 1, format: options.format, options: {
    text: { charset: null, decoding_mode: "strict" }, delimited_text: { header: "auto", ragged_rows: "strict" },
    ocr: { policy: options.ocrPolicy, model_bundle: null, minimum_confidence: options.ocrConfidence },
    ai: { vision_ocr: ai, image_description: ai, layout_repair: ai, table_repair: ai, formula_repair: ai, audio_transcription: ai, markdown_postprocess: ai },
    network: { enabled: options.networkEnabled, max_redirects: 3, deny_private_networks: !options.privateNetworkEnabled, allowed_hosts: options.allowedHosts },
    limits: { max_input_bytes: mib(options.maxInputMiB), max_decompressed_bytes: 1073741824, max_archive_entries: 100000,
      max_archive_depth: 16, max_archive_entry_bytes: 268435456, max_archive_compression_ratio: 100, max_nesting_depth: 256,
      max_pages: options.maxPages, max_asset_bytes: 67108864, max_total_asset_bytes: 134217728,
      max_memory_bytes: mib(options.maxMemoryMiB), max_temporary_bytes: mib(options.maxTemporaryMiB), max_table_rows: 100000,
      max_table_columns: 16384, max_table_cells: 1000000, max_field_bytes: 16777216, max_feed_entries: 10000,
      max_feed_text_bytes: 67108864, max_feed_html_bytes: 67108864 },
    output: { flavor: "gfm", asset_directory_suffix: "_assets", include_provenance: options.includeProvenance,
      asset_mode: options.assetMode, asset_uri_prefix: null },
  }, authorization: { network: options.authorizeNetwork, privateNetwork: options.authorizePrivateNetwork, provider: options.authorizeProvider } };
}

export interface ApiClient {
  status(signal?: AbortSignal): Promise<StatusResponse>; listTasks(signal?: AbortSignal): Promise<TaskRecord[]>;
  getTask(id: string, signal?: AbortSignal): Promise<TaskRecord>;
  upload(file: File, options: WorkbenchOptions, signal?: AbortSignal): Promise<TaskRecord>;
  cancel(id: string, signal?: AbortSignal): Promise<TaskRecord>;
  watchTask(id: string, onEvent: (event: TaskEvent) => void, signal: AbortSignal): Promise<void>;
  download(id: string, key: string, signal?: AbortSignal): Promise<Blob>;
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
    async listTasks(signal) { return parseTaskList(await jsonRequest("/api/tasks", { method: "GET", headers: auth(), ...(signal ? { signal } : {}) })); },
    async getTask(id, signal) { return parseTask(await jsonRequest(`/api/tasks/${id}`, { method: "GET", headers: auth(), ...(signal ? { signal } : {}) })); },
    async upload(file, options, signal) {
      const headers = auth(); headers["X-Into-Md-Filename-B64"] = base64UrlUtf8(file.name); headers["X-Into-Md-Request"] = base64UrlJson(taskRequest(options));
      return parseTask(await jsonRequest("/api/tasks", { method: "POST", headers, body: file, ...(signal ? { signal } : {}) }));
    },
    async cancel(id, signal) { return parseTask(await jsonRequest(`/api/tasks/${id}`, { method: "DELETE", headers: auth(), body: null, ...(signal ? { signal } : {}) })); },
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
    async download(id, key, signal) {
      let response: Response;
      try { response = await fetcher(`/api/tasks/${id}/artifacts/${key}`, { method: "GET", headers: auth(), cache: "no-store", credentials: "omit", redirect: "error", referrerPolicy: "no-referrer", ...(signal ? { signal } : {}) }); }
      catch { throw new ApiError("unreachable"); }
      if (!response.ok) throw new ApiError("downloadFailed"); return response.blob();
    },
  };
  return Object.freeze(client);
}

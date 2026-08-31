const MAX_RESPONSE_BYTES = 1024 * 1024;
const MAX_EVENT_BYTES = 64 * 1024;
export const MAX_PREVIEW_BYTES = 256 * 1024;

export interface ComponentStatus { available: boolean; code: string; detail: string }
export interface StatusResponse { schemaVersion: 1; localApi: ComponentStatus; documentConsole: ComponentStatus; imageOcr: ComponentStatus; audioTranscription?: ComponentStatus; speakerDiarization?: ComponentStatus }
export interface FormatAdmin { format: string; family: string; status: string; source: string; extensions: string[]; runtimeComponent?: string; installHint?: string }
export interface CapabilityAdmin { id: "ocr" | "transcription" | "diarization"; status: "not-installed" | "downloading" | "verifying" | "ready" | "update-available" | "corrupt" | "incompatible" | "blocked" | "disabled"; localStatus: "not-installed" | "downloading" | "verifying" | "ready" | "update-available" | "corrupt" | "incompatible" | "blocked" | "disabled"; currentSource: string; sources: string[]; version?: string; localVersion?: string }
export type CapabilityQuickStatus = CapabilityAdmin["status"] | "unknown" | "checking";
export interface CapabilityQuickView {
  id: CapabilityAdmin["id"]; name: string; status: CapabilityQuickStatus; localStatus: CapabilityQuickStatus;
  currentSource: string; currentSourceName: string; sources: string[]; version?: string; localVersion?: string;
  lastVerifiedAtMs?: number;
}
export interface CapabilitySnapshot { schemaVersion: 2; generation: number; checking: boolean; checkedAtMs?: number; capabilities: CapabilityQuickView[] }
export interface CapabilityCheck {
  schemaVersion: 1; id: string; capability: string; capabilityName: string; plugin: string; pluginName: string;
  status: "queued" | "running" | "cancelling" | "cancelled" | "completed" | "failed";
  stage: "queued" | "package" | "runtime" | "models" | "cancelling" | "completed";
  progress: number; code?: string; detail?: string; elapsedMs?: number;
}
export interface StagedPluginPackage {
  schemaVersion: 1; source: string; filename: string; byteLen: number; sha256: string;
  officialPluginId?: string; signingKeyId?: string; signingKeySha256?: string;
}
export interface ProviderAdmin { name: string; scope: "global" | "project" | "effective"; actionScope?: "global" | "project"; providerType?: string; baseUrl?: string; model?: string; models: Record<string, string>; apiKeyEnv?: string; environmentSet?: boolean; capabilities: string[]; timeoutMs?: number; allowedHosts: string[]; allowPrivateNetwork: boolean; default: boolean; effective: boolean; shadowedBy?: "effective" }
export interface PluginAdmin { id: string; scope: "global" | "project" | "effective"; actionScope?: "global" | "project"; packageScope?: "global" | "project"; source?: string; sha256?: string; protocol?: string; enabled?: boolean; effective: boolean; shadowedBy?: "effective"; verification?: string; version?: string; signingKeyId?: string; signingKeySha256?: string; target?: string }
export interface DoctorAdmin { id: string; status: string; detail: string }
export type AdminOperationResult = { kind: "detection"; sourceName?: string | null; sourceSize: number; candidates: Array<{ format: string; confidence: number; explicit: boolean; detectorId: string; reason: string; diagnostics: string[] }> } | { kind: "profile"; name: string; value: Record<string, unknown> } | { kind: "config"; operation: "paths" | "get" | "showMerged" | "showResolved"; value: unknown } | { kind: "doctor"; checks: DoctorAdmin[] } | { kind: "providerTest"; configuredModelAvailable: boolean; modelCount: number; capabilities: string[] };
export interface ProfileAdmin { name: string; scope: "global" | "project" | "effective"; effective: boolean; active: boolean; shadowedBy?: "project" }
export interface AdminSnapshot { schemaVersion: 1; formats: FormatAdmin[]; capabilities: CapabilityAdmin[]; providers: ProviderAdmin[]; plugins: PluginAdmin[]; configuration: Record<string, unknown>; profiles: ProfileAdmin[]; doctor: DoctorAdmin[]; operationResult?: AdminOperationResult; configurationReadOnly: boolean }
export interface AdminAction { schemaVersion: 1; action: string; scope?: "global" | "project" | undefined; target?: string; value?: string; source?: string; sha256?: string; signingKeyId?: string; signingKeySha256?: string; providerType?: string; model?: string; models?: Record<string, string>; apiKeyEnv?: string; capabilities?: string[]; timeoutMs?: number; charset?: string; formatHint?: string; extension?: string; mimeType?: string; allowHosts?: string[]; allowPrivateNetwork?: boolean; insecure?: boolean; force?: boolean; resolved?: boolean; from?: string; authorizationGrant?: string; authorizeDangerous?: boolean; authorizeNetwork?: boolean }
export interface AdminActionOutcome { operationResult?: AdminOperationResult }
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
  pinned: boolean; artifactGeneration: number;
  displayName?: string | null; format?: InputFormat | null; batchId?: string | null;
  workflow: "conversion" | "meetingTranscript";
  configuration: { schemaVersion: number; ocrEnabled: boolean; preserveLayout: boolean };
}
export interface SpeakerLabel { id: string; name: string }
export interface SpeakerLabels { schemaVersion: 1; artifactGeneration: number; speakers: SpeakerLabel[] }
export interface TaskCursor { updatedAtMs: number; id: string }
export interface TaskPage { tasks: TaskRecord[]; nextCursor?: TaskCursor }
export interface TaskFilters { limit?: number; after?: TaskCursor; status?: TaskStatus; pinned?: boolean; batchId?: string }
export interface CleanupSummary { schemaVersion: 1; deletedTasks: number; reclaimedBytes: number }
export interface TaskEvent {
  schemaVersion: 1; sequence: number; taskId: string; kind: "snapshot" | "progress";
  status: TaskStatus; progressMillionths: number; terminal: boolean;
  execution?: { stage: string; basisPoints: number; completedUnits: number | null;
    totalUnits: number | null; message: string | null };
}
export type InputFormat = "pdf" | "doc" | "docx" | "ppt" | "pptx" | "xls" | "xlsx" | "odt" | "ods" | "odp" | "rtf" | "epub" | "text" | "markdown" | "html" | "csv" | "tsv" | "json" | "xml" | "drawio" | "feed" | "ipynb" | "image" | "audio" | "video" | "zip" | "outlook-msg";
const inputFormats = new Set<InputFormat>(["pdf", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "odt", "ods", "odp", "rtf", "epub", "text", "markdown", "html", "csv", "tsv", "json", "xml", "drawio", "feed", "ipynb", "image", "audio", "video", "zip", "outlook-msg"]);
export type OcrPolicy = "off" | "auto" | "always";
export type AiMode = "off" | "fallback" | "prefer" | "only";
export type AssetMode = "extract" | "embed" | "omit";
export type NetworkMode = "restricted" | "unrestricted";
export interface WorkbenchOptions {
  format: InputFormat | null; ocrPolicy: OcrPolicy; ocrConfidence: number; aiMode: AiMode;
  assetMode: AssetMode; includeProvenance: boolean; maxInputMiB: number; maxMemoryMiB: number;
  maxTemporaryMiB: number; maxPages: number; networkMode: NetworkMode; authorizeProvider: boolean;
}
export const defaultWorkbenchOptions: WorkbenchOptions = {
  format: null, ocrPolicy: "auto", ocrConfidence: 0.7, aiMode: "off", assetMode: "extract",
  includeProvenance: true, maxInputMiB: 512, maxMemoryMiB: 1024, maxTemporaryMiB: 256,
  maxPages: 10_000, networkMode: "restricted", authorizeProvider: false,
};
export interface MeetingOptions {
  diarize: boolean;
  expectedSpeakers: number | null;
  transcriptLanguage: "auto" | "zh-Hans" | "zh-Hant" | "en";
  authorizeProvider: boolean;
  maxInputMiB: number;
  maxMemoryMiB: number;
  maxTemporaryMiB: number;
}
export const defaultMeetingOptions: MeetingOptions = {
  diarize: true, expectedSpeakers: null, transcriptLanguage: "auto", authorizeProvider: false, maxInputMiB: 512,
  maxMemoryMiB: 1536, maxTemporaryMiB: 4096,
};
export function meetingOptionsForLocale(locale: string): MeetingOptions {
  return { ...defaultMeetingOptions, transcriptLanguage: locale.toLowerCase().startsWith("zh") ? "zh-Hans" : "auto" };
}

export class ApiError extends Error {
  constructor(readonly code: string) { super("The local API request failed."); this.name = "ApiError"; }
}
function isObject(value: unknown): value is Record<string, unknown> { return typeof value === "object" && value !== null; }
function isComponent(value: unknown): value is ComponentStatus {
  return isObject(value) && typeof value.available === "boolean" && typeof value.code === "string" && typeof value.detail === "string";
}
function parseStatus(value: unknown): StatusResponse {
  if (!isObject(value) || value.schemaVersion !== 1 || !isComponent(value.localApi) || !isComponent(value.documentConsole)
    || !isComponent(value.imageOcr)
    || value.audioTranscription !== undefined && !isComponent(value.audioTranscription)
    || value.speakerDiarization !== undefined && !isComponent(value.speakerDiarization)) throw new ApiError("invalidResponse");
  return value as unknown as StatusResponse;
}
function shortString(value: unknown, limit = 4096): value is string { return typeof value === "string" && value.length <= limit; }
function stringList(value: unknown, limit = 256): value is string[] { return Array.isArray(value) && value.length <= limit && value.every((item) => shortString(item, 512)); }
function safeText(value: unknown, limit: number): value is string {
  return typeof value === "string" && value.length > 0 && value.length <= limit && !/[\u0000-\u001f\u007f]/.test(value);
}
function isFormatAdmin(value: unknown): value is FormatAdmin {
  return isObject(value) && safeText(value.format, 64) && safeText(value.family, 64)
    && ["available", "planned"].includes(String(value.status))
    && ["core", "optional_runtime", "plugin"].includes(String(value.source))
    && Array.isArray(value.extensions) && value.extensions.length <= 32
    && value.extensions.every((item) => typeof item === "string" && /^[a-z0-9][a-z0-9+-]{0,31}$/.test(item))
    && (value.runtimeComponent === undefined || safeText(value.runtimeComponent, 128))
    && (value.installHint === undefined || safeText(value.installHint, 512));
}
function isCapabilityAdmin(value: unknown): value is CapabilityAdmin {
  const sourceRef = (item: unknown) => item === "off" || item === "core:ocr"
    || typeof item === "string" && /^(plugin|provider):[A-Za-z0-9._-]+\/[a-z][a-z0-9-]{0,63}$/.test(item);
  const status = (item: unknown) => ["not-installed", "downloading", "verifying", "ready", "update-available", "corrupt", "incompatible", "blocked", "disabled"].includes(String(item));
  return isObject(value) && ["ocr", "transcription", "diarization"].includes(String(value.id))
    && status(value.status) && status(value.localStatus)
    && sourceRef(value.currentSource) && stringList(value.sources, 64)
    && value.sources.every(sourceRef) && value.sources.includes(value.currentSource as string)
    && (value.version === undefined || safeText(value.version, 128))
    && (value.localVersion === undefined || safeText(value.localVersion, 128));
}
function parseCapabilitySnapshot(value: unknown): CapabilitySnapshot {
  const statuses = new Set(["not-installed", "downloading", "verifying", "ready", "update-available", "corrupt", "incompatible", "blocked", "unknown", "checking", "disabled"]);
  if (!isObject(value) || value.schemaVersion !== 2 || !Number.isSafeInteger(value.generation)
    || typeof value.checking !== "boolean" || value.checkedAtMs !== undefined && !Number.isSafeInteger(value.checkedAtMs)
    || !Array.isArray(value.capabilities) || value.capabilities.length > 16) throw new ApiError("invalidResponse");
  for (const item of value.capabilities) {
    if (!isObject(item) || !["ocr", "transcription", "diarization"].includes(String(item.id))
      || !safeText(item.name, 128) || !statuses.has(String(item.status)) || !statuses.has(String(item.localStatus))
      || !safeText(item.currentSource, 512) || !safeText(item.currentSourceName, 256) || !stringList(item.sources, 64)
      || item.version !== undefined && !safeText(item.version, 128)
      || item.localVersion !== undefined && !safeText(item.localVersion, 128)
      || item.lastVerifiedAtMs !== undefined && !Number.isSafeInteger(item.lastVerifiedAtMs)) throw new ApiError("invalidResponse");
  }
  return value as unknown as CapabilitySnapshot;
}
function parseCapabilityCheck(value: unknown): CapabilityCheck {
  const statuses = new Set(["queued", "running", "cancelling", "cancelled", "completed", "failed"]);
  const stages = new Set(["queued", "package", "runtime", "models", "cancelling", "completed"]);
  if (!isObject(value) || value.schemaVersion !== 1 || !shortString(value.id, 128)
    || !shortString(value.capability, 64) || !safeText(value.capabilityName, 128)
    || !safeText(value.plugin, 128) || !safeText(value.pluginName, 128)
    || !statuses.has(String(value.status)) || !stages.has(String(value.stage))
    || !Number.isSafeInteger(value.progress) || Number(value.progress) < 0 || Number(value.progress) > 100
    || value.code !== undefined && !shortString(value.code, 128)
    || value.detail !== undefined && !shortString(value.detail, 4096)
    || value.elapsedMs !== undefined && (!Number.isSafeInteger(value.elapsedMs) || Number(value.elapsedMs) < 0)) throw new ApiError("invalidResponse");
  return value as unknown as CapabilityCheck;
}
function isProviderAdmin(value: unknown, configurationReadOnly: boolean): value is ProviderAdmin {
  if (!isObject(value) || !safeText(value.name, 128)
    || !["global", "project", "effective"].includes(String(value.scope))
    || value.actionScope !== undefined && !["global", "project"].includes(String(value.actionScope))
    || value.providerType !== undefined && value.providerType !== "openai-compatible"
    || value.baseUrl !== undefined && !safeText(value.baseUrl, 4096)
    || value.model !== undefined && !safeText(value.model, 256)
    || !isObject(value.models) || Object.keys(value.models).length > 64
    || Object.entries(value.models).some(([capability, model]) => !/^[a-z][a-z0-9-]{0,63}$/.test(capability) || !safeText(model, 512))
    || value.apiKeyEnv !== undefined && (typeof value.apiKeyEnv !== "string" || !/^[A-Za-z_][A-Za-z0-9_]{0,127}$/.test(value.apiKeyEnv))
    || value.environmentSet !== undefined && typeof value.environmentSet !== "boolean"
    || !Array.isArray(value.allowedHosts) || value.allowedHosts.length > 64
    || value.allowedHosts.some((host) => !safeText(host, 253) || /\s|\//.test(host))
    || typeof value.allowPrivateNetwork !== "boolean"
    || typeof value.default !== "boolean" || value.timeoutMs !== undefined && (!Number.isSafeInteger(value.timeoutMs) || Number(value.timeoutMs) <= 0 || Number(value.timeoutMs) > 86_400_000)
    || !Array.isArray(value.capabilities) || value.capabilities.length > 64
    || !value.capabilities.every((item) => typeof item === "string" && /^[a-z][a-z0-9-]{0,63}$/.test(item))) return false;
  if (typeof value.effective !== "boolean") return false;
  if (value.effective) return value.scope === "effective" && value.shadowedBy === undefined
    && (configurationReadOnly ? value.actionScope === undefined : value.actionScope !== undefined)
    && value.providerType === "openai-compatible" && safeText(value.baseUrl, 4096) && safeText(value.model, 256)
    && typeof value.apiKeyEnv === "string" && /^[A-Za-z_][A-Za-z0-9_]{0,127}$/.test(value.apiKeyEnv)
    && typeof value.environmentSet === "boolean";
  return value.scope !== "effective" && value.shadowedBy === "effective" && value.actionScope === value.scope;
}
function isDetectionResult(value: unknown): boolean {
  return isObject(value) && value.kind === "detection" && (value.sourceName === null || value.sourceName === undefined || safeText(value.sourceName, 512))
    && Number.isSafeInteger(value.sourceSize) && Number(value.sourceSize) >= 0
    && Array.isArray(value.candidates) && value.candidates.length <= 64
    && value.candidates.every((item) => isObject(item) && safeText(item.format, 64)
      && typeof item.confidence === "number" && Number.isFinite(item.confidence) && item.confidence >= 0 && item.confidence <= 1
      && typeof item.explicit === "boolean" && safeText(item.detectorId, 128) && safeText(item.reason, 512)
      && stringList(item.diagnostics, 64));
}
function boundedJson(value: unknown, depth = 0): boolean {
  if (depth > 16) return false;
  if (value === null || typeof value === "boolean" || typeof value === "number" && Number.isFinite(value)) return true;
  if (typeof value === "string") return value.length <= 4096 && !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/.test(value);
  if (Array.isArray(value)) return value.length <= 256 && value.every((item) => boundedJson(item, depth + 1));
  if (!isObject(value) || Object.keys(value).length > 256) return false;
  return Object.entries(value).every(([key, item]) => safeText(key, 128) && boundedJson(item, depth + 1));
}
function isOperationResult(value: unknown): value is AdminOperationResult {
  return isDetectionResult(value) || isObject(value) && value.kind === "profile"
    && safeText(value.name, 128) && isObject(value.value) && boundedJson(value.value)
    || isObject(value) && value.kind === "config" && ["paths", "get", "showMerged", "showResolved"].includes(String(value.operation)) && boundedJson(value.value)
    || isObject(value) && value.kind === "doctor" && Array.isArray(value.checks) && value.checks.length <= 512
      && value.checks.every((item) => isObject(item) && shortString(item.id, 256) && shortString(item.status, 64) && shortString(item.detail))
    || isObject(value) && value.kind === "providerTest" && typeof value.configuredModelAvailable === "boolean"
      && Number.isSafeInteger(value.modelCount) && Number(value.modelCount) >= 0 && Number(value.modelCount) <= 10_000
      && Array.isArray(value.capabilities) && value.capabilities.length <= 64
      && value.capabilities.every((item) => typeof item === "string" && /^[a-z][a-z0-9-]{0,63}$/.test(item));
}
function isPluginAdmin(value: unknown, configurationReadOnly: boolean): value is PluginAdmin {
  if (!isObject(value) || !shortString(value.id, 128) || !/^[A-Za-z0-9._-]+$/.test(value.id)
    || !["global", "project", "effective"].includes(String(value.scope))
    || value.actionScope !== undefined && !["global", "project"].includes(String(value.actionScope))
    || value.packageScope !== undefined && !["global", "project"].includes(String(value.packageScope))
    || value.source !== undefined && (!shortString(value.source) || /[\u0000-\u001f\u007f]/.test(value.source))
    || value.sha256 !== undefined && (typeof value.sha256 !== "string" || !/^[0-9a-f]{64}$/.test(value.sha256))
    || value.protocol !== undefined && (!shortString(value.protocol, 64) || !/^[A-Za-z0-9._-]+$/.test(value.protocol))
    || value.enabled !== undefined && typeof value.enabled !== "boolean" || typeof value.effective !== "boolean"
    || value.verification !== undefined && (!shortString(value.verification, 64) || !/^[A-Za-z][A-Za-z0-9]*$/.test(value.verification))
    || value.version !== undefined && (typeof value.version !== "string" || value.version.length > 128 || /[^\x20-\x7e]/.test(value.version))
    || value.signingKeyId !== undefined && (!shortString(value.signingKeyId, 128) || !/^[A-Za-z0-9._-]+$/.test(value.signingKeyId))
    || value.signingKeySha256 !== undefined && (typeof value.signingKeySha256 !== "string" || !/^[0-9a-f]{64}$/.test(value.signingKeySha256))
    || value.target !== undefined && (!shortString(value.target, 128) || !/^[A-Za-z0-9._-]+$/.test(value.target))) return false;
  const shadow = value.shadowedBy;
  return value.effective ? value.scope === "effective" && shadow === undefined
    && (configurationReadOnly ? value.actionScope === undefined && value.packageScope === undefined : value.actionScope !== undefined && value.packageScope !== undefined)
    && value.source !== undefined
    && value.protocol !== undefined && value.enabled !== undefined && value.verification !== undefined
    && value.signingKeyId !== undefined && value.signingKeySha256 !== undefined && value.target !== undefined
    : value.scope !== "effective" && shadow === "effective" && value.actionScope === value.scope && value.packageScope !== undefined;
}
export function parseAdminSnapshot(value: unknown): AdminSnapshot {
  if (!isObject(value) || value.schemaVersion !== 1 || !Array.isArray(value.formats) || value.formats.length > 128
    || value.formats.some((item) => !isFormatAdmin(item))
    || !Array.isArray(value.capabilities) || value.capabilities.length !== 3 || value.capabilities.some((item) => !isCapabilityAdmin(item))
    || !Array.isArray(value.providers) || value.providers.length > 128 || value.providers.some((item) => !isProviderAdmin(item, value.configurationReadOnly === true))
    || !Array.isArray(value.plugins) || value.plugins.length > 256 || value.plugins.some((item) => !isPluginAdmin(item, value.configurationReadOnly === true))
    || !isObject(value.configuration) || typeof value.configurationReadOnly !== "boolean" || !Array.isArray(value.profiles) || value.profiles.length > 128
    || value.profiles.some((item) => !isObject(item) || !safeText(item.name, 128)
      || !["global", "project", "effective"].includes(String(item.scope)) || typeof item.effective !== "boolean" || typeof item.active !== "boolean"
      || (item.effective ? item.shadowedBy !== undefined : item.shadowedBy !== "project"))
    || !Array.isArray(value.doctor) || value.doctor.length > 512 || value.doctor.some((item) => !isObject(item) || !shortString(item.id, 256) || !shortString(item.status, 64) || !shortString(item.detail))
    || value.operationResult !== undefined && !isOperationResult(value.operationResult)) throw new ApiError("invalidResponse");
  if (value.configurationReadOnly === true && (
    value.providers.some((item) => !isObject(item) || item.scope !== "effective" || item.effective !== true || item.actionScope !== undefined)
    || value.plugins.some((item) => !isObject(item) || item.scope !== "effective" || item.effective !== true || item.actionScope !== undefined || item.packageScope !== undefined)
    || value.profiles.some((item) => !isObject(item) || item.scope !== "effective" || item.effective !== true)
  )) throw new ApiError("invalidResponse");
  return value as unknown as AdminSnapshot;
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
    || !Number.isSafeInteger(value.artifactGeneration) || Number(value.artifactGeneration) < 0
    || Number(value.progressMillionths) < 0 || Number(value.progressMillionths) > 1_000_000
    || !Array.isArray(value.diagnostics) || value.diagnostics.length > 1024 || value.diagnostics.some((item) => !isObject(item) || typeof item.code !== "string" || item.code.length > 128)
    || !Array.isArray(value.artifacts) || value.artifacts.length > 128 || value.artifacts.some((artifact) => !isArtifact(artifact))
    || value.displayName !== undefined && value.displayName !== null
      && (typeof value.displayName !== "string" || value.displayName.length === 0 || value.displayName.length > 255 || /[\u0000-\u001f\u007f/\\]/.test(value.displayName))
    || value.format !== undefined && value.format !== null && !inputFormats.has(value.format as InputFormat)
    || value.batchId !== undefined && value.batchId !== null
      && (typeof value.batchId !== "string" || !/^[0-9a-f]{32}$/.test(value.batchId))
    || value.workflow !== "conversion" && value.workflow !== "meetingTranscript"
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
    || Number(value.sequence) < 0 || !/^[0-9a-f]{32}$/.test(value.taskId)
    || !Number.isSafeInteger(value.progressMillionths) || Number(value.progressMillionths) < 0
    || Number(value.progressMillionths) > 1_000_000 || typeof value.terminal !== "boolean"
    || value.execution !== undefined && (!isObject(value.execution)
      || !["resolving", "detecting", "probing", "converting", "ocr", "ai", "rendering", "completed"].includes(String(value.execution.stage))
      || !Number.isSafeInteger(value.execution.basisPoints) || Number(value.execution.basisPoints) < 0
      || Number(value.execution.basisPoints) > 10_000
      || value.execution.completedUnits !== null && !Number.isSafeInteger(value.execution.completedUnits)
      || value.execution.totalUnits !== null && !Number.isSafeInteger(value.execution.totalUnits)
      || value.execution.message !== null && (typeof value.execution.message !== "string"
        || value.execution.message.length > 256 || /[\u0000-\u001f\u007f]/.test(value.execution.message)))) throw new ApiError("invalidEvent");
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
export function taskRequest(options: WorkbenchOptions, batchId?: string): unknown {
  const ai = options.aiMode;
  const unrestrictedNetwork = options.networkMode === "unrestricted";
  return { schemaVersion: 1, workflow: "conversion", format: options.format, ...(batchId ? { batchId } : {}), options: {
    text: { charset: null, decoding_mode: "strict" }, delimited_text: { header: "auto", ragged_rows: "strict" },
    ocr: { policy: options.ocrPolicy, minimum_confidence: options.ocrConfidence },
    asr: { language: null, chinese_script: "preserve", max_threads: 4, max_duration_ms: null,
      max_segments: 100_000, max_native_memory_bytes: 900 * 1024 * 1024 },
    diarization: { enabled: false, expected_speakers: null, max_speakers: 16 },
    ai: { vision_ocr: ai, image_description: ai, layout_repair: ai, table_repair: ai, formula_repair: ai, audio_transcription: "off", markdown_postprocess: ai },
    network: { enabled: unrestrictedNetwork, max_redirects: 3, deny_private_networks: !unrestrictedNetwork, allowed_hosts: [] },
    limits: { max_input_bytes: mib(options.maxInputMiB), max_decompressed_bytes: 1073741824, max_archive_entries: 100000,
      max_archive_depth: 16, max_archive_entry_bytes: 268435456, max_archive_compression_ratio: 100, max_nesting_depth: 256,
      max_pages: options.maxPages, max_asset_bytes: 67108864, max_total_asset_bytes: 134217728,
      max_memory_bytes: mib(options.maxMemoryMiB), max_temporary_bytes: mib(options.maxTemporaryMiB), max_table_rows: 100000,
      max_table_columns: 16384, max_table_cells: 1000000, max_field_bytes: 16777216, max_feed_entries: 10000,
      max_feed_text_bytes: 67108864, max_feed_html_bytes: 67108864 },
    output: { flavor: "gfm", asset_directory_suffix: "_assets", include_provenance: options.includeProvenance,
      asset_mode: options.assetMode, asset_uri_prefix: null },
  }, authorization: { network: unrestrictedNetwork, privateNetwork: unrestrictedNetwork, provider: options.authorizeProvider } };
}

export function meetingTaskRequest(file: File, options: MeetingOptions): unknown {
  const inferred = file.type.startsWith("video/") ? "video"
    : file.type.startsWith("audio/") ? "audio"
      : /\.(mp4|mkv|webm|avi|mov)$/i.test(file.name) ? "video" : "audio";
  const chinese = options.transcriptLanguage === "zh-Hans" || options.transcriptLanguage === "zh-Hant";
  const language = chinese ? "zh" : options.transcriptLanguage === "en" ? "en" : null;
  const chineseScript = options.transcriptLanguage === "zh-Hans" ? "simplified"
    : options.transcriptLanguage === "zh-Hant" ? "traditional" : "preserve";
  return { schemaVersion: 1, workflow: "meetingTranscript", format: inferred, options: {
    text: { charset: null, decoding_mode: "strict" }, delimited_text: { header: "auto", ragged_rows: "strict" },
    ocr: { policy: "off", minimum_confidence: 0.7 },
    asr: { language, chinese_script: chineseScript, max_threads: 4, max_duration_ms: null,
      max_segments: 100_000, max_native_memory_bytes: 900 * 1024 * 1024 },
    diarization: { enabled: options.diarize, expected_speakers: options.expectedSpeakers,
      max_speakers: 16 },
    ai: { vision_ocr: "off", image_description: "off", layout_repair: "off", table_repair: "off",
      formula_repair: "off", audio_transcription: "only", markdown_postprocess: "off" },
    network: { enabled: options.authorizeProvider, max_redirects: 3, deny_private_networks: !options.authorizeProvider, allowed_hosts: [] },
    limits: { max_input_bytes: mib(options.maxInputMiB), max_decompressed_bytes: 1073741824, max_archive_entries: 100000,
      max_archive_depth: 16, max_archive_entry_bytes: 268435456, max_archive_compression_ratio: 100, max_nesting_depth: 256,
      max_pages: 10000, max_asset_bytes: 67108864, max_total_asset_bytes: 134217728,
      max_memory_bytes: mib(options.maxMemoryMiB), max_temporary_bytes: mib(options.maxTemporaryMiB), max_table_rows: 100000,
      max_table_columns: 16384, max_table_cells: 1000000, max_field_bytes: 16777216, max_feed_entries: 10000,
      max_feed_text_bytes: 67108864, max_feed_html_bytes: 67108864 },
    output: { flavor: "gfm", asset_directory_suffix: "_assets", include_provenance: true,
      asset_mode: "extract", asset_uri_prefix: null },
  }, authorization: { network: options.authorizeProvider, privateNetwork: options.authorizeProvider, provider: options.authorizeProvider } };
}

export interface ApiClient {
  status(signal?: AbortSignal): Promise<StatusResponse>; listTasks(filters?: TaskFilters, signal?: AbortSignal): Promise<TaskPage>;
  capabilitySnapshot(signal?: AbortSignal): Promise<CapabilitySnapshot>;
  startCapabilityCheck?(id: CapabilityAdmin["id"], signal?: AbortSignal): Promise<CapabilityCheck>;
  capabilityCheck?(id: string, signal?: AbortSignal): Promise<CapabilityCheck>;
  cancelCapabilityCheck?(id: string, signal?: AbortSignal): Promise<CapabilityCheck>;
  installCapability(id: "ocr" | "media", signal?: AbortSignal): Promise<void>;
  stagePluginPackage?(file: File, signal?: AbortSignal): Promise<StagedPluginPackage>;
  getTask(id: string, signal?: AbortSignal): Promise<TaskRecord>;
  upload(file: File, options: WorkbenchOptions, batchId: string, signal?: AbortSignal): Promise<TaskRecord>;
  uploadMeeting(file: File, options: MeetingOptions, signal?: AbortSignal): Promise<TaskRecord>;
  cancel(id: string, signal?: AbortSignal): Promise<TaskRecord>;
  retry(id: string, signal?: AbortSignal): Promise<TaskRecord>;
  setPinned(id: string, pinned: boolean, signal?: AbortSignal): Promise<TaskRecord>;
  speakerLabels(id: string, signal?: AbortSignal): Promise<SpeakerLabels>;
  relabelSpeakers(id: string, expectedGeneration: number, speakers: Record<string, string>, signal?: AbortSignal): Promise<TaskRecord>;
  deleteTask(id: string, signal?: AbortSignal): Promise<void>;
  cleanup(signal?: AbortSignal): Promise<CleanupSummary>;
  watchTask(id: string, onEvent: (event: TaskEvent) => void, signal: AbortSignal): Promise<void>;
  preview(id: string, key: string, signal?: AbortSignal): Promise<ArtifactPreview>;
  download(id: string, key: string, signal?: AbortSignal): Promise<ArtifactDownload>;
  admin(signal?: AbortSignal, section?: string): Promise<AdminSnapshot>;
  adminGrant(action: AdminAction, signal?: AbortSignal): Promise<string>;
  adminAction(action: AdminAction, signal?: AbortSignal): Promise<AdminActionOutcome>;
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
    async capabilitySnapshot(signal) { return parseCapabilitySnapshot(await jsonRequest("/api/capabilities/status", { method: "GET", headers: auth(), ...(signal ? { signal } : {}) }, 65536)); },
    async startCapabilityCheck(id, signal) { return parseCapabilityCheck(await jsonRequest(`/api/capabilities/${id}/verify`, { method: "POST", headers: auth(), body: null, ...(signal ? { signal } : {}) }, 65536)); },
    async capabilityCheck(id, signal) { return parseCapabilityCheck(await jsonRequest(`/api/capability-checks/${encodeURIComponent(id)}`, { method: "GET", headers: auth(), ...(signal ? { signal } : {}) }, 65536)); },
    async cancelCapabilityCheck(id, signal) { return parseCapabilityCheck(await jsonRequest(`/api/capability-checks/${encodeURIComponent(id)}`, { method: "DELETE", headers: auth(), body: null, ...(signal ? { signal } : {}) }, 65536)); },
    async installCapability(id, signal) {
      const value = await jsonRequest(`/api/capabilities/${id}/install`, { method: "POST", headers: auth(), body: null, ...(signal ? { signal } : {}) }, 4096);
      if (!isObject(value) || value.schemaVersion !== 1 || value.capability !== id || value.status !== "installed") throw new ApiError("invalidResponse");
    },
    async stagePluginPackage(file, signal) {
      const headers = auth(); headers["Content-Type"] = "application/octet-stream"; headers["X-Into-Md-Plugin-Filename-B64"] = base64UrlUtf8(file.name);
      const value = await jsonRequest("/api/admin/plugin-package", { method: "POST", headers, body: file, ...(signal ? { signal } : {}) }, 4096);
      if (!isObject(value) || value.schemaVersion !== 1 || !safeText(value.source, 4096) || !safeText(value.filename, 255)
        || !Number.isSafeInteger(value.byteLen) || Number(value.byteLen) <= 0 || typeof value.sha256 !== "string" || !/^[0-9a-f]{64}$/.test(value.sha256)
        || value.officialPluginId !== undefined && !safeText(value.officialPluginId, 128)
        || value.signingKeyId !== undefined && !safeText(value.signingKeyId, 256)
        || value.signingKeySha256 !== undefined && (typeof value.signingKeySha256 !== "string" || !/^[0-9a-f]{64}$/.test(value.signingKeySha256))
        || [value.officialPluginId, value.signingKeyId, value.signingKeySha256].filter((item) => item !== undefined).length % 3 !== 0) throw new ApiError("invalidResponse");
      return value as unknown as StagedPluginPackage;
    },
    async admin(signal, section) { const query = section ? `?section=${encodeURIComponent(section)}` : ""; return parseAdminSnapshot(await jsonRequest(`/api/admin${query}`, { method: "GET", headers: auth(), ...(signal ? { signal } : {}) }, MAX_RESPONSE_BYTES)); },
    async adminGrant(action, signal) {
      const headers = auth(); headers["Content-Type"] = "application/json";
      const value = await jsonRequest("/api/admin/grant", { method: "POST", headers, body: JSON.stringify(action), ...(signal ? { signal } : {}) }, 4096);
      if (!isObject(value) || value.schemaVersion !== 1 || !shortString(value.grant, 128) || !/^[A-Za-z0-9_-]{43}$/.test(value.grant)) throw new ApiError("invalidResponse");
      return value.grant;
    },
    async adminAction(action, signal) {
      const headers = auth(); headers["Content-Type"] = "application/json";
      const value = await jsonRequest("/api/admin", { method: "POST", headers, body: JSON.stringify(action), ...(signal ? { signal } : {}) }, MAX_RESPONSE_BYTES);
      if (!isObject(value) || value.schemaVersion !== 1 || value.code !== "ok"
        || value.operationResult !== undefined && !isOperationResult(value.operationResult)) throw new ApiError("invalidResponse");
      return value.operationResult === undefined ? {} : { operationResult: value.operationResult };
    },
    async listTasks(filters = {}, signal) {
      const query = new URLSearchParams(); query.set("limit", String(filters.limit ?? 25));
      if (filters.after) { query.set("afterUpdatedAtMs", String(filters.after.updatedAtMs)); query.set("afterId", filters.after.id); }
      if (filters.status) query.set("status", filters.status); if (filters.pinned !== undefined) query.set("pinned", String(filters.pinned));
      if (filters.batchId) query.set("batchId", filters.batchId);
      return parseTaskList(await jsonRequest(`/api/tasks?${query}`, { method: "GET", headers: auth(), ...(signal ? { signal } : {}) }));
    },
    async getTask(id, signal) { return parseTask(await jsonRequest(`/api/tasks/${id}`, { method: "GET", headers: auth(), ...(signal ? { signal } : {}) })); },
    async upload(file, options, batchId, signal) {
      const headers = auth(); headers["X-Into-Md-Filename-B64"] = base64UrlUtf8(file.name); headers["X-Into-Md-Request"] = base64UrlJson(taskRequest(options, batchId));
      return parseTask(await jsonRequest("/api/tasks", { method: "POST", headers, body: file, ...(signal ? { signal } : {}) }));
    },
    async uploadMeeting(file, options, signal) {
      const headers = auth(); headers["X-Into-Md-Filename-B64"] = base64UrlUtf8(file.name); headers["X-Into-Md-Request"] = base64UrlJson(meetingTaskRequest(file, options));
      return parseTask(await jsonRequest("/api/tasks", { method: "POST", headers, body: file, ...(signal ? { signal } : {}) }));
    },
    async cancel(id, signal) { return parseTask(await jsonRequest(`/api/tasks/${id}`, { method: "DELETE", headers: auth(), body: null, ...(signal ? { signal } : {}) })); },
    async retry(id, signal) { return parseTask(await jsonRequest(`/api/tasks/${id}/retry`, { method: "POST", headers: auth(), body: null, ...(signal ? { signal } : {}) })); },
    async setPinned(id, pinned, signal) { const headers = auth(); headers["Content-Type"] = "application/json"; return parseTask(await jsonRequest(`/api/tasks/${id}/pin`, { method: "POST", headers, body: JSON.stringify({ pinned }), ...(signal ? { signal } : {}) })); },
    async speakerLabels(id, signal) {
      const value = await jsonRequest(`/api/tasks/${id}/speakers`, { method: "GET", headers: auth(), ...(signal ? { signal } : {}) }, 65536);
      if (!isObject(value) || value.schemaVersion !== 1 || !Number.isSafeInteger(value.artifactGeneration)
        || Number(value.artifactGeneration) < 0 || !Array.isArray(value.speakers) || value.speakers.length > 64
        || value.speakers.some((speaker) => !isObject(speaker) || typeof speaker.id !== "string"
          || !/^speaker-(?:[1-9]|[1-5][0-9]|6[0-4])$/.test(speaker.id) || typeof speaker.name !== "string"
          || speaker.name.length === 0 || speaker.name.length > 80 || speaker.name.trim() !== speaker.name
          || /[\u0000-\u001f\u007f]/.test(speaker.name))) throw new ApiError("invalidResponse");
      return value as unknown as SpeakerLabels;
    },
    async relabelSpeakers(id, expectedGeneration, speakers, signal) {
      const headers = auth(); headers["Content-Type"] = "application/json";
      return parseTask(await jsonRequest(`/api/tasks/${id}/speakers`, { method: "POST", headers,
        body: JSON.stringify({ expectedGeneration, speakers }), ...(signal ? { signal } : {}) }, 65536));
    },
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

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Archive, AudioLines, Braces, CheckCircle2, ChevronDown, CircleAlert, Download, Eye, File,
  FileAudio, FileImage, FileJson, FileSpreadsheet, FileText, FolderOpen, History, Languages,
  LoaderCircle, MoreHorizontal, Package, Paperclip, Pin, PinOff, Plus, Presentation, RefreshCw,
  RotateCcw, ScanText, Settings2, ShieldCheck, SlidersHorizontal, Sparkles, Square, Trash2,
  UploadCloud, Wifi, WifiOff, X, type LucideIcon,
} from "lucide-react";
import type {
  AiMode, ApiClient, ArtifactPreview, ArtifactReference, InputFormat, NetworkMode, TaskCursor,
  TaskRecord, TaskStatus, WorkbenchOptions,
} from "./api";
import { ApiError, defaultWorkbenchOptions } from "./api";
import { I18nProvider, useI18n } from "./i18n";
import { JsonPreview, SafeMarkdownPreview } from "./preview";
import { RouteLink, Router, useRouter } from "./router";
import { ThemeProvider, useTheme } from "./theme";

const MAX_BATCH_FILES = 100;
const MAX_BATCH_BYTES = 1024 * 1024 * 1024;
const FORMATS: InputFormat[] = ["pdf", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "odt", "ods", "odp", "rtf", "epub", "text", "markdown", "html", "csv", "tsv", "json", "xml", "feed", "ipynb", "image", "audio", "video", "zip", "outlook-msg"];
const TERMINAL = new Set(["succeeded", "failed", "interrupted", "cancelled"]);

function formatForFile(file: File, hint: InputFormat | null): string {
  if (hint) return hint;
  const extension = file.name.toLocaleLowerCase("en-US").split(".").pop() ?? "";
  if (FORMATS.includes(extension as InputFormat)) return extension;
  if (["txt", "log"].includes(extension)) return "text";
  if (["md", "mdown"].includes(extension)) return "markdown";
  if (["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff"].includes(extension)) return "image";
  return "auto";
}

function iconForFormat(format: string): LucideIcon {
  if (["pdf", "doc", "docx", "rtf", "odt"].includes(format)) return FileText;
  if (["ppt", "pptx", "odp"].includes(format)) return Presentation;
  if (["xls", "xlsx", "ods", "csv", "tsv"].includes(format)) return FileSpreadsheet;
  if (format === "image") return FileImage;
  if (format === "audio" || format === "video") return FileAudio;
  if (["zip", "epub", "outlook-msg"].includes(format)) return Archive;
  if (format === "json" || format === "ipynb") return FileJson;
  return File;
}

function bytesLabel(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1048576).toFixed(1)} MiB`;
}

function Preferences() {
  const { locale, setLocale, t } = useI18n(); const { theme, setTheme } = useTheme();
  return <div className="preferences">
    <label className="compact-select"><Languages size={16} aria-hidden="true" /><span className="visually-hidden">{t("language")}</span><select aria-label={t("language")} value={locale} onChange={(event) => setLocale(event.target.value === "zh-CN" ? "zh-CN" : "en")}><option value="zh-CN">简体中文</option><option value="en">English</option></select></label>
    <label className="compact-select"><Settings2 size={16} aria-hidden="true" /><span className="visually-hidden">{t("theme")}</span><select aria-label={t("theme")} value={theme} onChange={(event) => { const value = event.target.value; setTheme(value === "light" || value === "dark" ? value : "system"); }}><option value="system">{t("system")}</option><option value="light">{t("light")}</option><option value="dark">{t("dark")}</option></select></label>
  </div>;
}

function ServiceBadge({ api }: { api: ApiClient }) {
  const { t } = useI18n(); const [state, setState] = useState<"checking" | "ready" | "error">("checking");
  useEffect(() => { const controller = new AbortController(); void api.status(controller.signal).then((value) => setState(value.localApi.available && value.documentConsole.available ? "ready" : "error"), () => { if (!controller.signal.aborted) setState("error"); }); return () => controller.abort(); }, [api]);
  const Icon = state === "checking" ? LoaderCircle : state === "ready" ? ShieldCheck : CircleAlert;
  return <RouteLink href="/status" className={`service-badge ${state}`}><Icon size={17} aria-hidden="true" className={state === "checking" ? "spin" : ""} /><span>{t(state === "ready" ? "systemReady" : state === "error" ? "systemNeedsAttention" : "checkingSystem")}</span></RouteLink>;
}

function SegmentedControl<T extends string>({ label, value, items, onChange }: { label: string; value: T; items: Array<{ value: T; label: string }>; onChange(value: T): void }) {
  return <div className="segmented-field"><span>{label}</span><div className="segmented-control" role="group" aria-label={label}>{items.map((item) => <button key={item.value} type="button" aria-pressed={value === item.value} onClick={() => onChange(item.value)}>{item.label}</button>)}</div></div>;
}

function OptionPanel({ value, onChange, disabled }: { value: WorkbenchOptions; onChange(value: WorkbenchOptions): void; disabled: boolean }) {
  const { t } = useI18n(); const patch = <K extends keyof WorkbenchOptions>(key: K, next: WorkbenchOptions[K]) => onChange({ ...value, [key]: next });
  return <section className="control-card" aria-labelledby="conversion-settings-heading">
    <div className="control-card-heading"><div><p className="section-kicker">{t("conversionSettings")}</p><h2 id="conversion-settings-heading">{t("smartDefaults")}</h2></div><Sparkles size={20} aria-hidden="true" /></div>
    <div className="quick-option-grid">
      <div className="segmented-field"><span>{t("outputFormat")}</span><div className="segmented-control single" aria-label={t("outputFormat")}><button type="button" aria-pressed="true"><FileText size={16} aria-hidden="true" /> Markdown</button></div></div>
      <SegmentedControl label={t("recognitionMode")} value={value.ocrPolicy} onChange={(next) => patch("ocrPolicy", next)} items={[{ value: "auto", label: t("smart") }, { value: "always", label: t("precise") }, { value: "off", label: t("off") }]} />
      <SegmentedControl label={t("assetMode")} value={value.assetMode} onChange={(next) => patch("assetMode", next)} items={[{ value: "extract", label: t("separateAssets") }, { value: "embed", label: t("embedAssets") }, { value: "omit", label: t("omitAssets") }]} />
    </div>
    <details className="advanced-settings"><summary><SlidersHorizontal size={17} aria-hidden="true" /><span>{t("advancedSettings")}</span><ChevronDown size={17} aria-hidden="true" /></summary><fieldset disabled={disabled}>
      <div className="option-grid">
        <label><span>{t("formatHint")}</span><select value={value.format ?? ""} onChange={(event) => patch("format", (event.target.value || null) as InputFormat | null)}><option value="">{t("automatic")}</option>{FORMATS.map((format) => <option key={format} value={format}>{format}</option>)}</select></label>
        <label><span>{t("ocrConfidence")}</span><input type="number" min="0" max="1" step="0.05" value={value.ocrConfidence} onChange={(event) => patch("ocrConfidence", Number(event.target.value))} /></label>
        <label><span>{t("aiMode")}</span><select value={value.aiMode} onChange={(event) => patch("aiMode", event.target.value as AiMode)}><option value="off">{t("off")}</option><option value="fallback">Fallback</option><option value="prefer">Prefer</option><option value="only">Only</option></select></label>
        <label><span>{t("maxInput")}</span><input type="number" min="1" max="512" value={value.maxInputMiB} onChange={(event) => patch("maxInputMiB", Number(event.target.value))} /></label>
        <label><span>{t("maxMemory")}</span><input type="number" min="1" max="256" value={value.maxMemoryMiB} onChange={(event) => patch("maxMemoryMiB", Number(event.target.value))} /></label>
        <label><span>{t("maxPages")}</span><input type="number" min="1" max="10000" value={value.maxPages} onChange={(event) => patch("maxPages", Number(event.target.value))} /></label>
      </div>
      <div className="authorization-box"><div className="network-choice"><div className="network-icon">{value.networkMode === "unrestricted" ? <Wifi size={18} aria-hidden="true" /> : <WifiOff size={18} aria-hidden="true" />}</div><div><strong>{t("networkAccess")}</strong><p>{t(value.networkMode === "unrestricted" ? "networkEnabledNote" : "networkDisabledNote")}</p></div><label className="switch"><span className="visually-hidden">{t("networkAccess")}</span><input type="checkbox" checked={value.networkMode === "unrestricted"} onChange={(event) => patch("networkMode", (event.target.checked ? "unrestricted" : "restricted") as NetworkMode)} /><span aria-hidden="true" /></label></div>
        {value.aiMode !== "off" && <label className="check grant"><input type="checkbox" checked={value.authorizeProvider} onChange={(event) => patch("authorizeProvider", event.target.checked)} />{t("authorizeProvider")}</label>}{value.aiMode !== "off" && <p>{t("authorizationNote")}</p>}
      </div>
    </fieldset></details>
  </section>;
}

function CapabilityStrip({ audioEnabled, onAudioChange }: { audioEnabled?: boolean; onAudioChange?: (enabled: boolean) => void }) {
  const { t } = useI18n(); const items = [
    { icon: FileText, label: t("documentParsing"), status: t("localReady"), tone: "ready" },
    { icon: ScanText, label: t("imageOcr"), status: t("automaticDetection"), tone: "ready" },
  ];
  return <section className="capability-strip" aria-label={t("capabilities")}>{items.map(({ icon: Icon, label, status, tone }) => <div className="capability-item" key={label}><span className="capability-icon"><Icon size={20} aria-hidden="true" /></span><div><strong>{label}</strong><span className={tone}><CheckCircle2 size={14} aria-hidden="true" />{status}</span></div></div>)}<div className="capability-item audio-capability"><span className="capability-icon"><AudioLines size={20} aria-hidden="true" /></span><div><strong>{t("audioTranscription")}</strong><span className={audioEnabled ? "ready" : "neutral"}>{audioEnabled ? <CheckCircle2 size={14} aria-hidden="true" /> : <CircleAlert size={14} aria-hidden="true" />}{t(audioEnabled ? "enabled" : onAudioChange ? "disabled" : "enableInWorkbench")}</span></div>{onAudioChange && <label className="switch compact-switch"><span className="visually-hidden">{t("audioTranscription")}</span><input type="checkbox" checked={audioEnabled} onChange={(event) => onAudioChange(event.target.checked)} /><span aria-hidden="true" /></label>}</div></section>;
}

function artifactLabel(artifact: ArtifactReference): string { return artifact.filename ?? ({ markdown: "result.md", documentIr: "document-ir.json", diagnostics: "diagnostics.json", bundle: "result.zip", asset: artifact.assetId ?? "asset" } as const)[artifact.kind]; }

function TaskCard({ task, name, format, stage, featured, api, onUpdate, onRetry, onDelete }: { task: TaskRecord; name?: string | undefined; format?: string | undefined; stage?: string | undefined; featured: boolean; api: ApiClient; onUpdate(task: TaskRecord): void; onRetry(): void; onDelete(): void }) {
  const { t } = useI18n(); const busy = !TERMINAL.has(task.status); const percent = Math.round(task.progressMillionths / 10_000);
  const [preview, setPreview] = useState<{ artifact: ArtifactReference; value?: ArtifactPreview; error?: boolean } | null>(null);
  const automaticPreviewKey = useRef<string | null>(null);
  const markdown = task.artifacts.find((artifact) => artifact.kind === "markdown");
  const cancel = async () => { try { onUpdate(await api.cancel(task.id)); } catch { /* SSE remains authoritative */ } };
  const download = async (key: string) => { const result = await api.download(task.id, key); const url = URL.createObjectURL(result.blob); const anchor = document.createElement("a"); anchor.href = url; anchor.download = result.filename; anchor.click(); URL.revokeObjectURL(url); };
  const openPreview = useCallback(async (artifact: ArtifactReference) => { setPreview({ artifact }); try { setPreview({ artifact, value: await api.preview(task.id, artifact.storageKey) }); } catch { setPreview({ artifact, error: true }); } }, [api, task.id]);
  useEffect(() => {
    if (!featured || !markdown) return;
    const key = `${task.id}:${markdown.storageKey}`;
    if (automaticPreviewKey.current === key) return;
    automaticPreviewKey.current = key;
    void openPreview(markdown);
  }, [featured, markdown, openPreview, task.id]);
  const pin = async () => { try { onUpdate(await api.setPinned(task.id, !task.pinned)); } catch { /* refresh remains available */ } };
  const removeHistory = async () => { if (!window.confirm(t("deleteWarning"))) return; try { await api.deleteTask(task.id); onDelete(); } catch { /* keep the durable card visible */ } };
  const displayName = name ?? `${t("restoredTask")} ${task.id.slice(0, 8)}`; const FormatIcon = iconForFormat(format ?? "auto");
  return <article className={`task-card ${task.status} ${featured ? "featured" : "compact"}`} aria-labelledby={`task-${task.id}`}>
    <div className="task-top"><div className="task-identity"><span className="file-type-icon"><FormatIcon size={20} aria-hidden="true" /></span><div><h3 id={`task-${task.id}`}>{displayName}</h3><p><span className="status-pill">{t(task.status)}</span>{task.pinned ? ` · ${t("pinned")}` : ""}{format ? ` · ${format.toUpperCase()}` : ""}{stage ? ` · ${stage}` : ""}</p></div></div><div className="task-meta"><small>{new Date(task.updatedAtMs).toLocaleString()}</small>{busy && <strong>{percent}%</strong>}</div></div>
    {busy && <progress max="100" value={percent} aria-label={`${displayName}: ${percent}%`} />}
    {task.diagnostics.length > 0 && <div className="diagnostic-summary" role="status"><CircleAlert size={16} aria-hidden="true" /><span>{task.diagnostics.map((item) => item.code).join(", ")}</span></div>}
    <div className="task-action-row">{busy && <button className="secondary danger" type="button" onClick={() => void cancel()}><Square size={15} aria-hidden="true" />{t("cancel")}</button>}{!busy && task.status !== "succeeded" && <button type="button" onClick={onRetry}><RotateCcw size={16} aria-hidden="true" />{t("retry")}</button>}{task.status === "succeeded" && markdown && <><button type="button" onClick={() => void openPreview(markdown)}><Eye size={16} aria-hidden="true" />{t("preview")} {artifactLabel(markdown)}</button><button className="secondary" type="button" onClick={() => void download(markdown.storageKey)}><Download size={16} aria-hidden="true" />{t("download")} {artifactLabel(markdown)}</button></>}
      <details className="task-menu"><summary aria-label={t("moreActions")}><MoreHorizontal size={19} aria-hidden="true" /></summary><div className="task-menu-popover">{!busy && <button className="menu-action" type="button" aria-pressed={task.pinned} onClick={() => void pin()}>{task.pinned ? <PinOff size={16} aria-hidden="true" /> : <Pin size={16} aria-hidden="true" />}{task.pinned ? t("unpin") : t("pin")}</button>}{!busy && task.status === "succeeded" && <button className="menu-action" type="button" onClick={onRetry}><RotateCcw size={16} aria-hidden="true" />{t("retry")}</button>}{task.artifacts.filter((artifact) => artifact.kind !== "markdown" && artifact.kind !== "asset").map((artifact) => <span className="artifact-menu" key={artifact.storageKey}><button className="menu-action" type="button" onClick={() => void download(artifact.storageKey)}>{artifact.kind === "bundle" ? <Package size={16} aria-hidden="true" /> : artifact.kind === "documentIr" ? <Braces size={16} aria-hidden="true" /> : <FileJson size={16} aria-hidden="true" />}{t("download")} {artifactLabel(artifact)}</button>{artifact.kind !== "bundle" && <button className="menu-action" type="button" onClick={() => void openPreview(artifact)}><Eye size={16} aria-hidden="true" />{t("preview")} {artifactLabel(artifact)}</button>}</span>)}{!busy && <button className="menu-action danger" type="button" onClick={() => void removeHistory()}><Trash2 size={16} aria-hidden="true" />{t("deleteTask")}</button>}</div></details>
    </div>
    <details className="task-details"><summary>{t("taskDetails")}</summary><dl><div><dt>ID</dt><dd><code>{task.id}</code></dd></div><div><dt>{t("created")}</dt><dd>{new Date(task.createdAtMs).toLocaleString()}</dd></div><div><dt>{t("updated")}</dt><dd>{new Date(task.updatedAtMs).toLocaleString()}</dd></div><div><dt>OCR</dt><dd>{task.configuration.ocrEnabled ? t("on") : t("off")}</dd></div></dl></details>
    {task.status === "succeeded" && task.artifacts.some((artifact) => artifact.kind === "asset") && <details className="asset-browser"><summary><Paperclip size={16} aria-hidden="true" />{t("resources")} ({task.artifacts.filter((artifact) => artifact.kind === "asset").length})</summary><ul>{task.artifacts.filter((artifact) => artifact.kind === "asset").map((artifact) => <li key={artifact.storageKey}><span>{artifactLabel(artifact)} · {artifact.mediaType ?? "application/octet-stream"} · {bytesLabel(artifact.byteLen)}</span><button className="secondary" type="button" onClick={() => void download(artifact.storageKey)}><Download size={15} aria-hidden="true" />{t("download")}</button></li>)}</ul></details>}
    {preview && featured && <section className="preview-panel" aria-label={`${t("preview")} ${artifactLabel(preview.artifact)}`}><div className="preview-title"><div><p className="section-kicker">{t("latestResult")}</p><h4>{artifactLabel(preview.artifact)}</h4></div><button className="icon-button neutral" type="button" aria-label={t("closePreview")} onClick={() => setPreview(null)}><X size={18} aria-hidden="true" /></button></div>{preview.error ? <p role="alert">{t("previewFailed")}</p> : !preview.value ? <div className="preview-loading" role="status"><LoaderCircle className="spin" size={20} aria-hidden="true" />{t("loadingPreview")}</div> : <>{preview.value.truncated && <p className="preview-notice" role="status">{t("previewTruncated")}</p>}{preview.artifact.kind === "markdown" ? <SafeMarkdownPreview source={preview.value.text} /> : <JsonPreview source={preview.value.text} truncated={preview.value.truncated} />}</>}</section>}
  </article>;
}

function Workbench({ api }: { api: ApiClient }) {
  const { t } = useI18n(); const input = useRef<HTMLInputElement>(null); const directory = useRef<HTMLInputElement>(null);
  const watchers = useRef(new Map<string, AbortController>()); const filesByTask = useRef(new Map<string, File>());
  const [files, setFiles] = useState<File[]>([]); const [tasks, setTasks] = useState<TaskRecord[]>([]); const [names, setNames] = useState<Record<string, string>>({}); const [formats, setFormats] = useState<Record<string, string>>({}); const [stages, setStages] = useState<Record<string, string>>({});
  const [options, setOptions] = useState(defaultWorkbenchOptions); const [loading, setLoading] = useState(true); const [statusFilter, setStatusFilter] = useState<TaskStatus | "">(""); const [pinnedOnly, setPinnedOnly] = useState(false); const [nextCursor, setNextCursor] = useState<TaskCursor>();
  const [uploading, setUploading] = useState(false); const [cleaning, setCleaning] = useState(false); const [message, setMessage] = useState(""); const [dragging, setDragging] = useState(false);
  const update = useCallback((task: TaskRecord) => setTasks((current) => [task, ...current.filter((item) => item.id !== task.id)].sort((a, b) => b.updatedAtMs - a.updatedAtMs)), []);
  const watch = useCallback((task: TaskRecord) => { if (TERMINAL.has(task.status) || watchers.current.has(task.id)) return; const controller = new AbortController(); watchers.current.set(task.id, controller); void api.watchTask(task.id, (event) => { setStages((current) => ({ ...current, [task.id]: event.execution?.stage ?? event.status })); setTasks((current) => current.map((item) => item.id === task.id ? { ...item, status: event.status, progressMillionths: event.progressMillionths, updatedAtMs: Date.now() } : item)); if (event.terminal) void api.getTask(task.id).then(update).finally(() => watchers.current.delete(task.id)); }, controller.signal).catch(() => { if (!controller.signal.aborted) setMessage(t("streamError")); }).finally(() => watchers.current.delete(task.id)); }, [api, t, update]);
  const loadHistory = useCallback(async (after?: TaskCursor, signal?: AbortSignal) => { const page = await api.listTasks({ limit: 25, ...(after ? { after } : {}), ...(statusFilter ? { status: statusFilter } : {}), ...(pinnedOnly ? { pinned: true } : {}) }, signal); setTasks((current) => after ? [...current, ...page.tasks.filter((task) => !current.some((item) => item.id === task.id))] : page.tasks); setNextCursor(page.nextCursor); }, [api, pinnedOnly, statusFilter]);
  useEffect(() => { const controller = new AbortController(); setLoading(true); void loadHistory(undefined, controller.signal).catch(() => { if (!controller.signal.aborted) setMessage(t("loadTasksError")); }).finally(() => { if (!controller.signal.aborted) setLoading(false); }); return () => { controller.abort(); watchers.current.forEach((watcher) => watcher.abort()); watchers.current.clear(); }; }, [loadHistory, t]);
  useEffect(() => { tasks.forEach(watch); }, [tasks, watch]);
  const addFiles = (incoming: File[]) => { const key = (file: File) => `${file.webkitRelativePath || file.name}\0${file.size}\0${file.lastModified}`; const seen = new Set(files.map(key)); const unique = incoming.filter((file) => { const value = key(file); if (seen.has(value)) return false; seen.add(value); return true; }); const combined = [...files, ...unique]; const total = combined.reduce((sum, file) => sum + file.size, 0); if (combined.length > MAX_BATCH_FILES) setMessage(t("tooManyFiles")); else if (combined.some((file) => file.size > options.maxInputMiB * 1024 * 1024)) setMessage(t("fileTooLarge")); else if (total > MAX_BATCH_BYTES) setMessage(t("batchTooLarge")); else { setFiles(combined); setMessage(""); } };
  const submit = async () => { if (!files.length) return; if (options.aiMode !== "off" && !options.authorizeProvider) { setMessage(t("authorizationRequired")); return; } setUploading(true); const pending = [...files]; setFiles([]); for (const file of pending) { try { const task = await api.upload(file, options); filesByTask.current.set(task.id, file); setNames((current) => ({ ...current, [task.id]: file.name })); setFormats((current) => ({ ...current, [task.id]: formatForFile(file, options.format) })); update(task); watch(task); } catch (error) { const code = error instanceof ApiError ? error.code : "unreachable"; setMessage(`${t("uploadFailed")}: ${file.name} (${code})`); } } setUploading(false); };
  const retry = async (task: TaskRecord) => { try { const retried = await api.retry(task.id); update(retried); watch(retried); } catch { const file = filesByTask.current.get(task.id); if (file) { setFiles((current) => [...current, file]); document.getElementById("upload-zone")?.focus(); } else setMessage(t("retryNeedsFile")); } };
  const cleanup = async () => { if (!window.confirm(t("cleanupWarning"))) return; setCleaning(true); try { const result = await api.cleanup(); await loadHistory(); setMessage(t("cleanupResult").replace("{tasks}", String(result.deletedTasks)).replace("{bytes}", (result.reclaimedBytes / 1048576).toFixed(1))); } catch { setMessage(t("loadTasksError")); } finally { setCleaning(false); } };
  const selectedBytes = useMemo(() => files.reduce((sum, file) => sum + file.size, 0), [files]);
  return <><div className="page-heading"><p className="eyebrow">LOCAL WORKBENCH</p><h1>{t("convertDocuments")}</h1><p>{t("convertDocumentsIntro")}</p></div>
    <section className="conversion-layout" aria-labelledby="upload-heading"><div className="card upload-card"><div className="card-heading"><div><p className="section-kicker">{t("sourceFiles")}</p><h2 id="upload-heading">{t("addDocuments")}</h2></div>{files.length > 0 && <span className="file-count">{files.length}</span>}</div>
      <div className="drop-zone-shell"><div id="upload-zone" className={`drop-zone ${dragging ? "dragging" : ""}`} role="button" tabIndex={0} onClick={() => input.current?.click()} onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); input.current?.click(); } }} onDragEnter={(event) => { event.preventDefault(); setDragging(true); }} onDragOver={(event) => event.preventDefault()} onDragLeave={() => setDragging(false)} onDrop={(event) => { event.preventDefault(); setDragging(false); addFiles(Array.from(event.dataTransfer.files)); }}><span className="upload-icon" aria-hidden="true"><UploadCloud size={28} /></span><strong>{t("dropFiles")}</strong></div><button className="secondary add-file-button" type="button" onClick={() => input.current?.click()}><Plus size={17} aria-hidden="true" />{t("chooseFiles")}</button></div>
      <input ref={input} className="visually-hidden" type="file" multiple aria-label={t("chooseFiles")} onChange={(event) => addFiles(Array.from(event.target.files ?? []))} /><input ref={directory} className="visually-hidden" type="file" multiple aria-label={t("chooseFolder")} {...({ webkitdirectory: "" } as Record<string, string>)} onChange={(event) => addFiles(Array.from(event.target.files ?? []))} />
      <div className="picker-actions"><button className="text-button" type="button" onClick={() => directory.current?.click()}><FolderOpen size={16} aria-hidden="true" />{t("chooseFolder")}</button><span>{t("batchLimitSummary")}</span></div>
      {files.length > 0 && <div className="selection"><div className="selection-title"><strong>{t("selectedFiles")} ({files.length})</strong><span>{bytesLabel(selectedBytes)}</span></div><ul>{files.map((file, index) => { const format = formatForFile(file, options.format); const FormatIcon = iconForFormat(format); return <li key={`${file.name}-${file.lastModified}`}><span className="file-type-icon"><FormatIcon size={20} aria-hidden="true" /></span><span className="selected-file-name"><strong>{file.webkitRelativePath || file.name}</strong><small>{format.toUpperCase()} · {bytesLabel(file.size)}</small></span><button className="icon-button" type="button" aria-label={`${t("remove")} ${file.name}`} onClick={() => setFiles((current) => current.filter((_, item) => item !== index))}><X size={17} aria-hidden="true" /></button></li>; })}</ul></div>}
    </div><div className="control-column"><CapabilityStrip audioEnabled={options.audioTranscription} onAudioChange={(audioTranscription) => setOptions((current) => ({ ...current, audioTranscription }))} /><OptionPanel value={options} onChange={setOptions} disabled={uploading} /><button className="convert-button" type="button" disabled={files.length === 0 || uploading} onClick={() => void submit()}>{uploading ? <LoaderCircle className="spin" size={19} aria-hidden="true" /> : <Sparkles size={19} aria-hidden="true" />}{uploading ? t("uploading") : `${t("convert")} ${files.length || ""}`}</button></div></section>
    <div className={`message-bar ${message ? "visible" : ""}`} role="status" aria-live="polite">{message && <><CircleAlert size={17} aria-hidden="true" />{message}</>}</div>
    <section className="task-section" aria-labelledby="tasks-heading"><div className="section-heading"><div><p className="section-kicker">{t("resultsAndHistory")}</p><h2 id="tasks-heading">{t("tasks")}</h2></div><details className="history-tools"><summary><SlidersHorizontal size={16} aria-hidden="true" />{t("manageHistory")}</summary><div className="history-controls"><label><span>{t("filterStatus")}</span><select value={statusFilter} onChange={(event) => setStatusFilter(event.target.value as TaskStatus | "")}><option value="">{t("allStatuses")}</option>{["pending", "running", "converted", "succeeded", "failed", "interrupted", "cancelled"].map((status) => <option key={status} value={status}>{t(status as TaskStatus)}</option>)}</select></label><label className="check"><input type="checkbox" checked={pinnedOnly} onChange={(event) => setPinnedOnly(event.target.checked)} />{t("pinnedOnly")}</label><button className="secondary" type="button" disabled={cleaning} onClick={() => void cleanup()}><Trash2 size={15} aria-hidden="true" />{t("cleanup")}</button><button className="secondary" type="button" onClick={() => void loadHistory()}><RefreshCw size={15} aria-hidden="true" />{t("refresh")}</button></div></details></div>{loading ? <div className="loading-state" role="status"><LoaderCircle className="spin" size={21} aria-hidden="true" />{t("loading")}</div> : tasks.length === 0 ? <div className="card empty-tasks"><span className="empty-task-icon"><History size={24} aria-hidden="true" /></span><h3>{t("noTasks")}</h3></div> : <><div className="task-list">{tasks.map((task, index) => <TaskCard key={task.id} task={task} name={names[task.id]} format={formats[task.id]} stage={stages[task.id]} featured={index === 0} api={api} onUpdate={update} onRetry={() => void retry(task)} onDelete={() => setTasks((current) => current.filter((item) => item.id !== task.id))} />)}</div>{nextCursor && <button className="secondary load-more" type="button" onClick={() => void loadHistory(nextCursor)}>{t("loadMore")}</button>}</>}</section>
  </>;
}

function StatusPage({ api }: { api: ApiClient }) {
  const { t } = useI18n(); const [status, setStatus] = useState<"loading" | "ok" | "error">("loading"); const [attempt, setAttempt] = useState(0);
  useEffect(() => { const controller = new AbortController(); setStatus("loading"); void api.status(controller.signal).then((value) => setStatus(value.localApi.available && value.documentConsole.available ? "ok" : "error"), () => { if (!controller.signal.aborted) setStatus("error"); }); return () => controller.abort(); }, [api, attempt]);
  const Icon = status === "ok" ? ShieldCheck : status === "error" ? CircleAlert : LoaderCircle;
  return <><div className="page-heading status-heading"><p className="eyebrow">LOCAL SERVICE</p><h1>{t("capabilityCenter")}</h1><p>{t("capabilityCenterIntro")}</p></div><section className={`card status-card ${status}`} role={status === "error" ? "alert" : "status"}><span className="status-icon"><Icon size={24} aria-hidden="true" className={status === "loading" ? "spin" : ""} /></span><div><h2>{status === "ok" ? t("apiAvailable") : status === "error" ? t("errorTitle") : t("loading")}</h2><p>{status === "ok" ? t("allLocalServicesReady") : status === "error" ? t("errorDetail") : t("checkingSystemDetail")}</p>{status === "error" && <button type="button" onClick={() => setAttempt((value) => value + 1)}><RefreshCw size={16} aria-hidden="true" />{t("retry")}</button>}</div></section><CapabilityStrip /><RouteLink className="back-link" href="/workbench"><FileText size={16} aria-hidden="true" />{t("backWorkbench")}</RouteLink></>;
}

function Content({ api }: { api: ApiClient }) { const { path } = useRouter(); const { t } = useI18n(); const main = useRef<HTMLElement>(null); useEffect(() => { document.title = `${path === "/status" ? t("status") : t("workbench")} · into-markdown`; }, [path, t]); useEffect(() => { main.current?.focus(); }, [path]); return <main id="main" ref={main} tabIndex={-1}>{path === "/status" ? <StatusPage api={api} /> : path === "/" || path === "/workbench" ? <Workbench api={api} /> : <section className="card not-found"><p className="error-number">404</p><h1>{t("notFound")}</h1><RouteLink href="/workbench">{t("backWorkbench")}</RouteLink></section>}</main>; }
function Shell({ api }: { api: ApiClient }) { const { t } = useI18n(); return <Router><a className="skip-link" href="#main">{t("skip")}</a><div className="app-shell"><header><RouteLink href="/workbench" className="brand" aria-label={t("appName")}><span className="brand-mark" aria-hidden="true">M↓</span><span>into-markdown</span></RouteLink><div className="header-actions"><ServiceBadge api={api} /><Preferences /></div></header><Content api={api} /></div></Router>; }
export function App({ api }: { api: ApiClient }) { return <I18nProvider><ThemeProvider><Shell api={api} /></ThemeProvider></I18nProvider>; }

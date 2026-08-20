import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { AiMode, ApiClient, ArtifactPreview, ArtifactReference, AssetMode, InputFormat, NetworkMode, OcrPolicy, TaskCursor, TaskRecord, TaskStatus, WorkbenchOptions } from "./api";
import { ApiError, defaultWorkbenchOptions } from "./api";
import { I18nProvider, useI18n } from "./i18n";
import { RouteLink, Router, useRouter } from "./router";
import { ThemeProvider, useTheme } from "./theme";
import { JsonPreview, SafeMarkdownPreview } from "./preview";

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

function Preferences() {
  const { locale, setLocale, t } = useI18n(); const { theme, setTheme } = useTheme();
  return <div className="preferences">
    <label><span>{t("language")}</span><select value={locale} onChange={(event) => setLocale(event.target.value === "zh-CN" ? "zh-CN" : "en")}><option value="zh-CN">简体中文</option><option value="en">English</option></select></label>
    <label><span>{t("theme")}</span><select value={theme} onChange={(event) => { const value = event.target.value; setTheme(value === "light" || value === "dark" ? value : "system"); }}><option value="system">{t("system")}</option><option value="light">{t("light")}</option><option value="dark">{t("dark")}</option></select></label>
  </div>;
}

function OptionPanel({ value, onChange, disabled }: { value: WorkbenchOptions; onChange(value: WorkbenchOptions): void; disabled: boolean }) {
  const { t } = useI18n();
  const patch = <K extends keyof WorkbenchOptions>(key: K, next: WorkbenchOptions[K]) => onChange({ ...value, [key]: next });
  return <fieldset className="options-panel" disabled={disabled}><legend>{t("options")}</legend>
    <div className="option-grid">
      <label><span>{t("formatHint")}</span><select value={value.format ?? ""} onChange={(e) => patch("format", (e.target.value || null) as InputFormat | null)}><option value="">{t("automatic")}</option>{FORMATS.map((format) => <option key={format} value={format}>{format}</option>)}</select></label>
      <label><span>{t("ocr")}</span><select value={value.ocrPolicy} onChange={(e) => patch("ocrPolicy", e.target.value as OcrPolicy)}><option value="off">{t("off")}</option><option value="auto">{t("automatic")}</option><option value="always">{t("always")}</option></select></label>
      <label><span>{t("ocrConfidence")}</span><input type="number" min="0" max="1" step="0.05" value={value.ocrConfidence} onChange={(e) => patch("ocrConfidence", Number(e.target.value))} /></label>
      <label><span>{t("aiMode")}</span><select value={value.aiMode} onChange={(e) => patch("aiMode", e.target.value as AiMode)}><option value="off">{t("off")}</option><option value="fallback">Fallback</option><option value="prefer">Prefer</option><option value="only">Only</option></select></label>
      <label><span>{t("assetMode")}</span><select value={value.assetMode} onChange={(e) => patch("assetMode", e.target.value as AssetMode)}><option value="extract">Extract</option><option value="embed">Embed</option><option value="omit">Omit</option></select></label>
      <label><span>{t("maxInput")}</span><input type="number" min="1" max="512" value={value.maxInputMiB} onChange={(e) => patch("maxInputMiB", Number(e.target.value))} /></label>
      <label><span>{t("maxMemory")}</span><input type="number" min="1" max="256" value={value.maxMemoryMiB} onChange={(e) => patch("maxMemoryMiB", Number(e.target.value))} /></label>
      <label><span>{t("maxPages")}</span><input type="number" min="1" max="10000" value={value.maxPages} onChange={(e) => patch("maxPages", Number(e.target.value))} /></label>
    </div>
    <div className="authorization-box">
      <label className="check"><input type="checkbox" checked={value.networkMode === "unrestricted"} onChange={(e) => patch("networkMode", (e.target.checked ? "unrestricted" : "restricted") as NetworkMode)} />{t("networkAccess")}</label>
      <p>{t(value.networkMode === "unrestricted" ? "networkEnabledNote" : "networkDisabledNote")}</p>
      {value.aiMode !== "off" && <label className="check grant"><input type="checkbox" checked={value.authorizeProvider} onChange={(e) => patch("authorizeProvider", e.target.checked)} />{t("authorizeProvider")}</label>}
      {value.aiMode !== "off" && <p>{t("authorizationNote")}</p>}
    </div>
  </fieldset>;
}

function TaskCard({ task, name, format, stage, api, onUpdate, onRetry, onDelete }: {
  task: TaskRecord; name?: string | undefined; format?: string | undefined; stage?: string | undefined; api: ApiClient; onUpdate(task: TaskRecord): void; onRetry(): void; onDelete(): void;
}) {
  const { t } = useI18n(); const busy = !TERMINAL.has(task.status); const percent = Math.round(task.progressMillionths / 10_000);
  const [preview, setPreview] = useState<{ artifact: ArtifactReference; value?: ArtifactPreview; error?: boolean } | null>(null);
  const cancel = async () => { try { onUpdate(await api.cancel(task.id)); } catch { /* SSE remains authoritative */ } };
  const download = async (key: string) => {
    const result = await api.download(task.id, key); const url = URL.createObjectURL(result.blob); const anchor = document.createElement("a");
    anchor.href = url; anchor.download = result.filename; anchor.click(); URL.revokeObjectURL(url);
  };
  const openPreview = async (artifact: ArtifactReference) => { setPreview({ artifact }); try { const value = await api.preview(task.id, artifact.storageKey); setPreview({ artifact, value }); } catch { setPreview({ artifact, error: true }); } };
  const pin = async () => { try { onUpdate(await api.setPinned(task.id, !task.pinned)); } catch { /* refresh remains available */ } };
  const removeHistory = async () => { if (!window.confirm(t("deleteWarning"))) return; try { await api.deleteTask(task.id); onDelete(); } catch { /* keep the durable card visible */ } };
  const artifactLabel = (artifact: ArtifactReference) => artifact.filename ?? ({ markdown: "result.md", documentIr: "document-ir.json", diagnostics: "diagnostics.json", bundle: "result.zip", asset: artifact.assetId ?? "asset" } as const)[artifact.kind];
  return <article className={`task-card ${task.status}`} aria-labelledby={`task-${task.id}`}>
    <div className="task-top"><div><h3 id={`task-${task.id}`}>{name ?? `${t("restoredTask")} ${task.id.slice(0, 8)}`}</h3><p><span className="status-pill">{t(task.status)}</span>{task.pinned ? ` · ${t("pinned")}` : ""}{format ? ` · ${t("detectedFormat")}: ${format}` : ""}{stage ? ` · ${stage}` : ""}</p><small>{new Date(task.updatedAtMs).toLocaleString()}</small></div><strong>{percent}%</strong></div>
    <progress max="100" value={percent} aria-label={`${name ?? task.id}: ${percent}%`} />
    <details className="task-details"><summary>{t("taskDetails")}</summary><dl><div><dt>ID</dt><dd><code>{task.id}</code></dd></div><div><dt>{t("created")}</dt><dd>{new Date(task.createdAtMs).toLocaleString()}</dd></div><div><dt>{t("updated")}</dt><dd>{new Date(task.updatedAtMs).toLocaleString()}</dd></div><div><dt>OCR</dt><dd>{task.configuration.ocrEnabled ? t("on") : t("off")}</dd></div></dl></details>
    {task.diagnostics.length > 0 && <ul className="diagnostics">{task.diagnostics.map((item, index) => <li key={`${item.code}-${index}`}>{item.code}</li>)}</ul>}
    <div className="task-actions">{busy && <button className="secondary danger" type="button" onClick={() => void cancel()}>{t("cancel")}</button>}{!busy && <><button className="secondary" type="button" aria-pressed={task.pinned} onClick={() => void pin()}>{task.pinned ? t("unpin") : t("pin")}</button><button type="button" onClick={onRetry}>{t("retry")}</button><button className="secondary danger" type="button" onClick={() => void removeHistory()}>{t("deleteTask")}</button></>}{task.status === "succeeded" && task.artifacts.map((item) => <span className="artifact-actions" key={item.storageKey}>{item.kind !== "asset" && item.kind !== "bundle" && <button className="secondary" type="button" onClick={() => void openPreview(item)}>{t("preview")} {artifactLabel(item)}</button>}<button className="secondary" type="button" onClick={() => void download(item.storageKey)}>{t("download")} {artifactLabel(item)}</button></span>)}</div>
    {task.status === "succeeded" && task.artifacts.some((item) => item.kind === "asset") && <details className="asset-browser"><summary>{t("resources")} ({task.artifacts.filter((item) => item.kind === "asset").length})</summary><ul>{task.artifacts.filter((item) => item.kind === "asset").map((item) => <li key={item.storageKey}><span>{artifactLabel(item)} · {item.mediaType ?? "application/octet-stream"} · {item.byteLen} B</span><button className="secondary" type="button" onClick={() => void download(item.storageKey)}>{t("download")}</button></li>)}</ul></details>}
    {preview && <section className="preview-panel" aria-label={`${t("preview")} ${artifactLabel(preview.artifact)}`}><div className="preview-title"><h4>{artifactLabel(preview.artifact)}</h4><button className="icon-button" type="button" aria-label={t("closePreview")} onClick={() => setPreview(null)}>×</button></div>{preview.error ? <p role="alert">{t("previewFailed")}</p> : !preview.value ? <p role="status">{t("loading")}</p> : <>{preview.value.truncated && <p className="preview-notice" role="status">{t("previewTruncated")}</p>}{preview.artifact.kind === "markdown" ? <SafeMarkdownPreview source={preview.value.text} /> : <JsonPreview source={preview.value.text} truncated={preview.value.truncated} />}</>}</section>}
  </article>;
}

function Workbench({ api }: { api: ApiClient }) {
  const { t } = useI18n(); const input = useRef<HTMLInputElement>(null); const directory = useRef<HTMLInputElement>(null);
  const watchers = useRef(new Map<string, AbortController>()); const filesByTask = useRef(new Map<string, File>());
  const [files, setFiles] = useState<File[]>([]); const [tasks, setTasks] = useState<TaskRecord[]>([]);
  const [names, setNames] = useState<Record<string, string>>({}); const [formats, setFormats] = useState<Record<string, string>>({}); const [stages, setStages] = useState<Record<string, string>>({});
  const [options, setOptions] = useState(defaultWorkbenchOptions); const [loading, setLoading] = useState(true);
  const [statusFilter, setStatusFilter] = useState<TaskStatus | "">(""); const [pinnedOnly, setPinnedOnly] = useState(false); const [nextCursor, setNextCursor] = useState<TaskCursor>();
  const [uploading, setUploading] = useState(false); const [cleaning, setCleaning] = useState(false); const [message, setMessage] = useState(""); const [dragging, setDragging] = useState(false);
  const update = useCallback((task: TaskRecord) => setTasks((current) => [task, ...current.filter((item) => item.id !== task.id)].sort((a, b) => b.updatedAtMs - a.updatedAtMs)), []);
  const watch = useCallback((task: TaskRecord) => {
    if (TERMINAL.has(task.status) || watchers.current.has(task.id)) return;
    const controller = new AbortController(); watchers.current.set(task.id, controller);
    void api.watchTask(task.id, (event) => {
      setStages((current) => ({ ...current, [task.id]: event.execution?.stage ?? event.status }));
      setTasks((current) => current.map((item) => item.id === task.id ? { ...item, status: event.status, progressMillionths: event.progressMillionths, updatedAtMs: Date.now() } : item));
      if (event.terminal) void api.getTask(task.id).then(update).finally(() => watchers.current.delete(task.id));
    }, controller.signal).catch(() => { if (!controller.signal.aborted) setMessage(t("streamError")); }).finally(() => watchers.current.delete(task.id));
  }, [api, t, update]);
  const loadHistory = useCallback(async (after?: TaskCursor, signal?: AbortSignal) => { const page = await api.listTasks({ limit: 25, ...(after ? { after } : {}), ...(statusFilter ? { status: statusFilter } : {}), ...(pinnedOnly ? { pinned: true } : {}) }, signal); setTasks((current) => after ? [...current, ...page.tasks.filter((task) => !current.some((item) => item.id === task.id))] : page.tasks); setNextCursor(page.nextCursor); }, [api, pinnedOnly, statusFilter]);
  useEffect(() => { const controller = new AbortController(); setLoading(true); void loadHistory(undefined, controller.signal).catch(() => { if (!controller.signal.aborted) setMessage(t("loadTasksError")); }).finally(() => { if (!controller.signal.aborted) setLoading(false); }); return () => { controller.abort(); watchers.current.forEach((watcher) => watcher.abort()); watchers.current.clear(); }; }, [loadHistory, t]);
  useEffect(() => { tasks.forEach(watch); }, [tasks, watch]);

  const addFiles = (incoming: File[]) => {
    const key = (file: File) => `${file.webkitRelativePath || file.name}\0${file.size}\0${file.lastModified}`;
    const seen = new Set(files.map(key)); const unique = incoming.filter((file) => { const value = key(file); if (seen.has(value)) return false; seen.add(value); return true; });
    const combined = [...files, ...unique]; const total = combined.reduce((sum, file) => sum + file.size, 0);
    if (combined.length > MAX_BATCH_FILES) setMessage(t("tooManyFiles"));
    else if (combined.some((file) => file.size > options.maxInputMiB * 1024 * 1024)) setMessage(t("fileTooLarge"));
    else if (total > MAX_BATCH_BYTES) setMessage(t("batchTooLarge"));
    else { setFiles(combined); setMessage(""); }
  };
  const submit = async () => {
    if (!files.length) return;
    if (options.aiMode !== "off" && !options.authorizeProvider) { setMessage(t("authorizationRequired")); return; }
    setUploading(true); const pending = [...files]; setFiles([]);
    for (const file of pending) {
      try { const task = await api.upload(file, options); filesByTask.current.set(task.id, file); setNames((current) => ({ ...current, [task.id]: file.name })); setFormats((current) => ({ ...current, [task.id]: formatForFile(file, options.format) })); update(task); watch(task); }
      catch (error) {
        const code = error instanceof ApiError ? error.code : "unreachable";
        setMessage(`${t("uploadFailed")}: ${file.name} (${code})`);
      }
    }
    setUploading(false);
  };
  const retry = async (task: TaskRecord) => { try { const retried = await api.retry(task.id); update(retried); watch(retried); } catch { const file = filesByTask.current.get(task.id); if (file) { setFiles((current) => [...current, file]); document.getElementById("upload-zone")?.focus(); } else setMessage(t("retryNeedsFile")); } };
  const cleanup = async () => {
    if (!window.confirm(t("cleanupWarning"))) return;
    setCleaning(true);
    try {
      const result = await api.cleanup();
      await loadHistory();
      setMessage(t("cleanupResult").replace("{tasks}", String(result.deletedTasks)).replace("{bytes}", (result.reclaimedBytes / 1048576).toFixed(1)));
    } catch { setMessage(t("loadTasksError")); }
    finally { setCleaning(false); }
  };
  const selectedBytes = useMemo(() => files.reduce((sum, file) => sum + file.size, 0), [files]);
  return <><div className="page-heading"><p className="eyebrow">LOCAL WORKBENCH</p><h1>{t("workbench")}</h1><p>{t("workbenchIntro")}</p></div>
    <section className="upload-layout" aria-labelledby="upload-heading"><div className="card upload-card"><h2 id="upload-heading">{t("addDocuments")}</h2>
      <div id="upload-zone" className={`drop-zone ${dragging ? "dragging" : ""}`} role="button" tabIndex={0} onClick={() => input.current?.click()} onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); input.current?.click(); } }} onDragEnter={(e) => { e.preventDefault(); setDragging(true); }} onDragOver={(e) => e.preventDefault()} onDragLeave={() => setDragging(false)} onDrop={(e) => { e.preventDefault(); setDragging(false); addFiles(Array.from(e.dataTransfer.files)); }}>
        <span className="upload-icon" aria-hidden="true">⇧</span><strong>{t("dropFiles")}</strong><span>{t("orChoose")}</span></div>
      <input ref={input} className="visually-hidden" type="file" multiple aria-label={t("chooseFiles")} onChange={(e) => addFiles(Array.from(e.target.files ?? []))} />
      <input ref={directory} className="visually-hidden" type="file" multiple aria-label={t("chooseFolder")} {...({ webkitdirectory: "" } as Record<string, string>)} onChange={(e) => addFiles(Array.from(e.target.files ?? []))} />
      <div className="picker-actions"><button className="secondary" type="button" onClick={() => input.current?.click()}>{t("chooseFiles")}</button><button className="secondary" type="button" onClick={() => directory.current?.click()}>{t("chooseFolder")}</button></div>
      {files.length > 0 && <div className="selection"><div className="selection-title"><strong>{t("selectedFiles")} ({files.length})</strong><span>{(selectedBytes / 1048576).toFixed(1)} MiB</span></div><ul>{files.map((file, index) => <li key={`${file.name}-${file.lastModified}`}><span>{file.webkitRelativePath || file.name} <small>· {t("detectedFormat")}: {formatForFile(file, options.format)}</small></span><button className="icon-button" type="button" aria-label={`${t("remove")} ${file.name}`} onClick={() => setFiles((current) => current.filter((_, item) => item !== index))}>×</button></li>)}</ul></div>}
    </div><OptionPanel value={options} onChange={setOptions} disabled={uploading} /></section>
    <div className="submit-row"><button type="button" disabled={files.length === 0 || uploading} onClick={() => void submit()}>{uploading ? t("uploading") : `${t("convert")} ${files.length || ""}`}</button><span role="status" aria-live="polite">{message}</span></div>
    <section className="task-section" aria-labelledby="tasks-heading"><div className="section-heading"><h2 id="tasks-heading">{t("tasks")}</h2><div className="history-controls"><label><span>{t("filterStatus")}</span><select value={statusFilter} onChange={(event) => setStatusFilter(event.target.value as TaskStatus | "")}><option value="">{t("allStatuses")}</option>{["pending", "running", "converted", "succeeded", "failed", "interrupted", "cancelled"].map((status) => <option key={status} value={status}>{t(status as TaskStatus)}</option>)}</select></label><label className="check"><input type="checkbox" checked={pinnedOnly} onChange={(event) => setPinnedOnly(event.target.checked)} />{t("pinnedOnly")}</label><button className="secondary" type="button" disabled={cleaning} onClick={() => void cleanup()}>{t("cleanup")}</button><button className="secondary" type="button" onClick={() => void loadHistory()}>{t("refresh")}</button></div></div>{loading ? <p role="status">{t("loading")}</p> : tasks.length === 0 ? <div className="card empty-tasks"><h3>{t("noTasks")}</h3><p>{t("noTasksDetail")}</p></div> : <><div className="task-list">{tasks.map((task) => <TaskCard key={task.id} task={task} name={names[task.id]} format={formats[task.id]} stage={stages[task.id]} api={api} onUpdate={update} onRetry={() => void retry(task)} onDelete={() => setTasks((current) => current.filter((item) => item.id !== task.id))} />)}</div>{nextCursor && <button className="secondary load-more" type="button" onClick={() => void loadHistory(nextCursor)}>{t("loadMore")}</button>}</>}</section>
  </>;
}

function StatusPage({ api }: { api: ApiClient }) {
  const { t } = useI18n(); const [status, setStatus] = useState<"loading" | "ok" | "error">("loading"); const [attempt, setAttempt] = useState(0);
  useEffect(() => { const controller = new AbortController(); setStatus("loading"); void api.status(controller.signal).then(() => setStatus("ok"), () => { if (!controller.signal.aborted) setStatus("error"); }); return () => controller.abort(); }, [api, attempt]);
  return <><div className="page-heading"><p className="eyebrow">LOCAL CONSOLE</p><h1>{t("status")}</h1></div><section className="card status-card" role={status === "error" ? "alert" : "status"}><span className={`status-icon ${status === "ok" ? "ok" : status === "error" ? "error" : ""}`}>{status === "ok" ? "✓" : status === "error" ? "!" : "…"}</span><div><h2>{status === "ok" ? t("apiAvailable") : status === "error" ? t("errorTitle") : t("loading")}</h2>{status === "error" && <button type="button" onClick={() => setAttempt((value) => value + 1)}>{t("retry")}</button>}</div></section></>;
}
function Content({ api }: { api: ApiClient }) {
  const { path } = useRouter(); const { t } = useI18n(); const main = useRef<HTMLElement>(null);
  useEffect(() => { document.title = `${path === "/status" ? t("status") : t("workbench")} · into-markdown`; }, [path, t]);
  useEffect(() => { main.current?.focus(); }, [path]);
  return <main id="main" ref={main} tabIndex={-1}>{path === "/status" ? <StatusPage api={api} /> : path === "/" || path === "/workbench" ? <Workbench api={api} /> : <section className="card not-found"><p className="error-number">404</p><h1>{t("notFound")}</h1><RouteLink href="/workbench">{t("backWorkbench")}</RouteLink></section>}</main>;
}
function Shell({ api }: { api: ApiClient }) { const { t } = useI18n(); return <Router><a className="skip-link" href="#main">{t("skip")}</a><div className="app-shell"><header><RouteLink href="/workbench" className="brand" aria-label={t("appName")}><span className="brand-mark" aria-hidden="true">M↓</span><span>into-markdown</span></RouteLink><Preferences /></header><div className="body-shell"><nav aria-label={t("appName")}><RouteLink href="/workbench" className="nav-link">{t("workbench")}</RouteLink><RouteLink href="/status" className="nav-link">{t("status")}</RouteLink></nav><Content api={api} /></div></div></Router>; }
export function App({ api }: { api: ApiClient }) { return <I18nProvider><ThemeProvider><Shell api={api} /></ThemeProvider></I18nProvider>; }

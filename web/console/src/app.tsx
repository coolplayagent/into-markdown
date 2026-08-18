import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { AiMode, ApiClient, AssetMode, InputFormat, OcrPolicy, TaskRecord, WorkbenchOptions } from "./api";
import { defaultWorkbenchOptions } from "./api";
import { I18nProvider, useI18n } from "./i18n";
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
      <label className="check"><input type="checkbox" checked={value.networkEnabled} onChange={(e) => onChange(e.target.checked ? { ...value, networkEnabled: true } : { ...value, networkEnabled: false, privateNetworkEnabled: false, authorizeNetwork: false, authorizePrivateNetwork: false })} />{t("enableNetwork")}</label>
      {value.networkEnabled && <><label><span>{t("allowedHosts")}</span><input value={value.allowedHosts.join(", ")} placeholder="api.example.com" onChange={(e) => patch("allowedHosts", e.target.value.split(",").map((item) => item.trim()).filter(Boolean))} /></label><label className="check grant"><input type="checkbox" checked={value.authorizeNetwork} onChange={(e) => patch("authorizeNetwork", e.target.checked)} />{t("authorizeNetwork")}</label></>}
      <label className="check"><input type="checkbox" checked={value.privateNetworkEnabled} disabled={!value.networkEnabled} onChange={(e) => patch("privateNetworkEnabled", e.target.checked)} />{t("enablePrivate")}</label>
      {value.privateNetworkEnabled && <label className="check grant"><input type="checkbox" checked={value.authorizePrivateNetwork} onChange={(e) => patch("authorizePrivateNetwork", e.target.checked)} />{t("authorizePrivate")}</label>}
      {value.aiMode !== "off" && <label className="check grant"><input type="checkbox" checked={value.authorizeProvider} onChange={(e) => patch("authorizeProvider", e.target.checked)} />{t("authorizeProvider")}</label>}
      <p>{t("authorizationNote")}</p>
    </div>
  </fieldset>;
}

function TaskCard({ task, name, format, stage, api, onUpdate, onRetry }: {
  task: TaskRecord; name?: string | undefined; format?: string | undefined; stage?: string | undefined; api: ApiClient; onUpdate(task: TaskRecord): void; onRetry(): void;
}) {
  const { t } = useI18n(); const busy = !TERMINAL.has(task.status); const percent = Math.round(task.progressMillionths / 10_000);
  const cancel = async () => { try { onUpdate(await api.cancel(task.id)); } catch { /* SSE remains authoritative */ } };
  const download = async (key: string, filename: string) => {
    const blob = await api.download(task.id, key); const url = URL.createObjectURL(blob); const anchor = document.createElement("a");
    anchor.href = url; anchor.download = filename; anchor.click(); URL.revokeObjectURL(url);
  };
  return <article className={`task-card ${task.status}`} aria-labelledby={`task-${task.id}`}>
    <div className="task-top"><div><h3 id={`task-${task.id}`}>{name ?? `${t("restoredTask")} ${task.id.slice(0, 8)}`}</h3><p><span className="status-pill">{t(task.status)}</span>{format ? ` · ${t("detectedFormat")}: ${format}` : ""}{stage ? ` · ${stage}` : ""}</p></div><strong>{percent}%</strong></div>
    <progress max="100" value={percent} aria-label={`${name ?? task.id}: ${percent}%`} />
    {task.diagnostics.length > 0 && <ul className="diagnostics">{task.diagnostics.map((item, index) => <li key={`${item.code}-${index}`}>{item.code}</li>)}</ul>}
    <div className="task-actions">{busy && <button className="secondary danger" type="button" onClick={() => void cancel()}>{t("cancel")}</button>}{(task.status === "failed" || task.status === "interrupted") && <button type="button" onClick={onRetry}>{t("retry")}</button>}{task.status === "succeeded" && task.artifacts.filter((item) => item.kind === "markdown" || item.kind === "bundle").map((item) => <button className="secondary" type="button" key={item.storageKey} onClick={() => void download(item.storageKey, item.kind === "bundle" ? "result.zip" : "result.md")}>{item.kind === "bundle" ? t("downloadBundle") : t("downloadMarkdown")}</button>)}</div>
  </article>;
}

function Workbench({ api }: { api: ApiClient }) {
  const { t } = useI18n(); const input = useRef<HTMLInputElement>(null); const directory = useRef<HTMLInputElement>(null);
  const watchers = useRef(new Map<string, AbortController>()); const filesByTask = useRef(new Map<string, File>());
  const [files, setFiles] = useState<File[]>([]); const [tasks, setTasks] = useState<TaskRecord[]>([]);
  const [names, setNames] = useState<Record<string, string>>({}); const [formats, setFormats] = useState<Record<string, string>>({}); const [stages, setStages] = useState<Record<string, string>>({});
  const [options, setOptions] = useState(defaultWorkbenchOptions); const [loading, setLoading] = useState(true);
  const [uploading, setUploading] = useState(false); const [message, setMessage] = useState(""); const [dragging, setDragging] = useState(false);
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
  useEffect(() => { const controller = new AbortController(); void api.listTasks(controller.signal).then((restored) => { setTasks(restored); restored.forEach(watch); }, () => setMessage(t("loadTasksError"))).finally(() => setLoading(false)); return () => { controller.abort(); watchers.current.forEach((watcher) => watcher.abort()); watchers.current.clear(); }; }, [api, t, watch]);
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
    if (!files.length) return; if (options.networkEnabled && !options.authorizeNetwork) { setMessage(t("authorizationRequired")); return; }
    if (options.privateNetworkEnabled && !options.authorizePrivateNetwork) { setMessage(t("authorizationRequired")); return; }
    if (options.aiMode !== "off" && !options.authorizeProvider) { setMessage(t("authorizationRequired")); return; }
    setUploading(true); const pending = [...files]; setFiles([]);
    for (const file of pending) {
      try { const task = await api.upload(file, options); filesByTask.current.set(task.id, file); setNames((current) => ({ ...current, [task.id]: file.name })); setFormats((current) => ({ ...current, [task.id]: formatForFile(file, options.format) })); update(task); watch(task); }
      catch { setMessage(`${t("uploadFailed")}: ${file.name}`); }
    }
    setUploading(false);
  };
  const retry = (task: TaskRecord) => { const file = filesByTask.current.get(task.id); if (file) { setFiles((current) => [...current, file]); document.getElementById("upload-zone")?.focus(); } else setMessage(t("retryNeedsFile")); };
  const selectedBytes = useMemo(() => files.reduce((sum, file) => sum + file.size, 0), [files]);
  return <><div className="page-heading"><p className="eyebrow">LOCAL WORKBENCH</p><h1>{t("workbench")}</h1><p>{t("workbenchIntro")}</p></div>
    <section className="upload-layout" aria-labelledby="upload-heading"><div className="card upload-card"><h2 id="upload-heading">{t("addDocuments")}</h2>
      <div id="upload-zone" className={`drop-zone ${dragging ? "dragging" : ""}`} role="button" tabIndex={0} onClick={() => input.current?.click()} onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); input.current?.click(); } }} onDragEnter={(e) => { e.preventDefault(); setDragging(true); }} onDragOver={(e) => e.preventDefault()} onDragLeave={() => setDragging(false)} onDrop={(e) => { e.preventDefault(); setDragging(false); addFiles(Array.from(e.dataTransfer.files)); }}>
        <span className="upload-icon" aria-hidden="true">⇧</span><strong>{t("dropFiles")}</strong><span>{t("orChoose")}</span></div>
      <input ref={input} className="visually-hidden" type="file" multiple onChange={(e) => addFiles(Array.from(e.target.files ?? []))} />
      <input ref={directory} className="visually-hidden" type="file" multiple {...({ webkitdirectory: "" } as Record<string, string>)} onChange={(e) => addFiles(Array.from(e.target.files ?? []))} />
      <div className="picker-actions"><button className="secondary" type="button" onClick={() => input.current?.click()}>{t("chooseFiles")}</button><button className="secondary" type="button" onClick={() => directory.current?.click()}>{t("chooseFolder")}</button></div>
      {files.length > 0 && <div className="selection"><div className="selection-title"><strong>{t("selectedFiles")} ({files.length})</strong><span>{(selectedBytes / 1048576).toFixed(1)} MiB</span></div><ul>{files.map((file, index) => <li key={`${file.name}-${file.lastModified}`}><span>{file.webkitRelativePath || file.name} <small>· {t("detectedFormat")}: {formatForFile(file, options.format)}</small></span><button className="icon-button" type="button" aria-label={`${t("remove")} ${file.name}`} onClick={() => setFiles((current) => current.filter((_, item) => item !== index))}>×</button></li>)}</ul></div>}
    </div><OptionPanel value={options} onChange={setOptions} disabled={uploading} /></section>
    <div className="submit-row"><button type="button" disabled={files.length === 0 || uploading} onClick={() => void submit()}>{uploading ? t("uploading") : `${t("convert")} ${files.length || ""}`}</button><span role="status" aria-live="polite">{message}</span></div>
    <section className="task-section" aria-labelledby="tasks-heading"><div className="section-heading"><h2 id="tasks-heading">{t("tasks")}</h2><button className="secondary" type="button" onClick={() => void api.listTasks().then(setTasks)}>{t("refresh")}</button></div>{loading ? <p role="status">{t("loading")}</p> : tasks.length === 0 ? <div className="card empty-tasks"><h3>{t("noTasks")}</h3><p>{t("noTasksDetail")}</p></div> : <div className="task-list">{tasks.map((task) => <TaskCard key={task.id} task={task} name={names[task.id]} format={formats[task.id]} stage={stages[task.id]} api={api} onUpdate={update} onRetry={() => retry(task)} />)}</div>}</section>
  </>;
}

function StatusPage({ api }: { api: ApiClient }) {
  const { t } = useI18n(); const [status, setStatus] = useState<"loading" | "ok" | "error">("loading"); const [attempt, setAttempt] = useState(0);
  useEffect(() => { const controller = new AbortController(); setStatus("loading"); void api.status(controller.signal).then(() => setStatus("ok"), () => { if (!controller.signal.aborted) setStatus("error"); }); return () => controller.abort(); }, [api, attempt]);
  return <><div className="page-heading"><p className="eyebrow">LOCAL CONSOLE</p><h1>{t("status")}</h1></div><section className="card status-card" role={status === "error" ? "alert" : "status"}><span className={`status-icon ${status === "ok" ? "ok" : status === "error" ? "error" : ""}`}>{status === "ok" ? "✓" : status === "error" ? "!" : "…"}</span><div><h2>{status === "ok" ? t("apiAvailable") : status === "error" ? t("errorTitle") : t("loading")}</h2>{status === "error" && <button type="button" onClick={() => setAttempt((value) => value + 1)}>{t("retry")}</button>}</div></section></>;
}
function Content({ api }: { api: ApiClient }) {
  const { path } = useRouter(); const { t } = useI18n(); const main = useRef<HTMLElement>(null);
  useEffect(() => { document.title = `${path === "/status" ? t("status") : t("workbench")} · into-markdown`; main.current?.focus(); }, [path, t]);
  return <main id="main" ref={main} tabIndex={-1}>{path === "/status" ? <StatusPage api={api} /> : path === "/" || path === "/workbench" ? <Workbench api={api} /> : <section className="card not-found"><p className="error-number">404</p><h1>{t("notFound")}</h1><RouteLink href="/workbench">{t("backWorkbench")}</RouteLink></section>}</main>;
}
function Shell({ api }: { api: ApiClient }) { const { t } = useI18n(); return <Router><a className="skip-link" href="#main">{t("skip")}</a><div className="app-shell"><header><RouteLink href="/workbench" className="brand" aria-label={t("appName")}><span className="brand-mark" aria-hidden="true">M↓</span><span>into-markdown</span></RouteLink><Preferences /></header><div className="body-shell"><nav aria-label={t("appName")}><RouteLink href="/workbench" className="nav-link">{t("workbench")}</RouteLink><RouteLink href="/status" className="nav-link">{t("status")}</RouteLink></nav><Content api={api} /></div></div></Router>; }
export function App({ api }: { api: ApiClient }) { return <I18nProvider><ThemeProvider><Shell api={api} /></ThemeProvider></I18nProvider>; }

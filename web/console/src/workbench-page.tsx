import { LocalSourceControls, useLocalPicker } from "./local-source-picker";
import { useObservedTaskRuntime } from "./task-provider";
import type { UploadEntry } from "./task-runtime";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  CheckCircle2, CircleAlert, FolderOpen, LoaderCircle, Plus, Sparkles, Square, UploadCloud, X,
} from "lucide-react";
import type { ApiClient, CapabilityAdmin, ComponentStatus, TaskRecord, WorkbenchOptions } from "./api";
import { ApiError, defaultWorkbenchOptions, validResourceLimits } from "./api";
import { CapabilityStrip, OptionPanel } from "./conversion-controls";
import { useI18n } from "./i18n";
import { ResultDialog } from "./result-page";
import { HistoryPanel } from "./history-panel";
import { useCapabilities } from "./capability-store";
import { useRouter } from "./router";
import {
  TERMINAL, bytesLabel, createBatchId, diagnosticLabel, taskFailureLabel, formatForName, listAllTasks,
  executionStageLabel, iconForFormat,
} from "./task-ui";


type BatchEntry = UploadEntry;

type MessageScope = "source" | "controls";

function entryKey(file: File): string {
  return `${file.webkitRelativePath || file.name}\0${file.size}\0${file.lastModified}`;
}

export function WorkbenchPage({ api, initialTaskId }: { api: ApiClient; initialTaskId?: string | undefined }) {
  const { locale, t } = useI18n();
  const { navigate } = useRouter();
  const capabilities = useCapabilities();
  const input = useRef<HTMLInputElement>(null);
  const directory = useRef<HTMLInputElement>(null);
  const watchers = useRef(new Map<string, AbortController>());
  const navigatedBatch = useRef<string | null>(null);
  const runtime = useObservedTaskRuntime();
  const [localEntries, setLocalEntries] = useState<BatchEntry[]>([]);
  const entries = runtime?.entries ?? localEntries;
  const setEntries = runtime?.setEntries ?? setLocalEntries;
  const [batchId, setBatchId] = useState<string | null>(null);
  const [options, setOptions] = useState<WorkbenchOptions>(defaultWorkbenchOptions);
  const [localUploading, setUploading] = useState(false);
  const uploading = runtime?.uploading ?? localUploading;
  const [message, setMessage] = useState("");
  const [messageScope, setMessageScope] = useState<MessageScope>("source");
  const [visibleCount, setVisibleCount] = useState(100);
  const [dragging, setDragging] = useState(false);
  const [recentTasks, setRecentTasks] = useState<TaskRecord[]>([]);
  const [historyFeedback, setHistoryFeedback] = useState<{ kind: "success" | "error"; message: string } | null>(null);
  const [activeTaskId, setActiveTaskId] = useState<string | undefined>(initialTaskId);
  const quickOcr = capabilities.capability("ocr");
  const ocrStatus: ComponentStatus | undefined = quickOcr ? { available: quickOcr.status === "ready", code: quickOcr.status, detail: quickOcr.currentSourceName } : undefined;
  const ocrCapability: CapabilityAdmin | undefined = quickOcr ? { id: "ocr", status: normalizeStatus(quickOcr.status), localStatus: normalizeStatus(quickOcr.localStatus), currentSource: quickOcr.currentSource, sources: quickOcr.sources, ...(quickOcr.version ? { version: quickOcr.version } : {}), ...(quickOcr.localVersion ? { localVersion: quickOcr.localVersion } : {}) } : undefined;

  useEffect(() => setActiveTaskId(initialTaskId), [initialTaskId]);

  const selectTask = useCallback((id: string) => {
    setActiveTaskId(id);
    navigate(`/results/${id}`);
  }, [navigate]);
  const closeResult = useCallback(() => {
    setActiveTaskId(undefined);
    navigate("/workbench");
  }, [navigate]);

  const updateTask = useCallback((id: string, update: (task: TaskRecord) => TaskRecord) => {
    setEntries((current) => current.map((entry) => entry.task?.id === id ? { ...entry, task: update(entry.task) } : entry));
  }, []);

  const watch = useCallback((entryKeyValue: string, task: TaskRecord) => {
    if (TERMINAL.has(task.status) || watchers.current.has(task.id)) return;
    const controller = new AbortController();
    watchers.current.set(task.id, controller);
    void api.watchTask(task.id, (event) => {
      setEntries((current) => current.map((entry) => entry.key === entryKeyValue && entry.task ? {
        ...entry,
        stage: event.execution?.stage ?? event.status,
        task: { ...entry.task, status: event.status, progressMillionths: event.progressMillionths, updatedAtMs: Date.now() },
      } : entry));
      if (event.terminal) {
        void api.getTask(task.id).then((record) => updateTask(task.id, () => record)).finally(() => watchers.current.delete(task.id));
      }
    }, controller.signal).catch(() => {
      if (!controller.signal.aborted) {
        setMessageScope("source");
        setMessage(t("streamError"));
      }
    }).finally(() => watchers.current.delete(task.id));
  }, [api, t, updateTask]);

  useEffect(() => () => {
    watchers.current.forEach((watcher) => watcher.abort());
    watchers.current.clear();
  }, []);

  const batchFinished = entries.length > 0 && entries.every((entry) => entry.error || entry.uploadState === "cancelled" || entry.task && TERMINAL.has(entry.task.status));

  useEffect(() => {
    const controller = new AbortController();
    void (runtime ? api.listTasks({ limit: 100, workflow: "conversion", active: true }, controller.signal).then(page => page.tasks) : listAllTasks(api, controller.signal))
      .then((tasks) => { setRecentTasks(tasks.filter((task) => task.workflow === "conversion")); setHistoryFeedback(null); })
      .catch((error: unknown) => {
        if (!(error instanceof DOMException && error.name === "AbortError")) {
          setHistoryFeedback({ kind: "error", message: t("loadTasksError") });
        }
      });
    return () => controller.abort();
  }, [api, batchFinished, t, runtime]);

  const pendingCount = entries.filter(entry => !entry.task && !entry.batchId).length;
  const selectedBytes = useMemo(() => entries.reduce((sum, entry) => sum + (entry.originalSize ?? entry.file.size), 0), [entries]);
  const remoteOcrSelected = ocrCapability?.currentSource.startsWith("provider:") === true;
  const recentHistory = useMemo(() => {
    const currentIds = new Set(entries.flatMap((entry) => entry.task ? [entry.task.id] : []));
    return recentTasks.filter((task) => TERMINAL.has(task.status) && !currentIds.has(task.id));
  }, [entries, recentTasks]);

  const addFiles = (incoming: File[]) => {
    const combined = mergeFiles(entries, incoming);
    setMessageScope("source");
    if (combined.some((entry) => options.maxInputMiB != null && entry.file.size > options.maxInputMiB * 1024 * 1024)) setMessage(t("fileTooLarge"));
    else {
      if (batchFinished) { setBatchId(null); navigatedBatch.current = null; }
      setEntries(combined);
      setMessage("");
    }
  };

  const localPicker = useLocalPicker(api, setEntries, addFiles, message => { setMessageScope("source"); setMessage(message); });

  const submit = async () => {
    const pending = entries.filter(entry => !entry.task && !entry.batchId);
    if (!pending.length) return;
    if (remoteOcrSelected && options.ocrPolicy !== "off" && options.networkMode !== "unrestricted") {
      setMessageScope("controls");
      setMessage(t("remoteNetworkRequired"));
      return;
    }
    if ((options.aiMode !== "off" || remoteOcrSelected && options.ocrPolicy !== "off") && !options.authorizeProvider) {
      setMessageScope("controls");
      setMessage(t("authorizationRequired"));
      return;
    }
    const nextBatchId = createBatchId();
    setBatchId(nextBatchId);
    navigatedBatch.current = null;
    if (runtime) { runtime.submit(pending.map(entry => entry.key), options, nextBatchId); return; }
    setUploading(true);
    setMessageScope("source");
    setMessage("");
    for (const entry of pending) {
      try {
        const task = await submitEntry(api, entry, options, nextBatchId);
        setEntries((current) => current.map((item) => item.key === entry.key ? { ...item, task, stage: task.status } : item));
        watch(entry.key, task);
      } catch (error) {
        const code = error instanceof ApiError ? error.code : "unreachable";
        setEntries((current) => current.map((item) => item.key === entry.key ? { ...item, error: code } : item));
        setMessageScope("source");
        setMessage(`${entry.file.name}${locale === "zh-CN" ? "：" : ": "}${diagnosticLabel(code, t)}`);
      }
    }
    setUploading(false);
  };

  const cancel = async (task: TaskRecord) => {
    try { const updated = await api.cancel(task.id); updateTask(task.id, () => updated); }
    catch { setMessageScope("source"); setMessage(t("streamError")); }
  };

  const cleanup = async () => {
    if (!window.confirm(t("cleanupWarning"))) return;
    setHistoryFeedback(null);
    try {
      const result = await api.cleanup();
      const tasks = runtime ? (await api.listTasks({ limit: 100, workflow: "conversion", active: true })).tasks : await listAllTasks(api);
      setRecentTasks(tasks.filter((task) => task.workflow === "conversion"));
      setHistoryFeedback({ kind: "success", message: t("cleanupResult").replace("{tasks}", String(result.deletedTasks)).replace("{bytes}", (result.reclaimedBytes / 1048576).toFixed(1)) });
    } catch {
      setHistoryFeedback({ kind: "error", message: t("loadTasksError") });
    }
  };

  return <section className="workbench-route" aria-labelledby="workbench-title">
    <div className="page-heading compact-heading"><div><p className="eyebrow">DOCUMENT TO MARKDOWN</p><h1 id="workbench-title">{t("convertDocuments")}</h1></div></div>
    {runtime?.connectionError && <p className="picker-feedback" role="status">{!runtime.connectionErrorCode || ["unreachable", "requestTimeout"].includes(runtime.connectionErrorCode) ? t("serviceWaiting") : diagnosticLabel(runtime.connectionErrorCode, t)}</p>}
    <div className="task-workspace"><div className="conversion-layout">
      <section className="card upload-card" aria-labelledby="upload-heading" onPaste={localPicker.onPaste}>
        <div className="card-heading"><div><p className="section-kicker">{t("sourceFiles")}</p><h2 id="upload-heading">{t("addDocuments")}</h2></div>{entries.length > 0 && <span className="file-count">{entries.length}</span>}</div>
        <div className="drop-zone-shell"><div id="upload-zone" className={`drop-zone ${dragging ? "dragging" : ""}`} role="button" tabIndex={0} onClick={() => input.current?.click()} onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); input.current?.click(); } }} onDragEnter={(event) => { event.preventDefault(); setDragging(true); }} onDragOver={(event) => event.preventDefault()} onDragLeave={() => setDragging(false)} onDrop={(event) => { event.preventDefault(); setDragging(false); addFiles(Array.from(event.dataTransfer.files)); }}><span className="upload-icon" aria-hidden="true"><UploadCloud size={28} /></span><strong>{t("dropFiles")}</strong></div><button className="secondary add-file-button" type="button" disabled={false} onClick={() => input.current?.click()}><Plus size={17} aria-hidden="true" />{t("chooseFiles")}</button></div>
        <input ref={input} className="visually-hidden" type="file" multiple aria-label={t("chooseFiles")} onChange={(event) => { addFiles(Array.from(event.target.files ?? [])); event.currentTarget.value = ""; }} />
        <input ref={directory} className="visually-hidden" type="file" multiple aria-label={t("chooseFolder")} {...({ webkitdirectory: "" } as Record<string, string>)} onChange={(event) => { addFiles(Array.from(event.target.files ?? [])); event.currentTarget.value = ""; }} />
        <LocalSourceControls picker={localPicker} />
        <div className="picker-meta">
          <div className="picker-actions"><button className="text-button" type="button" disabled={false} onClick={() => directory.current?.click()}><FolderOpen size={16} aria-hidden="true" />{t("chooseFolder")}</button><span>{t("batchLimitSummary")}</span></div>
          {messageScope === "source" && message && <div className="picker-feedback" role="status" aria-live="polite"><CircleAlert size={16} aria-hidden="true" /><span>{message}</span></div>}
        </div>
        <div className={`selection ${entries.length > 0 ? "has-current" : ""} ${recentHistory.length > 0 ? "has-history" : ""}`} data-empty={entries.length === 0 && recentHistory.length === 0}>
          {entries.length > 0 && <section className="current-batch" aria-labelledby="current-batch-heading">
            <div className="selection-title"><strong id="current-batch-heading">{batchId || entries.some(entry => entry.batchId) ? t("currentBatch") : `${t("selectedFiles")} (${entries.length})`}</strong><span>{bytesLabel(selectedBytes)}</span><button className="text-button" type="button" onClick={() => { entries.forEach(entry => { if (entry.task && !TERMINAL.has(entry.task.status)) void cancel(entry.task); else if (!entry.task) runtime?.cancelUpload(entry.key); }); }}>{locale === "zh-CN" ? "取消未完成项" : "Cancel unfinished"}</button></div>
            <div className="current-batch-scroll"><ul>{entries.slice(Math.max(0, visibleCount - 100), visibleCount).map((entry, index) => {
              const format = formatForName(entry.file.name, options.format);
              const FormatIcon = iconForFormat(format);
              const percent = entry.task ? Math.round(entry.task.progressMillionths / 10_000) : 0;
              const failureCode = entry.error;
              const failed = Boolean(entry.error) || entry.task?.status === "failed" || entry.task?.status === "interrupted";
              const content = <><span className="file-type-icon"><FormatIcon size={20} aria-hidden="true" /></span><span className="selected-file-name"><strong>{entry.file.webkitRelativePath || entry.file.name}</strong><small className={failed ? "failure-reason" : undefined}>{failed ? `${t(entry.task?.status === "interrupted" ? "interrupted" : "failed")} · ${(entry.task ? taskFailureLabel(entry.task, t) : diagnosticLabel(failureCode ?? "conversionFailed", t))}` : entry.task ? `${t(entry.task.status)}${!TERMINAL.has(entry.task.status) && entry.stage ? ` · ${executionStageLabel(entry.stage, locale)}` : ""}` : entry.uploadState ? uploadLabel(entry, locale) : `${format.toUpperCase()} · ${bytesLabel(entry.originalSize ?? entry.file.size)}`}</small>{!entry.task && entry.uploadState === "uploading" && <progress max={(entry.originalSize ?? entry.file.size) || 1} value={entry.localSelection ? undefined : entry.uploaded ?? 0} aria-label={entry.file.name} />}{entry.task && !TERMINAL.has(entry.task.status) && <progress max="100" value={percent} aria-label={`${entry.file.name}: ${percent}%`} />}</span></>;
              if (entry.task && TERMINAL.has(entry.task.status)) return <li key={entry.key} className={entry.task.status}><button className="current-task-link" type="button" aria-label={`${failed ? t("failureDetails") : t("conversionResult")} ${entry.file.name}`} onClick={() => selectTask(entry.task!.id)}>{content}<span className="row-status" aria-hidden="true">{entry.task.status === "succeeded" ? <CheckCircle2 size={17} /> : <CircleAlert size={17} />}</span></button></li>;
              return <li key={entry.key} className={entry.error ? "failed" : entry.task?.status ?? "selected"}>{content}{(entry.error || entry.uploadState === "cancelled") && runtime && <button className="text-button" type="button" onClick={() => runtime.canRetryUpload(entry.key) ? runtime.retryUpload(entry.key) : entry.localSelection ? void localPicker.select("pick") : input.current?.click()}>{t("retry")}</button>}{entry.task ? <button className="icon-button" type="button" aria-label={`${t("cancel")} ${entry.file.name}`} onClick={() => void cancel(entry.task!)}><Square size={15} aria-hidden="true" /></button> : entry.batchId && !entry.error && ["waiting", "uploading", "receiving"].includes(entry.uploadState ?? "") ? <button className="icon-button" type="button" aria-label={`${t("cancel")} ${entry.file.name}`} onClick={() => runtime?.cancelUpload(entry.key)}><Square size={15} aria-hidden="true" /></button> : <button className="icon-button" type="button" aria-label={`${t("remove")} ${entry.file.name}`} onClick={() => { if (!runtime && entry.localSelection) void api.releaseLocal?.(entry.localSelection.id).catch(() => { setMessageScope("source"); setMessage(t("taskActionFailed")); }); runtime?.cancelUpload(entry.key); setEntries((current) => current.filter((_, item) => item !== Math.max(0, visibleCount - 100) + index)); }}><X size={17} aria-hidden="true" /></button>}</li>;
            })}</ul>{visibleCount > 100 && <button type="button" className="secondary" onClick={() => setVisibleCount(value => Math.max(100, value - 100))}>{locale === "zh-CN" ? "上一页任务" : "Previous tasks"}</button>}{entries.length > visibleCount && <button type="button" className="secondary" onClick={() => setVisibleCount(value => value + 100)}>{locale === "zh-CN" ? "下一页任务" : "Next tasks"}</button>}</div>
          </section>}
        </div>
      </section>
      <div className="control-column">
        <CapabilityStrip ocr={ocrStatus} capability={ocrCapability} />
        <OptionPanel value={options} onChange={setOptions} disabled={false} />
        {remoteOcrSelected && options.ocrPolicy !== "off" && <label className="check grant remote-conversion-grant"><input type="checkbox" checked={options.networkMode === "unrestricted" && options.authorizeProvider} onChange={(event) => { const allowed = event.target.checked; setOptions((current) => ({ ...current, networkMode: allowed ? "unrestricted" : "restricted", authorizeProvider: allowed })); setMessage(""); }} /><span><strong>{t("authorizeRemoteConversion")}</strong><small>{t("authorizationNote")}</small></span></label>}
        <button className="convert-button" type="button" disabled={!validResourceLimits(options) || !entries.some(entry => !entry.task && !entry.batchId)} onClick={() => void submit()}>{uploading ? <LoaderCircle className="spin" size={19} aria-hidden="true" /> : <Sparkles size={19} aria-hidden="true" />}{uploading ? t("uploading") : `${t("convert")}${pendingCount ? ` (${pendingCount})` : ""}`}</button>
        <div className={`message-bar ${messageScope === "controls" && message ? "visible" : ""}`} role="status" aria-live="polite">{messageScope === "controls" && message && <><CircleAlert size={17} aria-hidden="true" />{message}</>}</div>
      </div>
    </div><HistoryPanel {...(runtime ? { api } : {})} tasks={recentHistory} fallbackName={t("restoredTask")} onOpen={selectTask} onCleanup={() => void cleanup()} feedback={historyFeedback} /></div>
    {activeTaskId && <ResultDialog api={api} taskId={activeTaskId} onSelectTask={selectTask} onClose={closeResult} onTaskRemoved={(id) => setRecentTasks((current) => current.filter((task) => task.id !== id))} />}
  </section>;
}

function normalizeStatus(status: string): CapabilityAdmin["status"] {
  if (status === "unknown" || status === "checking" || status === "disabled") return status === "disabled" ? "blocked" : "verifying";
  return status as CapabilityAdmin["status"];
}

function uploadLabel(entry: UploadEntry, locale: string) {
  const zh = locale === "zh-CN";
  const labels = { waiting: zh ? "等待导入" : "Waiting to import", uploading: zh ? "导入中" : "Importing", receiving: zh ? "服务端接收处理中" : "Preparing received file", cancelled: zh ? "已取消" : "Cancelled", reselect: zh ? "需要重新选择文件" : "Select the file again" };
  if (entry.waitingForService) return zh ? "等待程序回应，将自动继续" : "Waiting for the app; continuing automatically";
  const label = labels[entry.uploadState ?? "waiting"];
  return entry.uploadState === "uploading" && !entry.localSelection ? `${label} · ${Math.min(100, Math.round((entry.uploaded ?? 0) / Math.max(1, entry.file.size) * 100))}%` : label;
}

function mergeFiles(entries: BatchEntry[], incoming: File[]): BatchEntry[] {
    const replacements = new Map(incoming.map(file => [entryKey(file), file]));
    const base = entries.map(entry => ["reselect", "cancelled"].includes(entry.uploadState ?? "") && replacements.has(entry.key) ? { key: entry.key, file: replacements.get(entry.key)! } : entry);
    const seen = new Set(base.map((entry) => entry.key));
    // Core authenticates the format after upload, including renamed documents.
    const unique = incoming.filter((file) => { const key = entryKey(file); if (seen.has(key)) return false; seen.add(key); return true; });
    return [...base, ...unique.map((file) => ({ key: entryKey(file), file }))];
}

function submitEntry(api: ApiClient, entry: BatchEntry, options: WorkbenchOptions, batchId: string) {
  return entry.localSelection && api.importLocal
    ? api.importLocal(entry.localSelection, options, batchId, () => {}, new AbortController().signal)
    : api.upload(entry.file, options, batchId);
}

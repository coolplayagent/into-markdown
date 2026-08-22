import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  CheckCircle2, CircleAlert, FolderOpen, LoaderCircle, Plus, Sparkles, Square, UploadCloud, X,
} from "lucide-react";
import type { ApiClient, CapabilityAdmin, ComponentStatus, TaskRecord, WorkbenchOptions } from "./api";
import { ApiError, defaultWorkbenchOptions } from "./api";
import { AdvancedSettings, CapabilityStrip, OptionPanel } from "./conversion-controls";
import { useI18n } from "./i18n";
import { ResultDialog } from "./result-page";
import { HistoryPanel } from "./history-panel";
import { useCapabilities } from "./capability-store";
import {
  SUPPORTED_FILE_ACCEPT, TERMINAL, bytesLabel, createBatchId, diagnosticLabel, formatForName, listAllTasks,
  executionStageLabel, iconForFormat, supportsFileName,
} from "./task-ui";

const MAX_BATCH_FILES = 100;
const MAX_BATCH_BYTES = 1024 * 1024 * 1024;

interface BatchEntry {
  key: string;
  file: File;
  task?: TaskRecord;
  stage?: string;
  error?: string;
}

type MessageScope = "source" | "controls";

function entryKey(file: File): string {
  return `${file.webkitRelativePath || file.name}\0${file.size}\0${file.lastModified}`;
}

export function WorkbenchPage({ api, initialTaskId }: { api: ApiClient; initialTaskId?: string | undefined }) {
  const { locale, t } = useI18n();
  const capabilities = useCapabilities();
  const input = useRef<HTMLInputElement>(null);
  const directory = useRef<HTMLInputElement>(null);
  const watchers = useRef(new Map<string, AbortController>());
  const navigatedBatch = useRef<string | null>(null);
  const [entries, setEntries] = useState<BatchEntry[]>([]);
  const [batchId, setBatchId] = useState<string | null>(null);
  const [options, setOptions] = useState<WorkbenchOptions>(defaultWorkbenchOptions);
  const [uploading, setUploading] = useState(false);
  const [message, setMessage] = useState("");
  const [messageScope, setMessageScope] = useState<MessageScope>("source");
  const [dragging, setDragging] = useState(false);
  const [advanced, setAdvanced] = useState(false);
  const [recentTasks, setRecentTasks] = useState<TaskRecord[]>([]);
  const [activeTaskId, setActiveTaskId] = useState<string | undefined>(initialTaskId);
  const quickOcr = capabilities.capability("ocr");
  const ocrStatus: ComponentStatus | undefined = quickOcr ? { available: quickOcr.status === "ready", code: quickOcr.status, detail: quickOcr.currentSourceName } : undefined;
  const ocrCapability: CapabilityAdmin | undefined = quickOcr ? { id: "ocr", status: normalizeStatus(quickOcr.status), localStatus: normalizeStatus(quickOcr.localStatus), currentSource: quickOcr.currentSource, sources: quickOcr.sources, ...(quickOcr.version ? { version: quickOcr.version } : {}), ...(quickOcr.localVersion ? { localVersion: quickOcr.localVersion } : {}) } : undefined;

  useEffect(() => setActiveTaskId(initialTaskId), [initialTaskId]);

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

  const batchFinished = entries.length > 0 && entries.every((entry) => entry.error || entry.task && TERMINAL.has(entry.task.status));

  useEffect(() => {
    const controller = new AbortController();
    void listAllTasks(api, controller.signal)
      .then((tasks) => { setRecentTasks(tasks.filter((task) => task.workflow === "conversion")); })
      .catch((error: unknown) => {
        if (!(error instanceof DOMException && error.name === "AbortError")) {
          setMessageScope("source");
          setMessage(t("loadTasksError"));
        }
      });
    return () => controller.abort();
  }, [api, batchFinished, t]);

  const installOcr = useCallback(async () => {
    await api.installCapability("ocr");
    await capabilities.refresh();
  }, [api, capabilities]);

  useEffect(() => {
    if (!batchId || uploading || navigatedBatch.current === batchId || entries.length === 0) return;
    const finished = entries.every((entry) => entry.error || entry.task && TERMINAL.has(entry.task.status));
    if (!finished) return;
    const first = entries.find((entry) => entry.task?.status === "succeeded")?.task;
    if (first) {
      navigatedBatch.current = batchId;
      setActiveTaskId(first.id);
    }
  }, [batchId, entries, uploading]);

  const selectedBytes = useMemo(() => entries.reduce((sum, entry) => sum + entry.file.size, 0), [entries]);
  const remoteOcrSelected = ocrCapability?.currentSource.startsWith("provider:") === true;
  const recentHistory = useMemo(() => {
    const currentIds = new Set(entries.flatMap((entry) => entry.task ? [entry.task.id] : []));
    return recentTasks.filter((task) => TERMINAL.has(task.status) && !currentIds.has(task.id));
  }, [entries, recentTasks]);

  const addFiles = (incoming: File[]) => {
    const base = batchFinished ? [] : entries;
    const seen = new Set(base.map((entry) => entry.key));
    const unsupported = incoming.filter((file) => !supportsFileName(file.name, options.format));
    const supported = incoming.filter((file) => supportsFileName(file.name, options.format));
    const unique = supported.filter((file) => { const key = entryKey(file); if (seen.has(key)) return false; seen.add(key); return true; });
    const combined = [...base, ...unique.map((file) => ({ key: entryKey(file), file }))];
    const total = combined.reduce((sum, entry) => sum + entry.file.size, 0);
    const skipped = unsupported.slice(0, 3).map((file) => file.name).join("、")
      + (unsupported.length > 3 ? ` +${unsupported.length - 3}` : "");
    const unsupportedMessage = unsupported.length > 0 ? t("unsupportedFiles").replace("{files}", skipped) : "";
    setMessageScope("source");
    if (combined.length > MAX_BATCH_FILES) setMessage(t("tooManyFiles"));
    else if (combined.some((entry) => entry.file.size > options.maxInputMiB * 1024 * 1024)) setMessage(t("fileTooLarge"));
    else if (total > MAX_BATCH_BYTES) setMessage(t("batchTooLarge"));
    else {
      if (batchFinished) { setBatchId(null); navigatedBatch.current = null; }
      setEntries(combined);
      setMessage(unsupportedMessage);
    }
  };

  const submit = async () => {
    if (!entries.length || uploading || entries.some((entry) => entry.task)) return;
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
    setUploading(true);
    setMessageScope("source");
    setMessage("");
    for (const entry of entries) {
      try {
        const task = await api.upload(entry.file, options, nextBatchId);
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
    try {
      const result = await api.cleanup();
      const tasks = await listAllTasks(api);
      setRecentTasks(tasks.filter((task) => task.workflow === "conversion"));
      setMessageScope("source");
      setMessage(t("cleanupResult").replace("{tasks}", String(result.deletedTasks)).replace("{bytes}", (result.reclaimedBytes / 1048576).toFixed(1)));
    } catch {
      setMessageScope("source"); setMessage(t("loadTasksError"));
    }
  };

  return <section className="workbench-route" aria-labelledby="workbench-title">
    <div className="page-heading compact-heading"><div><p className="eyebrow">DOCUMENT TO MARKDOWN</p><h1 id="workbench-title">{t("convertDocuments")}</h1></div></div>
    <div className="task-workspace"><div className="conversion-layout">
      <section className="card upload-card" aria-labelledby="upload-heading">
        <div className="card-heading"><div><p className="section-kicker">{t("sourceFiles")}</p><h2 id="upload-heading">{t("addDocuments")}</h2></div>{entries.length > 0 && <span className="file-count">{entries.length}</span>}</div>
        <div className="drop-zone-shell"><div id="upload-zone" className={`drop-zone ${dragging ? "dragging" : ""}`} role="button" tabIndex={0} onClick={() => input.current?.click()} onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); input.current?.click(); } }} onDragEnter={(event) => { event.preventDefault(); setDragging(true); }} onDragOver={(event) => event.preventDefault()} onDragLeave={() => setDragging(false)} onDrop={(event) => { event.preventDefault(); setDragging(false); addFiles(Array.from(event.dataTransfer.files)); }}><span className="upload-icon" aria-hidden="true"><UploadCloud size={28} /></span><strong>{t("dropFiles")}</strong></div><button className="secondary add-file-button" type="button" disabled={uploading} onClick={() => input.current?.click()}><Plus size={17} aria-hidden="true" />{t("chooseFiles")}</button></div>
        <input ref={input} className="visually-hidden" type="file" multiple accept={options.format ? undefined : SUPPORTED_FILE_ACCEPT} aria-label={t("chooseFiles")} onChange={(event) => { addFiles(Array.from(event.target.files ?? [])); event.currentTarget.value = ""; }} />
        <input ref={directory} className="visually-hidden" type="file" multiple accept={options.format ? undefined : SUPPORTED_FILE_ACCEPT} aria-label={t("chooseFolder")} {...({ webkitdirectory: "" } as Record<string, string>)} onChange={(event) => { addFiles(Array.from(event.target.files ?? [])); event.currentTarget.value = ""; }} />
        <div className="picker-meta">
          <div className="picker-actions"><button className="text-button" type="button" disabled={uploading} onClick={() => directory.current?.click()}><FolderOpen size={16} aria-hidden="true" />{t("chooseFolder")}</button><span>{t("batchLimitSummary")}</span></div>
          {messageScope === "source" && message && <div className="picker-feedback" role="status" aria-live="polite"><CircleAlert size={16} aria-hidden="true" /><span>{message}</span></div>}
        </div>
        <div className={`selection ${entries.length > 0 ? "has-current" : ""} ${recentHistory.length > 0 ? "has-history" : ""}`} data-empty={entries.length === 0 && recentHistory.length === 0}>
          {entries.length > 0 && <section className="current-batch" aria-labelledby="current-batch-heading">
            <div className="selection-title"><strong id="current-batch-heading">{batchId ? t("currentBatch") : `${t("selectedFiles")} (${entries.length})`}</strong><span>{bytesLabel(selectedBytes)}</span></div>
            <div className="current-batch-scroll"><ul>{entries.map((entry, index) => {
              const format = formatForName(entry.file.name, options.format);
              const FormatIcon = iconForFormat(format);
              const percent = entry.task ? Math.round(entry.task.progressMillionths / 10_000) : 0;
              const failureCode = entry.error ?? entry.task?.diagnostics[0]?.code;
              const failed = Boolean(entry.error) || entry.task?.status === "failed" || entry.task?.status === "interrupted";
              const content = <><span className="file-type-icon"><FormatIcon size={20} aria-hidden="true" /></span><span className="selected-file-name"><strong>{entry.file.webkitRelativePath || entry.file.name}</strong><small className={failed ? "failure-reason" : undefined}>{failed ? `${t(entry.task?.status === "interrupted" ? "interrupted" : "failed")} · ${diagnosticLabel(failureCode ?? "conversionFailed", t)}` : entry.task ? `${t(entry.task.status)}${!TERMINAL.has(entry.task.status) && entry.stage ? ` · ${executionStageLabel(entry.stage, locale)}` : ""}` : `${format.toUpperCase()} · ${bytesLabel(entry.file.size)}`}</small>{entry.task && !TERMINAL.has(entry.task.status) && <progress max="100" value={percent} aria-label={`${entry.file.name}: ${percent}%`} />}</span></>;
              if (entry.task && TERMINAL.has(entry.task.status)) return <li key={entry.key} className={entry.task.status}><button className="current-task-link" type="button" aria-label={`${failed ? t("failureDetails") : t("conversionResult")} ${entry.file.name}`} onClick={() => setActiveTaskId(entry.task!.id)}>{content}<span className="row-status" aria-hidden="true">{entry.task.status === "succeeded" ? <CheckCircle2 size={17} /> : <CircleAlert size={17} />}</span></button></li>;
              return <li key={entry.key} className={entry.error ? "failed" : entry.task?.status ?? "selected"}>{content}{entry.task ? <button className="icon-button" type="button" aria-label={`${t("cancel")} ${entry.file.name}`} onClick={() => void cancel(entry.task!)}><Square size={15} aria-hidden="true" /></button> : <button className="icon-button" type="button" aria-label={`${t("remove")} ${entry.file.name}`} onClick={() => setEntries((current) => current.filter((_, item) => item !== index))}><X size={17} aria-hidden="true" /></button>}</li>;
            })}</ul></div>
          </section>}
        </div>
      </section>
      <div className="control-column">
        <CapabilityStrip ocr={ocrStatus} capability={ocrCapability} onInstallOcr={installOcr} />
        <OptionPanel value={options} onChange={setOptions} disabled={uploading || entries.some((entry) => Boolean(entry.task))} onOpenAdvanced={() => setAdvanced(true)} />
        <button className="convert-button" type="button" disabled={entries.length === 0 || uploading || entries.some((entry) => Boolean(entry.task))} onClick={() => void submit()}>{uploading ? <LoaderCircle className="spin" size={19} aria-hidden="true" /> : <Sparkles size={19} aria-hidden="true" />}{uploading ? t("uploading") : `${t("convert")}${entries.length ? ` (${entries.length})` : ""}`}</button>
        <div className={`message-bar ${messageScope === "controls" && message ? "visible" : ""}`} role="status" aria-live="polite">{messageScope === "controls" && message && <><CircleAlert size={17} aria-hidden="true" />{message}</>}</div>
      </div>
    </div><HistoryPanel tasks={recentHistory} fallbackName={t("restoredTask")} onOpen={setActiveTaskId} onCleanup={() => void cleanup()} /></div>
    <AdvancedSettings value={options} onChange={setOptions} open={advanced} onClose={() => setAdvanced(false)} providerCapabilityActive={remoteOcrSelected && options.ocrPolicy !== "off"} />
    {activeTaskId && <ResultDialog api={api} taskId={activeTaskId} onSelectTask={setActiveTaskId} onClose={() => setActiveTaskId(undefined)} onTaskRemoved={(id) => setRecentTasks((current) => current.filter((task) => task.id !== id))} />}
  </section>;
}

function normalizeStatus(status: string): CapabilityAdmin["status"] {
  if (status === "unknown" || status === "checking" || status === "disabled") return status === "disabled" ? "blocked" : "verifying";
  return status as CapabilityAdmin["status"];
}

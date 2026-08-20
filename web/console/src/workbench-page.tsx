import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  CheckCircle2, CircleAlert, FolderOpen, LoaderCircle, Plus, Sparkles, Square, UploadCloud, X,
} from "lucide-react";
import type { ApiClient, TaskRecord, WorkbenchOptions } from "./api";
import { ApiError, defaultWorkbenchOptions } from "./api";
import { AdvancedSettings, CapabilityStrip, OptionPanel } from "./conversion-controls";
import { useI18n } from "./i18n";
import { useRouter } from "./router";
import {
  TERMINAL, bytesLabel, createBatchId, formatForName, iconForFormat,
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

function entryKey(file: File): string {
  return `${file.webkitRelativePath || file.name}\0${file.size}\0${file.lastModified}`;
}

export function WorkbenchPage({ api }: { api: ApiClient }) {
  const { t } = useI18n();
  const { path, navigate } = useRouter();
  const input = useRef<HTMLInputElement>(null);
  const directory = useRef<HTMLInputElement>(null);
  const watchers = useRef(new Map<string, AbortController>());
  const navigatedBatch = useRef<string | null>(null);
  const [entries, setEntries] = useState<BatchEntry[]>([]);
  const [batchId, setBatchId] = useState<string | null>(null);
  const [options, setOptions] = useState<WorkbenchOptions>(defaultWorkbenchOptions);
  const [uploading, setUploading] = useState(false);
  const [message, setMessage] = useState("");
  const [dragging, setDragging] = useState(false);
  const [advanced, setAdvanced] = useState(false);

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
      if (!controller.signal.aborted) setMessage(t("streamError"));
    }).finally(() => watchers.current.delete(task.id));
  }, [api, t, updateTask]);

  useEffect(() => () => {
    watchers.current.forEach((watcher) => watcher.abort());
    watchers.current.clear();
  }, []);

  useEffect(() => {
    const workbenchVisible = path === "/" || path === "/workbench";
    if (!batchId || uploading || !workbenchVisible || navigatedBatch.current === batchId || entries.length === 0) return;
    const finished = entries.every((entry) => entry.error || entry.task && TERMINAL.has(entry.task.status));
    if (!finished) return;
    const first = entries.find((entry) => entry.task?.status === "succeeded")?.task;
    if (first) {
      navigatedBatch.current = batchId;
      navigate(`/results/${first.id}`);
    }
  }, [batchId, entries, navigate, path, uploading]);

  const batchFinished = entries.length > 0 && entries.every((entry) => entry.error || entry.task && TERMINAL.has(entry.task.status));
  const selectedBytes = useMemo(() => entries.reduce((sum, entry) => sum + entry.file.size, 0), [entries]);

  const addFiles = (incoming: File[]) => {
    const base = batchFinished ? [] : entries;
    const seen = new Set(base.map((entry) => entry.key));
    const unique = incoming.filter((file) => { const key = entryKey(file); if (seen.has(key)) return false; seen.add(key); return true; });
    const combined = [...base, ...unique.map((file) => ({ key: entryKey(file), file }))];
    const total = combined.reduce((sum, entry) => sum + entry.file.size, 0);
    if (combined.length > MAX_BATCH_FILES) setMessage(t("tooManyFiles"));
    else if (combined.some((entry) => entry.file.size > options.maxInputMiB * 1024 * 1024)) setMessage(t("fileTooLarge"));
    else if (total > MAX_BATCH_BYTES) setMessage(t("batchTooLarge"));
    else {
      if (batchFinished) { setBatchId(null); navigatedBatch.current = null; }
      setEntries(combined);
      setMessage("");
    }
  };

  const submit = async () => {
    if (!entries.length || uploading || entries.some((entry) => entry.task)) return;
    if (options.aiMode !== "off" && !options.authorizeProvider) { setMessage(t("authorizationRequired")); return; }
    const nextBatchId = createBatchId();
    setBatchId(nextBatchId);
    navigatedBatch.current = null;
    setUploading(true);
    setMessage("");
    for (const entry of entries) {
      try {
        const task = await api.upload(entry.file, options, nextBatchId);
        setEntries((current) => current.map((item) => item.key === entry.key ? { ...item, task, stage: task.status } : item));
        watch(entry.key, task);
      } catch (error) {
        const code = error instanceof ApiError ? error.code : "unreachable";
        setEntries((current) => current.map((item) => item.key === entry.key ? { ...item, error: code } : item));
        setMessage(`${t("uploadFailed")}: ${entry.file.name} (${code})`);
      }
    }
    setUploading(false);
  };

  const cancel = async (task: TaskRecord) => {
    try { const updated = await api.cancel(task.id); updateTask(task.id, () => updated); }
    catch { setMessage(t("streamError")); }
  };

  return <section className="workbench-route" aria-labelledby="workbench-title">
    <div className="page-heading compact-heading"><div><p className="eyebrow">LOCAL WORKBENCH</p><h1 id="workbench-title">{t("convertDocuments")}</h1></div><p>{t("convertDocumentsIntro")}</p></div>
    <div className="conversion-layout">
      <section className="card upload-card" aria-labelledby="upload-heading">
        <div className="card-heading"><div><p className="section-kicker">{t("sourceFiles")}</p><h2 id="upload-heading">{t("addDocuments")}</h2></div>{entries.length > 0 && <span className="file-count">{entries.length}</span>}</div>
        <div className="drop-zone-shell"><div id="upload-zone" className={`drop-zone ${dragging ? "dragging" : ""}`} role="button" tabIndex={0} onClick={() => input.current?.click()} onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); input.current?.click(); } }} onDragEnter={(event) => { event.preventDefault(); setDragging(true); }} onDragOver={(event) => event.preventDefault()} onDragLeave={() => setDragging(false)} onDrop={(event) => { event.preventDefault(); setDragging(false); addFiles(Array.from(event.dataTransfer.files)); }}><span className="upload-icon" aria-hidden="true"><UploadCloud size={28} /></span><strong>{t("dropFiles")}</strong></div><button className="secondary add-file-button" type="button" disabled={uploading} onClick={() => input.current?.click()}><Plus size={17} aria-hidden="true" />{t("chooseFiles")}</button></div>
        <input ref={input} className="visually-hidden" type="file" multiple aria-label={t("chooseFiles")} onChange={(event) => addFiles(Array.from(event.target.files ?? []))} />
        <input ref={directory} className="visually-hidden" type="file" multiple aria-label={t("chooseFolder")} {...({ webkitdirectory: "" } as Record<string, string>)} onChange={(event) => addFiles(Array.from(event.target.files ?? []))} />
        <div className="picker-actions"><button className="text-button" type="button" disabled={uploading} onClick={() => directory.current?.click()}><FolderOpen size={16} aria-hidden="true" />{t("chooseFolder")}</button><span>{t("batchLimitSummary")}</span></div>
        <div className="selection current-batch" data-empty={entries.length === 0}>
          {entries.length > 0 && <div className="selection-title"><strong>{batchId ? t("currentBatch") : `${t("selectedFiles")} (${entries.length})`}</strong><span>{bytesLabel(selectedBytes)}</span></div>}
          {entries.length > 0 && <ul>{entries.map((entry, index) => { const format = formatForName(entry.file.name, options.format); const FormatIcon = iconForFormat(format); const percent = entry.task ? Math.round(entry.task.progressMillionths / 10_000) : 0; return <li key={entry.key} className={entry.error ? "failed" : entry.task?.status ?? "selected"}><span className="file-type-icon"><FormatIcon size={20} aria-hidden="true" /></span><span className="selected-file-name"><strong>{entry.file.webkitRelativePath || entry.file.name}</strong><small>{entry.error ? `${t("failed")} · ${entry.error}` : entry.task ? `${t(entry.task.status)}${entry.stage ? ` · ${entry.stage}` : ""}` : `${format.toUpperCase()} · ${bytesLabel(entry.file.size)}`}</small>{entry.task && !TERMINAL.has(entry.task.status) && <progress max="100" value={percent} aria-label={`${entry.file.name}: ${percent}%`} />}</span>{entry.task && !TERMINAL.has(entry.task.status) ? <button className="icon-button" type="button" aria-label={`${t("cancel")} ${entry.file.name}`} onClick={() => void cancel(entry.task!)}><Square size={15} aria-hidden="true" /></button> : !entry.task ? <button className="icon-button" type="button" aria-label={`${t("remove")} ${entry.file.name}`} onClick={() => setEntries((current) => current.filter((_, item) => item !== index))}><X size={17} aria-hidden="true" /></button> : <span className="row-status" aria-hidden="true">{entry.task.status === "succeeded" ? <CheckCircle2 size={17} /> : <CircleAlert size={17} />}</span>}</li>; })}</ul>}
        </div>
      </section>
      <div className="control-column">
        <CapabilityStrip value={options} onChange={setOptions} />
        <OptionPanel value={options} onChange={setOptions} disabled={uploading || entries.some((entry) => Boolean(entry.task))} onOpenAdvanced={() => setAdvanced(true)} />
        <button className="convert-button" type="button" disabled={entries.length === 0 || uploading || entries.some((entry) => Boolean(entry.task))} onClick={() => void submit()}>{uploading ? <LoaderCircle className="spin" size={19} aria-hidden="true" /> : <Sparkles size={19} aria-hidden="true" />}{uploading ? t("uploading") : `${t("convert")}${entries.length ? ` (${entries.length})` : ""}`}</button>
        <div className={`message-bar ${message ? "visible" : ""}`} role="status" aria-live="polite">{message && <><CircleAlert size={17} aria-hidden="true" />{message}</>}</div>
      </div>
    </div>
    <AdvancedSettings value={options} onChange={setOptions} open={advanced} onClose={() => setAdvanced(false)} />
  </section>;
}

import { useCallback, useEffect, useMemo, useState } from "react";
import { createPortal } from "react-dom";
import {
  Braces, CheckCircle2, ChevronDown, CircleAlert, Code2, Download, Eye, FileJson, Info,
  LoaderCircle, MoreHorizontal, Package, Pin, PinOff, RotateCcw, Save, Trash2, Users, X,
} from "lucide-react";
import type { ApiClient, ArtifactPreview, SpeakerLabels, TaskRecord } from "./api";
import { DismissibleMenu } from "./dismissible-menu";
import { SafeMarkdownPreview } from "./preview";
import { useI18n } from "./i18n";
import { useDialogLifecycle } from "./dialog-lifecycle";
import {
  TERMINAL, artifactLabel, bytesLabel, diagnosticLabel, taskFailureCode, downloadArtifact, iconForFormat, taskFormat,
  taskName,
} from "./task-ui";

export function ResultDialog({ api, taskId, onSelectTask, onClose, onTaskRemoved, onTaskUpdated }: {
  api: ApiClient;
  taskId: string;
  onSelectTask(taskId: string): void;
  onClose(): void;
  onTaskRemoved(taskId: string): void;
  onTaskUpdated?(task: TaskRecord): void;
}) {
  const { t } = useI18n();
  const dialogRef = useDialogLifecycle<HTMLElement>(true, onClose, (dialog) => !dialog.querySelector('[role="menu"]'));
  const [task, setTask] = useState<TaskRecord | null>(null);
  const [batch, setBatch] = useState<TaskRecord[]>([]);
  const [preview, setPreview] = useState<ArtifactPreview | null>(null);
  const [previewError, setPreviewError] = useState(false);
  const [mode, setMode] = useState<"rendered" | "source">("rendered");
  const [drawer, setDrawer] = useState(false);
  const [loading, setLoading] = useState(true);
  const [speakerLabels, setSpeakerLabels] = useState<SpeakerLabels | null>(null);
  const [speakerEdits, setSpeakerEdits] = useState<Record<string, string>>({});
  const [speakerMessage, setSpeakerMessage] = useState("");
  const [savingSpeakers, setSavingSpeakers] = useState(false);

  const load = useCallback(async (signal?: AbortSignal) => {
    setLoading(true); setPreview(null); setPreviewError(false); setSpeakerLabels(null); setSpeakerEdits({}); setSpeakerMessage("");
    const current = await api.getTask(taskId, signal);
    setTask(current); onTaskUpdated?.(current);
    setDrawer(false);
    if (current.batchId) {
      const page = await api.listTasks({ limit: 100, batchId: current.batchId }, signal);
      setBatch([...page.tasks].sort((left, right) => left.createdAtMs - right.createdAtMs));
    } else setBatch([current]);
    const markdown = current.artifacts.find((artifact) => artifact.kind === "markdown");
    if (markdown) {
      try { setPreview(await api.preview(current.id, markdown.storageKey, signal)); }
      catch { if (!signal?.aborted) setPreviewError(true); }
    }
    if (current.workflow === "meetingTranscript" && current.status === "succeeded") {
      try {
        const labels = await api.speakerLabels(current.id, signal);
        setSpeakerLabels(labels);
        setSpeakerEdits(Object.fromEntries(labels.speakers.map((speaker) => [speaker.id, speaker.name])));
      } catch { if (!signal?.aborted) setSpeakerMessage(t("speakerLabelsLoadFailed")); }
    }
    setLoading(false);
  }, [api, onTaskUpdated, taskId, t]);

  useEffect(() => {
    const controller = new AbortController();
    void load(controller.signal).catch(() => { if (!controller.signal.aborted) { setPreviewError(true); setLoading(false); } });
    return () => controller.abort();
  }, [load]);

  const watchedTaskId = task?.id;
  const watchedTaskActive = task ? !TERMINAL.has(task.status) : false;
  useEffect(() => {
    if (!watchedTaskId || !watchedTaskActive) return;
    const controller = new AbortController();
    void api.watchTask(watchedTaskId, (event) => {
      setTask((current) => current ? { ...current, status: event.status, progressMillionths: event.progressMillionths, updatedAtMs: Date.now() } : current);
      if (event.terminal) void load();
    }, controller.signal);
    return () => controller.abort();
  }, [api, load, watchedTaskActive, watchedTaskId]);

  const markdown = task?.artifacts.find((artifact) => artifact.kind === "markdown");
  const title = task ? taskName(task, `${t("restoredTask")} ${task.id.slice(0, 8)}`) : t("loadingPreview");
  const FormatIcon = iconForFormat(task ? taskFormat(task) : "auto");
  const assets = useMemo(() => task?.artifacts.filter((artifact) => artifact.kind === "asset") ?? [], [task]);
  const failureMessage = task && (task.status === "failed" || task.status === "interrupted")
    ? diagnosticLabel(taskFailureCode(task), t)
    : null;

  const pin = async () => { if (task) setTask(await api.setPinned(task.id, !task.pinned)); };
  const retry = async () => {
    if (!task) return;
    const next = await api.retry(task.id);
    setTask(next); setPreview(null); setPreviewError(false); setDrawer(false);
    setSpeakerLabels(null); setSpeakerEdits({}); setSpeakerMessage("");
    onTaskUpdated?.(next); onSelectTask(next.id);
  };
  const remove = async () => { if (!task || !window.confirm(t("deleteWarning"))) return; await api.deleteTask(task.id); onTaskRemoved(task.id); onClose(); };
  const saveSpeakers = async () => {
    if (!task || !speakerLabels) return;
    const changed = Object.fromEntries(speakerLabels.speakers
      .filter((speaker) => speakerEdits[speaker.id] !== speaker.name)
      .map((speaker) => [speaker.id, speakerEdits[speaker.id]?.trim() ?? ""]));
    if (Object.keys(changed).length === 0) return;
    if (Object.values(changed).some((value) => value.length === 0 || value.length > 80
      || /[\u0000-\u001f\u007f]/.test(value))) {
      setSpeakerMessage(t("invalidSpeakerName")); return;
    }
    setSavingSpeakers(true); setSpeakerMessage("");
    try {
      await api.relabelSpeakers(task.id, speakerLabels.artifactGeneration, changed);
      await load(); setSpeakerMessage(t("speakerNamesSaved"));
    } catch { setSpeakerMessage(t("speakerNamesSaveFailed")); }
    finally { setSavingSpeakers(false); }
  };
  const speakersChanged = speakerLabels?.speakers.some((speaker) => speakerEdits[speaker.id] !== speaker.name) ?? false;

  return createPortal(<div className="result-dialog-backdrop" role="presentation" onMouseDown={(event) => { if (event.currentTarget === event.target) onClose(); }}>
    <section ref={dialogRef} className="result-dialog" role="dialog" aria-modal="true" aria-labelledby="result-title">
    <div className="result-header">
      <div className="result-toolbar-main">
        <div className="result-identity">
          <span className="file-type-icon"><FormatIcon size={20} aria-hidden="true" /></span>
          <div><div className="result-title-meta"><span>{t("conversionResult")}</span>{task && <span className={`result-status ${task.status}`}>{task.status === "succeeded" ? <CheckCircle2 size={14} aria-hidden="true" /> : <CircleAlert size={14} aria-hidden="true" />}{t(task.status)}</span>}</div><h1 id="result-title">{title}</h1></div>
        </div>
        <button className="icon-button neutral result-close" type="button" aria-label={t("close")} onClick={onClose}><X size={20} aria-hidden="true" /></button>
      </div>
      <div className="result-toolbar-secondary">
        <div className="view-toggle" role="group" aria-label={t("previewMode")}><button type="button" aria-pressed={mode === "rendered"} onClick={() => setMode("rendered")}><Eye size={16} aria-hidden="true" />{t("renderedPreview")}</button><button type="button" aria-pressed={mode === "source"} onClick={() => setMode("source")}><Code2 size={16} aria-hidden="true" />{t("markdownSource")}</button></div>
        <div className="result-actions">
          {task && markdown && <button className="secondary" type="button" onClick={() => void downloadArtifact(api, task, markdown.storageKey)}><Download size={16} aria-hidden="true" />{t("downloadMarkdown")}</button>}
          <button className="secondary" type="button" aria-expanded={drawer} onClick={() => setDrawer((value) => !value)}><Info size={16} aria-hidden="true" />{t("detailsAndResources")}</button>
          {task && <DismissibleMenu key={task.id} label={t("moreActions")} trigger={<MoreHorizontal size={19} aria-hidden="true" />}><button role="menuitem" className="menu-action" type="button" onClick={() => void pin()}>{task.pinned ? <PinOff size={16} aria-hidden="true" /> : <Pin size={16} aria-hidden="true" />}{t(task.pinned ? "unpin" : "pin")}</button><button role="menuitem" className="menu-action" type="button" onClick={() => void retry()}><RotateCcw size={16} aria-hidden="true" />{t("retry")}</button><button role="menuitem" className="menu-action danger" type="button" onClick={() => void remove()}><Trash2 size={16} aria-hidden="true" />{t("deleteTask")}</button></DismissibleMenu>}
        </div>
      </div>
      {batch.length > 1 && <nav className="batch-switcher" aria-label={t("batchResults")}>
        {batch.slice(0, 6).map((item) => <button key={item.id} type="button" aria-current={item.id === taskId ? "page" : undefined} onClick={() => onSelectTask(item.id)}>{taskName(item, item.id.slice(0, 8))}</button>)}
        {batch.length > 6 && <label className="select-shell batch-select"><span className="visually-hidden">{t("moreBatchResults")}</span><select value={taskId} onChange={(event) => onSelectTask(event.target.value)}>{batch.map((item) => <option key={item.id} value={item.id}>{taskName(item, item.id.slice(0, 8))}</option>)}</select><ChevronDown size={15} aria-hidden="true" /></label>}
      </nav>}
    </div>

    <div className={`result-body ${drawer ? "drawer-open" : ""}`}>
      <div className="result-document-scroll" tabIndex={-1} role="document">
        {task?.workflow === "meetingTranscript" && speakerLabels && speakerLabels.speakers.length > 0 && <section className="speaker-editor" aria-labelledby="speaker-editor-title">
          <div><Users size={18} aria-hidden="true" /><div><strong id="speaker-editor-title">{t("speakerNames")}</strong><small>{t("speakerNamesHint")}</small></div></div>
          <div className="speaker-editor-fields">{speakerLabels.speakers.map((speaker) => <label key={speaker.id}><span>{speaker.id.replace("speaker-", "Speaker ")}</span><input value={speakerEdits[speaker.id] ?? ""} maxLength={80} onInput={(event) => { const value = event.currentTarget.value; setSpeakerEdits((current) => ({ ...current, [speaker.id]: value })); }} /></label>)}</div>
          <button className="secondary" type="button" disabled={!speakersChanged || savingSpeakers} onClick={() => void saveSpeakers()}>{savingSpeakers ? <LoaderCircle className="spin" size={16} /> : <Save size={16} />}{t("saveSpeakerNames")}</button>
          {speakerMessage && <p className="speaker-editor-message" role="status">{speakerMessage}</p>}
        </section>}
        {task?.workflow === "meetingTranscript" && speakerMessage && !speakerLabels && <p className="speaker-editor-message standalone" role="status">{speakerMessage}</p>}
        <article className="document-canvas">
          {loading ? <div className="preview-loading" role="status"><LoaderCircle className="spin" size={22} aria-hidden="true" />{t("loadingPreview")}</div> : previewError ? <div className="result-empty" role="alert"><CircleAlert size={25} aria-hidden="true" /><h2>{t("previewFailed")}</h2></div> : !preview ? <div className="result-empty" role={failureMessage ? "alert" : undefined}>{failureMessage ? <CircleAlert size={25} aria-hidden="true" /> : <Code2 size={25} aria-hidden="true" />}<h2>{failureMessage ?? t("noMarkdownResult")}</h2>{failureMessage && <button type="button" onClick={() => void retry()}><RotateCcw size={16} aria-hidden="true" />{t("retry")}</button>}</div> : <>{preview.truncated && <p className="preview-notice" role="status">{t("previewTruncated")}</p>}{mode === "rendered" ? <SafeMarkdownPreview source={preview.text} /> : <pre className="markdown-source"><code>{preview.text}</code></pre>}</>}
        </article>
      </div>
      {drawer && task && <aside className="result-drawer" aria-label={t("detailsAndResources")}><div className="drawer-heading"><h2>{t("detailsAndResources")}</h2><button className="icon-button neutral" type="button" aria-label={t("close")} onClick={() => setDrawer(false)}><X size={18} aria-hidden="true" /></button></div><section><h3>{t("taskDetails")}</h3><dl><div><dt>ID</dt><dd><code>{task.id}</code></dd></div><div><dt>{t("created")}</dt><dd>{new Date(task.createdAtMs).toLocaleString()}</dd></div><div><dt>{t("updated")}</dt><dd>{new Date(task.updatedAtMs).toLocaleString()}</dd></div><div><dt>OCR</dt><dd>{task.configuration.ocrEnabled ? t("on") : t("off")}</dd></div></dl></section><section><h3>{t("resources")} ({assets.length})</h3>{assets.length === 0 ? <p className="muted">{t("noResources")}</p> : <ul className="drawer-list">{assets.map((artifact) => <li key={artifact.storageKey}><div><strong>{artifactLabel(artifact)}</strong><small>{artifact.mediaType ?? "application/octet-stream"} · {bytesLabel(artifact.byteLen)}</small></div><button className="icon-button" type="button" aria-label={`${t("download")} ${artifactLabel(artifact)}`} onClick={() => void downloadArtifact(api, task, artifact.storageKey)}><Download size={16} aria-hidden="true" /></button></li>)}</ul>}</section><section><h3>{t("diagnostics")}</h3>{task.diagnostics.length === 0 ? <p className="muted">{t("noDiagnostics")}</p> : <ul className="diagnostic-list">{task.diagnostics.map((item, index) => <li key={`${item.code}-${index}`}>{diagnosticLabel(item.code, t)}</li>)}</ul>}</section><section><h3>{t("otherArtifacts")}</h3><div className="artifact-actions">{task.artifacts.filter((artifact) => artifact.kind !== "markdown" && artifact.kind !== "asset").map((artifact) => <button className="secondary" type="button" key={artifact.storageKey} onClick={() => void downloadArtifact(api, task, artifact.storageKey)}>{artifact.kind === "bundle" ? <Package size={16} aria-hidden="true" /> : artifact.kind === "documentIr" ? <Braces size={16} aria-hidden="true" /> : <FileJson size={16} aria-hidden="true" />}{artifactLabel(artifact)}</button>)}</div></section></aside>}
    </div>
  </section>
  </div>, document.body);
}

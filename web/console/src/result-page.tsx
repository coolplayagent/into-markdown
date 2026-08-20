import { useCallback, useEffect, useMemo, useState } from "react";
import {
  ArrowLeft, Braces, CheckCircle2, CircleAlert, Code2, Download, Eye, FileJson, Info,
  LoaderCircle, MoreHorizontal, Package, Pin, PinOff, RotateCcw, Trash2, X,
} from "lucide-react";
import type { ApiClient, ArtifactPreview, TaskRecord } from "./api";
import { SafeMarkdownPreview } from "./preview";
import { RouteLink, useRouter } from "./router";
import { useI18n } from "./i18n";
import {
  TERMINAL, artifactLabel, bytesLabel, downloadArtifact, iconForFormat, taskFormat, taskName,
} from "./task-ui";

export function ResultPage({ api, taskId }: { api: ApiClient; taskId: string }) {
  const { t } = useI18n();
  const { navigate } = useRouter();
  const [task, setTask] = useState<TaskRecord | null>(null);
  const [batch, setBatch] = useState<TaskRecord[]>([]);
  const [preview, setPreview] = useState<ArtifactPreview | null>(null);
  const [previewError, setPreviewError] = useState(false);
  const [mode, setMode] = useState<"rendered" | "source">("rendered");
  const [drawer, setDrawer] = useState(false);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async (signal?: AbortSignal) => {
    setLoading(true); setPreview(null); setPreviewError(false);
    const current = await api.getTask(taskId, signal);
    setTask(current);
    if (current.batchId) {
      const page = await api.listTasks({ limit: 100, batchId: current.batchId }, signal);
      setBatch([...page.tasks].sort((left, right) => left.createdAtMs - right.createdAtMs));
    } else setBatch([current]);
    const markdown = current.artifacts.find((artifact) => artifact.kind === "markdown");
    if (markdown) {
      try { setPreview(await api.preview(current.id, markdown.storageKey, signal)); }
      catch { if (!signal?.aborted) setPreviewError(true); }
    }
    setLoading(false);
  }, [api, taskId]);

  useEffect(() => {
    const controller = new AbortController();
    void load(controller.signal).catch(() => { if (!controller.signal.aborted) { setPreviewError(true); setLoading(false); } });
    return () => controller.abort();
  }, [load]);

  const watchedTaskId = task?.id;
  const watchedTaskStatus = task?.status;
  useEffect(() => {
    if (!watchedTaskId || !watchedTaskStatus || TERMINAL.has(watchedTaskStatus)) return;
    const controller = new AbortController();
    void api.watchTask(watchedTaskId, (event) => {
      setTask((current) => current ? { ...current, status: event.status, progressMillionths: event.progressMillionths, updatedAtMs: Date.now() } : current);
      if (event.terminal) void load();
    }, controller.signal);
    return () => controller.abort();
  }, [api, load, watchedTaskId, watchedTaskStatus]);

  const markdown = task?.artifacts.find((artifact) => artifact.kind === "markdown");
  const title = task ? taskName(task, `${t("restoredTask")} ${task.id.slice(0, 8)}`) : t("loadingPreview");
  const FormatIcon = iconForFormat(task ? taskFormat(task) : "auto");
  const assets = useMemo(() => task?.artifacts.filter((artifact) => artifact.kind === "asset") ?? [], [task]);

  const pin = async () => { if (task) setTask(await api.setPinned(task.id, !task.pinned)); };
  const retry = async () => { if (task) { const next = await api.retry(task.id); navigate(`/results/${next.id}`); } };
  const remove = async () => { if (!task || !window.confirm(t("deleteWarning"))) return; await api.deleteTask(task.id); navigate("/history"); };

  return <section className="result-route" aria-labelledby="result-title">
    <header className="result-toolbar">
      <div className="result-identity">
        <RouteLink className="icon-button neutral" href="/workbench"><ArrowLeft size={19} aria-hidden="true" /><span className="visually-hidden">{t("backWorkbench")}</span></RouteLink>
        <span className="file-type-icon"><FormatIcon size={20} aria-hidden="true" /></span>
        <div><p className="section-kicker">{t("conversionResult")}</p><h1 id="result-title">{title}</h1></div>
        {task && <span className={`result-status ${task.status}`}>{task.status === "succeeded" ? <CheckCircle2 size={14} aria-hidden="true" /> : <CircleAlert size={14} aria-hidden="true" />}{t(task.status)}</span>}
      </div>
      <div className="result-actions">
        <div className="view-toggle" role="group" aria-label={t("previewMode")}><button type="button" aria-pressed={mode === "rendered"} onClick={() => setMode("rendered")}><Eye size={16} aria-hidden="true" />{t("renderedPreview")}</button><button type="button" aria-pressed={mode === "source"} onClick={() => setMode("source")}><Code2 size={16} aria-hidden="true" />{t("markdownSource")}</button></div>
        {task && markdown && <button type="button" onClick={() => void downloadArtifact(api, task, markdown.storageKey)}><Download size={16} aria-hidden="true" />{t("downloadMarkdown")}</button>}
        <button className="secondary" type="button" aria-expanded={drawer} onClick={() => setDrawer((value) => !value)}><Info size={16} aria-hidden="true" />{t("detailsAndResources")}</button>
        {task && <details className="task-menu"><summary aria-label={t("moreActions")}><MoreHorizontal size={19} aria-hidden="true" /></summary><div className="task-menu-popover"><button className="menu-action" type="button" onClick={() => void pin()}>{task.pinned ? <PinOff size={16} aria-hidden="true" /> : <Pin size={16} aria-hidden="true" />}{t(task.pinned ? "unpin" : "pin")}</button><button className="menu-action" type="button" onClick={() => void retry()}><RotateCcw size={16} aria-hidden="true" />{t("retry")}</button><button className="menu-action danger" type="button" onClick={() => void remove()}><Trash2 size={16} aria-hidden="true" />{t("deleteTask")}</button></div></details>}
      </div>
    </header>

    {batch.length > 1 && <nav className="batch-switcher" aria-label={t("batchResults")}>
      {batch.slice(0, 6).map((item) => <button key={item.id} type="button" aria-current={item.id === taskId ? "page" : undefined} onClick={() => navigate(`/results/${item.id}`)}>{taskName(item, item.id.slice(0, 8))}</button>)}
      {batch.length > 6 && <label><span className="visually-hidden">{t("moreBatchResults")}</span><select value={taskId} onChange={(event) => navigate(`/results/${event.target.value}`)}>{batch.map((item) => <option key={item.id} value={item.id}>{taskName(item, item.id.slice(0, 8))}</option>)}</select></label>}
    </nav>}

    <div className={`result-body ${drawer ? "drawer-open" : ""}`}>
      <div className="result-document-scroll" tabIndex={-1} role="document">
        <article className="document-canvas">
          {loading ? <div className="preview-loading" role="status"><LoaderCircle className="spin" size={22} aria-hidden="true" />{t("loadingPreview")}</div> : previewError ? <div className="result-empty" role="alert"><CircleAlert size={25} aria-hidden="true" /><h2>{t("previewFailed")}</h2></div> : !preview ? <div className="result-empty"><Code2 size={25} aria-hidden="true" /><h2>{t("noMarkdownResult")}</h2></div> : <>{preview.truncated && <p className="preview-notice" role="status">{t("previewTruncated")}</p>}{mode === "rendered" ? <SafeMarkdownPreview source={preview.text} /> : <pre className="markdown-source"><code>{preview.text}</code></pre>}</>}
        </article>
      </div>
      {drawer && task && <aside className="result-drawer" aria-label={t("detailsAndResources")}><div className="drawer-heading"><h2>{t("detailsAndResources")}</h2><button className="icon-button neutral" type="button" aria-label={t("close")} onClick={() => setDrawer(false)}><X size={18} aria-hidden="true" /></button></div><section><h3>{t("taskDetails")}</h3><dl><div><dt>ID</dt><dd><code>{task.id}</code></dd></div><div><dt>{t("created")}</dt><dd>{new Date(task.createdAtMs).toLocaleString()}</dd></div><div><dt>{t("updated")}</dt><dd>{new Date(task.updatedAtMs).toLocaleString()}</dd></div><div><dt>OCR</dt><dd>{task.configuration.ocrEnabled ? t("on") : t("off")}</dd></div></dl></section><section><h3>{t("resources")} ({assets.length})</h3>{assets.length === 0 ? <p className="muted">{t("noResources")}</p> : <ul className="drawer-list">{assets.map((artifact) => <li key={artifact.storageKey}><div><strong>{artifactLabel(artifact)}</strong><small>{artifact.mediaType ?? "application/octet-stream"} · {bytesLabel(artifact.byteLen)}</small></div><button className="icon-button" type="button" aria-label={`${t("download")} ${artifactLabel(artifact)}`} onClick={() => void downloadArtifact(api, task, artifact.storageKey)}><Download size={16} aria-hidden="true" /></button></li>)}</ul>}</section><section><h3>{t("diagnostics")}</h3>{task.diagnostics.length === 0 ? <p className="muted">{t("noDiagnostics")}</p> : <ul className="diagnostic-list">{task.diagnostics.map((item, index) => <li key={`${item.code}-${index}`}>{item.code}</li>)}</ul>}</section><section><h3>{t("otherArtifacts")}</h3><div className="artifact-actions">{task.artifacts.filter((artifact) => artifact.kind !== "markdown" && artifact.kind !== "asset").map((artifact) => <button className="secondary" type="button" key={artifact.storageKey} onClick={() => void downloadArtifact(api, task, artifact.storageKey)}>{artifact.kind === "bundle" ? <Package size={16} aria-hidden="true" /> : artifact.kind === "documentIr" ? <Braces size={16} aria-hidden="true" /> : <FileJson size={16} aria-hidden="true" />}{artifactLabel(artifact)}</button>)}</div></section></aside>}
    </div>
  </section>;
}

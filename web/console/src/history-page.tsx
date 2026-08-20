import { useCallback, useEffect, useMemo, useState } from "react";
import {
  CircleAlert, Download, LoaderCircle, MoreHorizontal, Pin, PinOff, RefreshCw, RotateCcw,
  SlidersHorizontal, Trash2,
} from "lucide-react";
import type { ApiClient, TaskCursor, TaskRecord, TaskStatus } from "./api";
import { RouteLink, useRouter } from "./router";
import { useI18n } from "./i18n";
import { downloadArtifact, iconForFormat, taskFormat, taskName } from "./task-ui";

const STATUSES: TaskStatus[] = ["pending", "running", "converted", "succeeded", "failed", "interrupted", "cancelled"];

export function HistoryPage({ api }: { api: ApiClient }) {
  const { t } = useI18n();
  const { navigate } = useRouter();
  const [tasks, setTasks] = useState<TaskRecord[]>([]);
  const [statusFilter, setStatusFilter] = useState<TaskStatus | "">("");
  const [pinnedOnly, setPinnedOnly] = useState(false);
  const [nextCursor, setNextCursor] = useState<TaskCursor>();
  const [loading, setLoading] = useState(true);
  const [cleaning, setCleaning] = useState(false);
  const [message, setMessage] = useState("");
  const loadError = t("loadTasksError");

  const load = useCallback(async (after?: TaskCursor, signal?: AbortSignal) => {
    const page = await api.listTasks({ limit: 50, ...(after ? { after } : {}), ...(statusFilter ? { status: statusFilter } : {}), ...(pinnedOnly ? { pinned: true } : {}) }, signal);
    setTasks((current) => after ? [...current, ...page.tasks.filter((task) => !current.some((item) => item.id === task.id))] : page.tasks);
    setNextCursor(page.nextCursor);
  }, [api, pinnedOnly, statusFilter]);

  useEffect(() => {
    const controller = new AbortController(); setLoading(true);
    void api.listTasks({ limit: 50, ...(statusFilter ? { status: statusFilter } : {}), ...(pinnedOnly ? { pinned: true } : {}) }, controller.signal)
      .then((page) => { if (!controller.signal.aborted) { setTasks(page.tasks); setNextCursor(page.nextCursor); } })
      .catch(() => { if (!controller.signal.aborted) setMessage(loadError); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [api, loadError, pinnedOnly, statusFilter]);

  const batchCounts = useMemo(() => {
    const counts = new Map<string, number>();
    for (const task of tasks) if (task.batchId) counts.set(task.batchId, (counts.get(task.batchId) ?? 0) + 1);
    return counts;
  }, [tasks]);

  const pin = async (task: TaskRecord) => {
    const updated = await api.setPinned(task.id, !task.pinned);
    setTasks((current) => current.map((item) => item.id === task.id ? updated : item));
  };
  const retry = async (task: TaskRecord) => { const next = await api.retry(task.id); navigate(`/results/${next.id}`); };
  const remove = async (task: TaskRecord) => { if (!window.confirm(t("deleteWarning"))) return; await api.deleteTask(task.id); setTasks((current) => current.filter((item) => item.id !== task.id)); };
  const cleanup = async () => {
    if (!window.confirm(t("cleanupWarning"))) return;
    setCleaning(true);
    try { const result = await api.cleanup(); await load(); setMessage(t("cleanupResult").replace("{tasks}", String(result.deletedTasks)).replace("{bytes}", (result.reclaimedBytes / 1048576).toFixed(1))); }
    catch { setMessage(t("loadTasksError")); }
    finally { setCleaning(false); }
  };

  return <section className="history-route" aria-labelledby="history-title">
    <div className="history-heading"><div><p className="eyebrow">RESULT HISTORY</p><h1 id="history-title">{t("history")}</h1></div><div className="history-toolbar"><label className="compact-select"><SlidersHorizontal size={16} aria-hidden="true" /><span className="visually-hidden">{t("filterStatus")}</span><select value={statusFilter} onChange={(event) => setStatusFilter(event.target.value as TaskStatus | "")}><option value="">{t("allStatuses")}</option>{STATUSES.map((status) => <option key={status} value={status}>{t(status)}</option>)}</select></label><label className="check compact-check"><input type="checkbox" checked={pinnedOnly} onChange={(event) => setPinnedOnly(event.target.checked)} />{t("pinnedOnly")}</label><button className="secondary" type="button" onClick={() => void load()}><RefreshCw size={16} aria-hidden="true" />{t("refresh")}</button><button className="secondary danger" type="button" disabled={cleaning} onClick={() => void cleanup()}><Trash2 size={16} aria-hidden="true" />{t("cleanup")}</button></div></div>
    <div className={`message-bar ${message ? "visible" : ""}`} role="status">{message && <><CircleAlert size={17} aria-hidden="true" />{message}</>}</div>
    <div className="history-table-shell">
      <div className="history-table-head" aria-hidden="true"><span>{t("file")}</span><span>{t("status")}</span><span>{t("updated")}</span><span>{t("size")}</span><span /></div>
      <div className="history-table-scroll">
        {loading ? <div className="loading-state" role="status"><LoaderCircle className="spin" size={21} aria-hidden="true" />{t("loading")}</div> : tasks.length === 0 ? <div className="history-empty"><h2>{t("noTasks")}</h2></div> : tasks.map((task) => { const FormatIcon = iconForFormat(taskFormat(task)); const markdown = task.artifacts.find((artifact) => artifact.kind === "markdown"); const batchCount = task.batchId ? batchCounts.get(task.batchId) ?? 1 : 1; return <article className="history-row" key={task.id}>
          <RouteLink className="history-file" href={`/results/${task.id}`}><span className="file-type-icon"><FormatIcon size={19} aria-hidden="true" /></span><span><strong>{taskName(task, `${t("restoredTask")} ${task.id.slice(0, 8)}`)}</strong><small>{taskFormat(task).toUpperCase()}{batchCount > 1 ? ` · ${t("batchOf").replace("{count}", String(batchCount))}` : ""}</small></span></RouteLink>
          <span className={`history-status ${task.status}`}>{t(task.status)}</span>
          <time dateTime={new Date(task.updatedAtMs).toISOString()}>{new Date(task.updatedAtMs).toLocaleString()}</time>
          <span>{task.artifacts.length ? `${task.artifacts.length} ${t("artifacts")}` : "—"}</span>
          <div className="history-actions">{markdown && <button className="icon-button" type="button" aria-label={`${t("download")} ${taskName(task, task.id)}`} onClick={() => void downloadArtifact(api, task, markdown.storageKey)}><Download size={16} aria-hidden="true" /></button>}<details className="task-menu"><summary aria-label={t("moreActions")}><MoreHorizontal size={18} aria-hidden="true" /></summary><div className="task-menu-popover"><button className="menu-action" type="button" onClick={() => void pin(task)}>{task.pinned ? <PinOff size={16} aria-hidden="true" /> : <Pin size={16} aria-hidden="true" />}{t(task.pinned ? "unpin" : "pin")}</button><button className="menu-action" type="button" onClick={() => void retry(task)}><RotateCcw size={16} aria-hidden="true" />{t("retry")}</button><button className="menu-action danger" type="button" onClick={() => void remove(task)}><Trash2 size={16} aria-hidden="true" />{t("deleteTask")}</button></div></details></div>
        </article>; })}
        {nextCursor && <button className="secondary load-more" type="button" onClick={() => void load(nextCursor)}>{t("loadMore")}</button>}
      </div>
    </div>
  </section>;
}

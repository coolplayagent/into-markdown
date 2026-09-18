import { useTaskRuntime, useTaskRevision } from "./task-provider";
import { useEffect, useMemo, useState } from "react";
import { CheckCircle2, ChevronLeft, ChevronRight, CircleAlert, Clock3, Search, Trash2 } from "lucide-react";
import type { ApiClient, TaskRecord, TaskStatus } from "./api";
import { useI18n } from "./i18n";
import { TERMINAL, taskFailureLabel, taskName } from "./task-ui";

const PAGE_SIZE = 6;

export function HistoryPanel({ api, workflow = "conversion", tasks, fallbackName, onOpen, onCleanup, feedback }: { api?: ApiClient; workflow?: "conversion" | "meetingTranscript"; tasks: TaskRecord[]; fallbackName: string; onOpen: (id: string) => void; onCleanup?: () => void; feedback?: { kind: "success" | "error"; message: string } | null }) {
  const runtime = useTaskRuntime(); useTaskRevision();
  const historyRevision = runtime?.historyRevision;
  const { locale, t } = useI18n();
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<"all" | TaskStatus>("all");
  const [page, setPage] = useState(0);
  const [remote, setRemote] = useState<TaskRecord[]>([]);
  const [cursors, setCursors] = useState<Array<{ updatedAtMs: number; id: string } | undefined>>([undefined]);
  const [nextCursor, setNextCursor] = useState<{ updatedAtMs: number; id: string } | undefined>();
  const [loading, setLoading] = useState(false);
  const [failed, setFailed] = useState(false);
  const completionKey = tasks.filter(task => TERMINAL.has(task.status)).map(task => `${task.id}:${task.updatedAtMs}`).join();
  useEffect(() => { setCursors([undefined]); }, [query, status]);
  useEffect(() => {
    if (!api) return;
    const controller = new AbortController();
    setLoading(true); setFailed(false);
    const timer = setTimeout(() => {
      void api.listTasks({ limit: PAGE_SIZE, workflow, active: false, ...(status !== "all" ? { status } : {}), ...(query.trim() ? { search: query.trim() } : {}), ...(cursors[page] ? { after: cursors[page]! } : {}) }, controller.signal)
        .then(result => { if (!controller.signal.aborted) { setRemote(result.tasks); setNextCursor(result.nextCursor); } })
        .catch(() => { if (!controller.signal.aborted) setFailed(true); })
        .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    }, query ? 200 : 0);
    return () => { clearTimeout(timer); controller.abort(); };
  }, [api, workflow, query, status, page, cursors, completionKey, feedback, historyRevision]);
  const terminal = useMemo(() => tasks.filter((task) => TERMINAL.has(task.status)), [tasks]);
  const filtered = useMemo(() => terminal.filter((task) => (status === "all" || task.status === status)
    && taskName(task, fallbackName).toLocaleLowerCase().includes(query.trim().toLocaleLowerCase())), [fallbackName, query, status, terminal]);
  const pages = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
  const safePage = Math.min(page, pages - 1);
  const visible = api ? remote.filter(task => !runtime?.deletedIds.has(task.id)).map(task => runtime?.tasks.get(task.id) ?? task) : filtered.slice(safePage * PAGE_SIZE, (safePage + 1) * PAGE_SIZE);
  useEffect(() => { setPage(0); }, [query, status]);
  useEffect(() => { if (!api && page >= pages) setPage(Math.max(0, pages - 1)); }, [api, page, pages]);
  return <aside className="history-rail recent-history" aria-labelledby="history-rail-title">
    <header><div><span className="section-kicker">{t("history")}</span><h2 id="history-rail-title">{t("recentHistory")}</h2></div><span className="history-count">{api ? remote.length : terminal.length}</span></header>
    <div className="history-rail-filters"><label><Search size={15} aria-hidden="true" /><span className="visually-hidden">{t("historySearch")}</span><input value={query} placeholder={t("historySearch")} onChange={(event) => setQuery(event.target.value)} /></label><select aria-label={t("filterStatus")} value={status} onChange={(event) => setStatus(event.target.value as "all" | TaskStatus)}><option value="all">{t("allStatuses")}</option>{(["succeeded", "failed", "interrupted", "cancelled"] as TaskStatus[]).map((value) => <option key={value} value={value}>{t(value)}</option>)}</select></div>
    {loading && <p role="status">{locale === "zh-CN" ? "正在加载历史记录" : "Loading history"}</p>}{failed && <p role="alert">{t("loadTasksError")}</p>}
    {visible.length === 0 ? <div className="history-empty"><Clock3 size={22} /><p>{terminal.length === 0 ? t("noTasks") : t("noHistoryMatches")}</p></div> : <ul className="recent-history-scroll">{visible.map((task) => { const failure = task.status === "failed" || task.status === "interrupted" ? taskFailureLabel(task, t) : ""; return <li key={task.id}><button className="recent-task-link" type="button" onClick={() => onOpen(task.id)}><span><strong>{taskName(task, fallbackName)}</strong><small className={failure ? "failure-reason" : undefined}>{failure || new Date(task.updatedAtMs).toLocaleString(locale, { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" })}</small></span><span className={`history-status ${task.status}`}>{t(task.status)}</span></button></li>; })}</ul>}
    <footer className="history-rail-footer"><span>{api ? `${page + 1}` : `${filtered.length} ${t("tasks")} · ${safePage + 1}/${pages}`}</span><div className="history-footer-actions">{(onCleanup || feedback) && <span className="history-cleanup-slot">{onCleanup && <button className="history-cleanup secondary danger" type="button" aria-label={t("cleanup")} onClick={onCleanup}><Trash2 size={14} />{t("cleanup")}</button>}{feedback && <span className={`history-rail-feedback ${feedback.kind}`} role={feedback.kind === "error" ? "alert" : "status"}>{feedback.kind === "success" ? <CheckCircle2 size={15} /> : <CircleAlert size={15} />}<span>{feedback.message}</span></span>}</span>}<span className="history-page-buttons"><button className="icon-button neutral" type="button" aria-label={t("previousPage")} disabled={page === 0 || loading} onClick={() => setPage((value) => Math.max(0, value - 1))}><ChevronLeft size={16} /></button><button className="icon-button neutral" type="button" aria-label={t("nextPage")} disabled={loading || (api ? !nextCursor : safePage + 1 >= pages)} onClick={() => { if (api) { setCursors(current => [...current.slice(0, page + 1), nextCursor]); setPage(page + 1); } else setPage(value => Math.min(pages - 1, value + 1)); }}><ChevronRight size={16} /></button></span></div></footer>
  </aside>;
}

import { useEffect, useMemo, useState } from "react";
import { CheckCircle2, ChevronLeft, ChevronRight, CircleAlert, Clock3, Search, Trash2 } from "lucide-react";
import type { TaskRecord, TaskStatus } from "./api";
import { useI18n } from "./i18n";
import { TERMINAL, diagnosticLabel, taskName } from "./task-ui";

const PAGE_SIZE = 6;

export function HistoryPanel({ tasks, fallbackName, onOpen, onCleanup, feedback }: { tasks: TaskRecord[]; fallbackName: string; onOpen: (id: string) => void; onCleanup?: () => void; feedback?: { kind: "success" | "error"; message: string } | null }) {
  const { locale, t } = useI18n();
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<"all" | TaskStatus>("all");
  const [page, setPage] = useState(0);
  const terminal = useMemo(() => tasks.filter((task) => TERMINAL.has(task.status)), [tasks]);
  const filtered = useMemo(() => terminal.filter((task) => (status === "all" || task.status === status)
    && taskName(task, fallbackName).toLocaleLowerCase().includes(query.trim().toLocaleLowerCase())), [fallbackName, query, status, terminal]);
  const pages = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
  const safePage = Math.min(page, pages - 1);
  const visible = filtered.slice(safePage * PAGE_SIZE, (safePage + 1) * PAGE_SIZE);
  useEffect(() => { setPage(0); }, [query, status]);
  useEffect(() => { if (page >= pages) setPage(Math.max(0, pages - 1)); }, [page, pages]);
  return <aside className="history-rail recent-history" aria-labelledby="history-rail-title">
    <header><div><span className="section-kicker">{t("history")}</span><h2 id="history-rail-title">{t("recentHistory")}</h2></div><span className="history-count">{terminal.length}</span></header>
    <div className="history-rail-filters"><label><Search size={15} aria-hidden="true" /><span className="visually-hidden">{t("historySearch")}</span><input value={query} placeholder={t("historySearch")} onChange={(event) => setQuery(event.target.value)} /></label><select aria-label={t("filterStatus")} value={status} onChange={(event) => setStatus(event.target.value as "all" | TaskStatus)}><option value="all">{t("allStatuses")}</option>{(["succeeded", "failed", "interrupted", "cancelled"] as TaskStatus[]).map((value) => <option key={value} value={value}>{t(value)}</option>)}</select></div>
    {visible.length === 0 ? <div className="history-empty"><Clock3 size={22} /><p>{terminal.length === 0 ? t("noTasks") : t("noHistoryMatches")}</p></div> : <ul className="recent-history-scroll">{visible.map((task) => { const failure = task.status === "failed" || task.status === "interrupted" ? diagnosticLabel(task.diagnostics[0]?.code ?? "conversionFailed", t) : ""; return <li key={task.id}><button className="recent-task-link" type="button" onClick={() => onOpen(task.id)}><span><strong>{taskName(task, fallbackName)}</strong><small className={failure ? "failure-reason" : undefined}>{failure || new Date(task.updatedAtMs).toLocaleString(locale, { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" })}</small></span><span className={`history-status ${task.status}`}>{t(task.status)}</span></button></li>; })}</ul>}
    <footer className="history-rail-footer"><span>{filtered.length} {t("tasks")} · {safePage + 1}/{pages}</span><div className="history-footer-actions">{(onCleanup || feedback) && <span className="history-cleanup-slot">{onCleanup && <button className="history-cleanup secondary danger" type="button" aria-label={t("cleanup")} onClick={onCleanup}><Trash2 size={14} />{t("cleanup")}</button>}{feedback && <span className={`history-rail-feedback ${feedback.kind}`} role={feedback.kind === "error" ? "alert" : "status"}>{feedback.kind === "success" ? <CheckCircle2 size={15} /> : <CircleAlert size={15} />}<span>{feedback.message}</span></span>}</span>}<span className="history-page-buttons"><button className="icon-button neutral" type="button" aria-label={t("previousPage")} disabled={safePage === 0} onClick={() => setPage((value) => Math.max(0, value - 1))}><ChevronLeft size={16} /></button><button className="icon-button neutral" type="button" aria-label={t("nextPage")} disabled={safePage + 1 >= pages} onClick={() => setPage((value) => Math.min(pages - 1, value + 1))}><ChevronRight size={16} /></button></span></div></footer>
  </aside>;
}

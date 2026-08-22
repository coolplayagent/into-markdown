import { useEffect, useMemo, useRef, useState } from "react";
import { ChevronLeft, ChevronRight, Clock3, Search, Trash2, X } from "lucide-react";
import type { TaskRecord, TaskStatus } from "./api";
import { useI18n } from "./i18n";
import { TERMINAL, diagnosticLabel, taskName } from "./task-ui";

const PAGE_SIZE = 10;

export function HistoryPanel({ tasks, fallbackName, onOpen, onCleanup }: { tasks: TaskRecord[]; fallbackName: string; onOpen: (id: string) => void; onCleanup?: () => void }) {
  const { locale, t } = useI18n();
  const [drawer, setDrawer] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const recent = tasks.filter((task) => TERMINAL.has(task.status)).slice(0, 5);
  return <aside className="history-rail recent-history" aria-labelledby="history-rail-title">
    <header><div><span className="section-kicker">{t("history")}</span><h2 id="history-rail-title">{t("recentHistory")}</h2></div><span className="history-count">{tasks.length}</span></header>
    {recent.length === 0 ? <div className="history-empty"><Clock3 size={22} /><p>{t("noTasks")}</p></div> : <ul className="recent-history-scroll">{recent.map((task) => { const failure = task.status === "failed" || task.status === "interrupted" ? diagnosticLabel(task.diagnostics[0]?.code ?? "conversionFailed", t) : ""; return <li key={task.id}><button className="recent-task-link" type="button" onClick={() => onOpen(task.id)}><span><strong>{taskName(task, fallbackName)}</strong><small className={failure ? "failure-reason" : undefined}>{failure || new Date(task.updatedAtMs).toLocaleString(locale, { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" })}</small></span><span className={`history-status ${task.status}`}>{t(task.status)}</span></button></li>; })}</ul>}
    <button ref={trigger} className="secondary view-all-history" type="button" onClick={() => setDrawer(true)}>{t("viewAllHistory")}</button>
    {drawer && <HistoryDrawer tasks={tasks} fallbackName={fallbackName} onOpen={(id) => { onOpen(id); setDrawer(false); }} {...(onCleanup ? { onCleanup } : {})} onClose={() => { setDrawer(false); window.requestAnimationFrame(() => trigger.current?.focus()); }} />}
  </aside>;
}

function HistoryDrawer({ tasks, fallbackName, onOpen, onClose, onCleanup }: { tasks: TaskRecord[]; fallbackName: string; onOpen: (id: string) => void; onClose: () => void; onCleanup?: () => void }) {
  const { locale, t } = useI18n();
  const close = useRef<HTMLButtonElement>(null);
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<"all" | TaskStatus>("all");
  const [page, setPage] = useState(0);
  useEffect(() => { close.current?.focus(); }, []);
  useEffect(() => { setPage(0); }, [query, status]);
  useEffect(() => {
    const escape = (event: KeyboardEvent) => { if (event.key === "Escape") onClose(); };
    window.addEventListener("keydown", escape); return () => window.removeEventListener("keydown", escape);
  }, [onClose]);
  const filtered = useMemo(() => tasks.filter((task) => (status === "all" || task.status === status)
    && taskName(task, fallbackName).toLocaleLowerCase().includes(query.trim().toLocaleLowerCase())), [fallbackName, query, status, tasks]);
  const pages = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
  const safePage = Math.min(page, pages - 1);
  const visible = filtered.slice(safePage * PAGE_SIZE, (safePage + 1) * PAGE_SIZE);
  return <div className="sheet-backdrop history-drawer-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
    <section className="history-drawer" role="dialog" aria-modal="true" aria-labelledby="history-drawer-title">
      <header className="drawer-heading"><div><span className="section-kicker">{t("history")}</span><h2 id="history-drawer-title">{t("viewAllHistory")}</h2></div><button ref={close} className="icon-button neutral" type="button" aria-label={t("close")} onClick={onClose}><X size={19} /></button></header>
      <div className="history-filters"><label><Search size={16} /><span className="visually-hidden">{t("historySearch")}</span><input value={query} placeholder={t("historySearch")} onChange={(event) => setQuery(event.target.value)} /></label><select aria-label={t("filterStatus")} value={status} onChange={(event) => setStatus(event.target.value as "all" | TaskStatus)}><option value="all">{t("allStatuses")}</option>{(["succeeded", "failed", "interrupted", "cancelled"] as TaskStatus[]).map((value) => <option key={value} value={value}>{t(value)}</option>)}</select></div>
      <div className="history-drawer-list">{visible.length === 0 ? <p className="history-drawer-empty">{t("noHistoryMatches")}</p> : <ul>{visible.map((task) => <li key={task.id}><button type="button" onClick={() => onOpen(task.id)}><span><strong>{taskName(task, fallbackName)}</strong><small>{new Date(task.updatedAtMs).toLocaleString(locale)}</small></span><span className={`history-status ${task.status}`}>{t(task.status)}</span></button></li>)}</ul>}</div>
      <footer><span>{filtered.length} {t("tasks")} · {t("page")} {safePage + 1}/{pages}</span><div>{onCleanup && <button className="secondary" type="button" aria-label={t("cleanup")} onClick={onCleanup}><Trash2 size={16} />{t("cleanup")}</button>}<button className="secondary" type="button" disabled={safePage === 0} onClick={() => setPage((value) => Math.max(0, value - 1))}><ChevronLeft size={16} />{t("previousPage")}</button><button className="secondary" type="button" disabled={safePage + 1 >= pages} onClick={() => setPage((value) => Math.min(pages - 1, value + 1))}>{t("nextPage")}<ChevronRight size={16} /></button></div></footer>
    </section>
  </div>;
}

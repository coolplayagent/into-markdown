import { useEffect, useRef, useState } from "react";
import { CircleAlert, FileText, History, Languages, LoaderCircle, Settings2, ShieldCheck, Wrench } from "lucide-react";
import type { ApiClient } from "./api";
import { HistoryPage } from "./history-page";
import { I18nProvider, useI18n } from "./i18n";
import { ResultPage } from "./result-page";
import { RouteLink, Router, useRouter } from "./router";
import { ThemeProvider, useTheme } from "./theme";
import { WorkbenchPage } from "./workbench-page";

function Preferences() {
  const { locale, setLocale, t } = useI18n();
  const { theme, setTheme } = useTheme();
  return <div className="preferences">
    <label className="compact-select"><Languages size={16} aria-hidden="true" /><span className="visually-hidden">{t("language")}</span><select aria-label={t("language")} value={locale} onChange={(event) => setLocale(event.target.value === "zh-CN" ? "zh-CN" : "en")}><option value="zh-CN">简体中文</option><option value="en">English</option></select></label>
    <label className="compact-select"><Settings2 size={16} aria-hidden="true" /><span className="visually-hidden">{t("theme")}</span><select aria-label={t("theme")} value={theme} onChange={(event) => { const value = event.target.value; setTheme(value === "light" || value === "dark" ? value : "system"); }}><option value="system">{t("system")}</option><option value="light">{t("light")}</option><option value="dark">{t("dark")}</option></select></label>
  </div>;
}

function ServiceBadge({ api }: { api: ApiClient }) {
  const { t } = useI18n();
  const [state, setState] = useState<"checking" | "ready" | "error">("checking");
  useEffect(() => {
    const controller = new AbortController();
    void api.status(controller.signal).then((value) => setState(value.localApi.available && value.documentConsole.available ? "ready" : "error"), () => { if (!controller.signal.aborted) setState("error"); });
    return () => controller.abort();
  }, [api]);
  const Icon = state === "checking" ? LoaderCircle : state === "ready" ? ShieldCheck : CircleAlert;
  return <RouteLink href="/status" className={`service-badge ${state}`}><Icon size={17} aria-hidden="true" className={state === "checking" ? "spin" : ""} /><span>{t(state === "ready" ? "systemReady" : state === "error" ? "systemNeedsAttention" : "checkingSystem")}</span></RouteLink>;
}

function StatusPage({ api }: { api: ApiClient }) {
  const { t } = useI18n();
  const [status, setStatus] = useState<"loading" | "ok" | "error">("loading");
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    const controller = new AbortController(); setStatus("loading");
    void api.status(controller.signal).then((value) => setStatus(value.localApi.available && value.documentConsole.available ? "ok" : "error"), () => { if (!controller.signal.aborted) setStatus("error"); });
    return () => controller.abort();
  }, [api, attempt]);
  const Icon = status === "ok" ? ShieldCheck : status === "error" ? CircleAlert : LoaderCircle;
  return <section className="status-route"><div className="page-heading status-heading"><p className="eyebrow">LOCAL SERVICE</p><h1>{t("capabilityCenter")}</h1><p>{t("capabilityCenterIntro")}</p></div><section className={`card status-card ${status}`} role={status === "error" ? "alert" : "status"}><span className="status-icon"><Icon size={24} aria-hidden="true" className={status === "loading" ? "spin" : ""} /></span><div><h2>{status === "ok" ? t("apiAvailable") : status === "error" ? t("errorTitle") : t("loading")}</h2><p>{status === "ok" ? t("allLocalServicesReady") : status === "error" ? t("errorDetail") : t("checkingSystemDetail")}</p>{status === "error" && <button type="button" onClick={() => setAttempt((value) => value + 1)}>{t("retry")}</button>}</div></section><RouteLink className="back-link" href="/workbench"><FileText size={16} aria-hidden="true" />{t("backWorkbench")}</RouteLink></section>;
}

function Content({ api }: { api: ApiClient }) {
  const { path } = useRouter();
  const { t } = useI18n();
  const main = useRef<HTMLElement>(null);
  const result = /^\/results\/([0-9a-f]{32})$/.exec(path);
  const isWorkbench = path === "/" || path === "/workbench";
  useEffect(() => {
    const title = path === "/status" ? t("status") : path === "/history" ? t("history") : result ? t("conversionResult") : t("workbench");
    document.title = `${title} · into-markdown`;
  }, [path, result, t]);
  useEffect(() => { main.current?.focus(); }, [path]);
  return <main id="main" ref={main} tabIndex={-1}>
    <div className="route-surface" hidden={!isWorkbench}><WorkbenchPage api={api} /></div>
    {path === "/history" ? <HistoryPage api={api} /> : path === "/status" ? <StatusPage api={api} /> : result ? <ResultPage api={api} taskId={result[1]!} /> : !isWorkbench ? <section className="card not-found"><p className="error-number">404</p><h1>{t("notFound")}</h1><RouteLink href="/workbench">{t("backWorkbench")}</RouteLink></section> : null}
  </main>;
}

function Shell({ api }: { api: ApiClient }) {
  const { path } = useRouter();
  const { t } = useI18n();
  return <><a className="skip-link" href="#main">{t("skip")}</a><div className="app-shell"><header className="app-header"><RouteLink href="/workbench" className="brand" aria-label={t("appName")}><span className="brand-mark" aria-hidden="true">M↓</span><span>into-markdown</span></RouteLink><nav className="primary-nav" aria-label={t("primaryNavigation")}><RouteLink href="/workbench" className={path === "/" || path === "/workbench" ? "active" : ""}><Wrench size={17} aria-hidden="true" />{t("workbench")}</RouteLink><RouteLink href="/history" className={path === "/history" || path.startsWith("/results/") ? "active" : ""}><History size={17} aria-hidden="true" />{t("history")}</RouteLink></nav><div className="header-actions"><ServiceBadge api={api} /><Preferences /></div></header><Content api={api} /></div></>;
}

export function App({ api }: { api: ApiClient }) {
  return <I18nProvider><ThemeProvider><Router><Shell api={api} /></Router></ThemeProvider></I18nProvider>;
}

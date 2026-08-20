import { useEffect, useRef, useState } from "react";
import { ChevronDown, CircleAlert, Languages, LoaderCircle, Settings2, ShieldCheck } from "lucide-react";
import type { ApiClient } from "./api";
import { I18nProvider, useI18n } from "./i18n";
import { RouteLink, Router, useRouter } from "./router";
import { ThemeProvider, useTheme } from "./theme";
import { WorkbenchPage } from "./workbench-page";
import { MeetingPage } from "./meeting-page";

function Preferences() {
  const { locale, setLocale, t } = useI18n();
  const { theme, setTheme } = useTheme();
  return <div className="preferences">
    <label className="compact-select"><Languages size={16} aria-hidden="true" /><span className="visually-hidden">{t("language")}</span><select aria-label={t("language")} value={locale} onChange={(event) => setLocale(event.target.value === "zh-CN" ? "zh-CN" : "en")}><option value="zh-CN">简体中文</option><option value="en">English</option></select><ChevronDown size={14} aria-hidden="true" /></label>
    <label className="compact-select"><Settings2 size={16} aria-hidden="true" /><span className="visually-hidden">{t("theme")}</span><select aria-label={t("theme")} value={theme} onChange={(event) => { const value = event.target.value; setTheme(value === "light" || value === "dark" ? value : "system"); }}><option value="system">{t("system")}</option><option value="light">{t("light")}</option><option value="dark">{t("dark")}</option></select><ChevronDown size={14} aria-hidden="true" /></label>
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
  return <span className={`service-badge ${state}`} role="status"><Icon size={17} aria-hidden="true" className={state === "checking" ? "spin" : ""} /><span>{t(state === "ready" ? "systemReady" : state === "error" ? "systemNeedsAttention" : "checkingSystem")}</span></span>;
}

function Content({ api }: { api: ApiClient }) {
  const { path } = useRouter();
  const { t } = useI18n();
  const main = useRef<HTMLElement>(null);
  const meetingResult = /^\/meetings\/results\/([0-9a-f]{32})$/.exec(path);
  const workbenchResult = /^\/results\/([0-9a-f]{32})$/.exec(path);
  const meeting = path === "/meetings" || Boolean(meetingResult);
  useEffect(() => {
    document.title = `${t(meeting ? "meetingNotes" : "workbench")} · into-markdown`;
  }, [meeting, t]);
  useEffect(() => { main.current?.focus(); }, [path]);
  return <main id="main" ref={main} tabIndex={-1}>
    <div className="route-surface">{meeting
      ? <MeetingPage api={api} initialTaskId={meetingResult?.[1]} />
      : <WorkbenchPage api={api} initialTaskId={workbenchResult?.[1]} />}</div>
  </main>;
}

function Shell({ api }: { api: ApiClient }) {
  const { t } = useI18n();
  const { path } = useRouter();
  return <><a className="skip-link" href="#main">{t("skip")}</a><div className="app-shell"><header className="app-header"><RouteLink href="/workbench" className="brand" ariaLabel={t("appName")}><span className="brand-mark" aria-hidden="true">M↓</span><span>into-markdown</span></RouteLink><nav className="primary-nav" aria-label={t("primaryNavigation")}><RouteLink href="/workbench" className={!path.startsWith("/meetings") ? "active" : ""}>{t("workbench")}</RouteLink><RouteLink href="/meetings" className={path.startsWith("/meetings") ? "active" : ""}>{t("meetingNotes")}</RouteLink></nav><div className="header-actions"><ServiceBadge api={api} /><Preferences /></div></header><Content api={api} /></div></>;
}

export function App({ api }: { api: ApiClient }) {
  return <I18nProvider><ThemeProvider><Router><Shell api={api} /></Router></ThemeProvider></I18nProvider>;
}

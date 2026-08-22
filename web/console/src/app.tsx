import { useEffect, useRef } from "react";
import { ChevronDown, CircleAlert, Languages, LoaderCircle, Settings2, ShieldCheck } from "lucide-react";
import type { ApiClient } from "./api";
import { I18nProvider, useI18n } from "./i18n";
import { RouteLink, Router, useRouter } from "./router";
import { ThemeProvider, useTheme } from "./theme";
import { WorkbenchPage } from "./workbench-page";
import { MeetingPage } from "./meeting-page";
import { AdminPage, type AdminSection } from "./admin-page";
import { CapabilityProvider, useCapabilities } from "./capability-store";

function Preferences() {
  const { locale, setLocale, t } = useI18n();
  const { theme, setTheme } = useTheme();
  return <div className="preferences">
    <label className="compact-select"><Languages size={16} aria-hidden="true" /><span className="visually-hidden">{t("language")}</span><select aria-label={t("language")} value={locale} onChange={(event) => setLocale(event.target.value === "zh-CN" ? "zh-CN" : "en")}><option value="zh-CN">简体中文</option><option value="en">English</option></select><ChevronDown size={14} aria-hidden="true" /></label>
    <label className="compact-select"><Settings2 size={16} aria-hidden="true" /><span className="visually-hidden">{t("theme")}</span><select aria-label={t("theme")} value={theme} onChange={(event) => { const value = event.target.value; setTheme(value === "light" || value === "dark" ? value : "system"); }}><option value="system">{t("system")}</option><option value="light">{t("light")}</option><option value="dark">{t("dark")}</option></select><ChevronDown size={14} aria-hidden="true" /></label>
  </div>;
}

function ServiceBadge() {
  const { t } = useI18n();
  const capabilities = useCapabilities();
  const state: "checking" | "ready" | "error" = !capabilities.snapshot
    ? capabilities.error ? "error" : "checking"
    : capabilities.snapshot.capabilities.some((item) => ["corrupt", "incompatible", "blocked"].includes(item.status)) ? "error" : "ready";
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
  const adminMatch = /^\/admin\/(capabilities|formats|providers|plugins|configuration|doctor)$/.exec(path);
  const requestedAdminSection = adminMatch?.[1] as AdminSection | undefined;
  const legacyAdminContext = requestedAdminSection === "providers" || requestedAdminSection === "plugins" || requestedAdminSection === "formats" ? requestedAdminSection : undefined;
  const adminSection = legacyAdminContext ? "capabilities" : requestedAdminSection;
  useEffect(() => {
    if (legacyAdminContext) window.history.replaceState(null, "", "/admin/capabilities");
  }, [legacyAdminContext]);
  useEffect(() => {
    document.title = `${t(adminSection ?? (meeting ? "speechTranscription" : "workbench"))} · into-markdown`;
  }, [adminSection, meeting, t]);
  useEffect(() => { main.current?.focus(); }, [path]);
  return <main id="main" ref={main} tabIndex={-1}>
    <div className="route-surface">{adminSection
      ? <AdminPage api={api} section={adminSection} {...(legacyAdminContext ? { initialContext: legacyAdminContext } : {})} />
      : meeting
      ? <MeetingPage api={api} initialTaskId={meetingResult?.[1]} />
      : <WorkbenchPage api={api} initialTaskId={workbenchResult?.[1]} />}</div>
  </main>;
}

function Shell({ api }: { api: ApiClient }) {
  const { t } = useI18n();
  const { path } = useRouter();
  return <><a className="skip-link" href="#main">{t("skip")}</a><div className="app-shell"><header className="app-header"><RouteLink href="/workbench" className="brand" ariaLabel={t("appName")}><span className="brand-mark" aria-hidden="true">M↓</span><span>into-markdown</span></RouteLink><nav className="primary-nav" aria-label={t("primaryNavigation")}><RouteLink href="/workbench" className={!path.startsWith("/meetings") && !path.startsWith("/admin/") ? "active" : ""}>{t("documentConversion")}</RouteLink><RouteLink href="/meetings" className={path.startsWith("/meetings") ? "active" : ""}>{t("speechTranscription")}</RouteLink><RouteLink href="/admin/capabilities" className={path.startsWith("/admin/") ? "active" : ""}>{t("administration")}</RouteLink></nav><div className="header-actions"><ServiceBadge /><Preferences /></div></header><Content api={api} /></div></>;
}

export function App({ api }: { api: ApiClient }) {
  return <I18nProvider><ThemeProvider><Router><CapabilityProvider api={api}><Shell api={api} /></CapabilityProvider></Router></ThemeProvider></I18nProvider>;
}

import { useEffect, useRef, useState } from "react";
import type { ApiClient, StatusResponse } from "./api";
import { ErrorBoundary } from "./error-boundary";
import { I18nProvider, useI18n } from "./i18n";
import { RouteLink, Router, useRouter } from "./router";
import { ThemeProvider, useTheme } from "./theme";

type LoadState =
  | { kind: "loading" }
  | { kind: "loaded"; status: StatusResponse }
  | { kind: "error" };

function Preferences() {
  const { locale, setLocale, t } = useI18n();
  const { theme, setTheme } = useTheme();
  return (
    <div className="preferences">
      <label>
        <span>{t("language")}</span>
        <select value={locale} onChange={(event) => setLocale(event.target.value === "zh-CN" ? "zh-CN" : "en")}>
          <option value="zh-CN">简体中文</option>
          <option value="en">English</option>
        </select>
      </label>
      <label>
        <span>{t("theme")}</span>
        <select value={theme} onChange={(event) => {
          const value = event.target.value;
          setTheme(value === "light" || value === "dark" ? value : "system");
        }}>
          <option value="system">{t("system")}</option>
          <option value="light">{t("light")}</option>
          <option value="dark">{t("dark")}</option>
        </select>
      </label>
    </div>
  );
}

function StatusPage({ api }: { api: ApiClient }) {
  const { t } = useI18n();
  const [attempt, setAttempt] = useState(0);
  const [state, setState] = useState<LoadState>({ kind: "loading" });
  useEffect(() => {
    const controller = new AbortController();
    setState({ kind: "loading" });
    void api.status(controller.signal).then(
      (status) => setState({ kind: "loaded", status }),
      () => {
        if (!controller.signal.aborted) setState({ kind: "error" });
      },
    );
    return () => controller.abort();
  }, [api, attempt]);

  if (state.kind === "loading") {
    return <div className="card status-card" role="status"><span className="spinner" aria-hidden="true" />{t("loading")}</div>;
  }
  if (state.kind === "error") {
    return (
      <section className="card status-card" role="alert">
        <span className="status-icon error" aria-hidden="true">!</span>
        <div><h2>{t("errorTitle")}</h2><p>{t("errorDetail")}</p>
          <button type="button" onClick={() => setAttempt((value) => value + 1)}>{t("retry")}</button>
        </div>
      </section>
    );
  }
  const available = state.status.localApi.available;
  return (
    <div className="status-grid">
      <section className="card status-card" aria-labelledby="api-heading">
        <span className={`status-icon ${available ? "ok" : "error"}`} aria-hidden="true">{available ? "✓" : "!"}</span>
        <div><h2 id="api-heading">{available ? t("apiAvailable") : t("apiUnavailable")}</h2>
          <p className="technical-code">{state.status.localApi.code}</p></div>
      </section>
      {!state.status.documentConsole.available && (
        <section className="card empty-card" aria-labelledby="console-heading">
          <div className="empty-illustration" aria-hidden="true"><span /><span /><span /></div>
          <div><h2 id="console-heading">{t("consoleUnavailable")}</h2><p>{t("unavailableDetail")}</p></div>
        </section>
      )}
    </div>
  );
}

function Content({ api }: { api: ApiClient }) {
  const { path } = useRouter();
  const { t } = useI18n();
  const main = useRef<HTMLElement>(null);
  useEffect(() => {
    document.title = `${path === "/" || path === "/status" ? t("status") : t("notFound")} · into-markdown`;
  }, [path, t]);
  useEffect(() => {
    main.current?.focus();
  }, [path]);
  return (
    <main id="main" ref={main} tabIndex={-1}>
      {path === "/" || path === "/status" ? (
        <><div className="page-heading"><p className="eyebrow">LOCAL CONSOLE</p><h1>{t("status")}</h1></div><StatusPage api={api} /></>
      ) : (
        <section className="card not-found"><p className="error-number">404</p><h1>{t("notFound")}</h1><RouteLink href="/status">{t("backStatus")}</RouteLink></section>
      )}
    </main>
  );
}

function Shell({ api }: { api: ApiClient }) {
  const { t } = useI18n();
  return (
    <Router>
      <a className="skip-link" href="#main">{t("skip")}</a>
      <div className="app-shell">
        <header>
          <RouteLink href="/status" className="brand" aria-label={t("appName")}><span className="brand-mark" aria-hidden="true">M↓</span><span>into-markdown</span></RouteLink>
          <Preferences />
        </header>
        <div className="body-shell">
          <nav aria-label={t("appName")}><RouteLink href="/status" className="nav-link">{t("status")}</RouteLink></nav>
          <Content api={api} />
        </div>
      </div>
    </Router>
  );
}

export function App({ api }: { api: ApiClient }) {
  return <ErrorBoundary><I18nProvider><ThemeProvider><Shell api={api} /></ThemeProvider></I18nProvider></ErrorBoundary>;
}

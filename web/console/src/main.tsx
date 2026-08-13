import { StrictMode, type ComponentType } from "react";
import { createRoot } from "react-dom/client";
import { createApiClient, type ApiClient } from "./api";
import { App } from "./app";
import { ErrorBoundary } from "./error-boundary";
import { renderStartupFailure } from "./startup-failure";
import "./styles.css";

type ConsoleRoot = ComponentType<{ api: ApiClient }>;

export function startConsole(session: string, Console: ConsoleRoot = App): void {
  const container = document.getElementById("app");
  if (!container) throw new Error("application root is unavailable");
  const discardError = (): void => {};
  createRoot(container, {
    onCaughtError: discardError,
    onRecoverableError: discardError,
    onUncaughtError: () => renderStartupFailure("startup"),
  }).render(
    <StrictMode><ErrorBoundary><Console api={createApiClient(session)} /></ErrorBoundary></StrictMode>,
  );
}

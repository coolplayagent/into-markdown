import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { createApiClient } from "./api";
import { App } from "./app";
import "./styles.css";

export function startConsole(session: string): void {
  const container = document.getElementById("app");
  if (!container) throw new Error("application root is unavailable");
  createRoot(container).render(<StrictMode><App api={createApiClient(session)} /></StrictMode>);
}

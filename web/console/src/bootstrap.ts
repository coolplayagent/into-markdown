import { takeSession } from "./session";
declare const INTO_MD_APP_MODULE: string;

type StartupFailure = "handoff" | "startup";

export function renderStartupFailure(kind: StartupFailure): void {
  const root = document.getElementById("app");
  if (!root) return;
  const main = document.createElement("main");
  main.className = "handoff-error";
  main.tabIndex = -1;
  const heading = document.createElement("h1");
  heading.textContent = "into-markdown";
  const message = document.createElement("p");
  message.textContent = kind === "handoff"
    ? "Session handoff is missing or invalid. Restart into-md ui."
    : "The local console could not start. Restart into-md ui.";
  main.append(heading, message);
  root.replaceChildren(main);
  main.focus();
}

const session = takeSession(window.location, window.history);
if (session === null) {
  renderStartupFailure("handoff");
} else {
  // This absolute, content-addressed module path is replaced by the deterministic build.
  void import(INTO_MD_APP_MODULE)
    .then(({ startConsole }: typeof import("./main")) => startConsole(session))
    .catch(() => renderStartupFailure("startup"));
}

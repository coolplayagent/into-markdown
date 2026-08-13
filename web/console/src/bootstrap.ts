import { takeSession } from "./session";
declare const INTO_MD_APP_MODULE: string;

function renderHandoffError(): void {
  const root = document.getElementById("app");
  if (!root) return;
  const main = document.createElement("main");
  main.className = "handoff-error";
  main.tabIndex = -1;
  const heading = document.createElement("h1");
  heading.textContent = "into-markdown";
  const message = document.createElement("p");
  message.textContent = "Session handoff is missing or invalid. Restart into-md ui.";
  main.append(heading, message);
  root.replaceChildren(main);
  main.focus();
}

const session = takeSession(window.location, window.history);
if (session === null) {
  renderHandoffError();
} else {
  // This absolute, content-addressed module path is replaced by the deterministic build.
  void import(INTO_MD_APP_MODULE)
    .then(({ startConsole }: typeof import("./main")) => startConsole(session))
    .catch(() => renderHandoffError());
}

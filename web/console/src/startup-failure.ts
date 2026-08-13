export type StartupFailure = "handoff" | "startup";

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

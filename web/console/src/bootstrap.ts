import { takeSession } from "./session";
import { renderStartupFailure } from "./startup-failure";
declare const INTO_MD_APP_MODULE: string;

const session = takeSession(window.location, window.history);
if (session === null) {
  renderStartupFailure("handoff");
} else {
  // This absolute, content-addressed module path is replaced by the deterministic build.
  void import(INTO_MD_APP_MODULE)
    .then(({ startConsole }: typeof import("./main")) => startConsole(session))
    .catch(() => renderStartupFailure("startup"));
}

import { Component, type ErrorInfo, type ReactNode } from "react";
import type { MessageKey } from "./i18n";

interface Props { children: ReactNode; t(key: MessageKey): string }
interface State { failed: boolean }

export class ErrorBoundary extends Component<Props, State> {
  override state: State = { failed: false };

  static getDerivedStateFromError(): State {
    return { failed: true };
  }

  override componentDidCatch(_error: Error, _info: ErrorInfo): void {
    // Intentionally silent: UI errors can include untrusted data and this local server has no telemetry.
  }

  override render(): ReactNode {
    if (!this.state.failed) return this.props.children;
    return (
      <main className="center-card" id="main" tabIndex={-1}>
        <section className="card" role="alert">
          <h1>{this.props.t("unexpectedTitle")}</h1>
          <p>{this.props.t("unexpectedDetail")}</p>
          <button type="button" onClick={() => window.location.reload()}>{this.props.t("reload")}</button>
        </section>
      </main>
    );
  }
}

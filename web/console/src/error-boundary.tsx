import { Component, createRef, type ErrorInfo, type ReactNode } from "react";

interface Props { children: ReactNode }
interface State { failed: boolean }

export class ErrorBoundary extends Component<Props, State> {
  override state: State = { failed: false };
  private readonly heading = createRef<HTMLHeadingElement>();

  static getDerivedStateFromError(): State {
    return { failed: true };
  }

  override componentDidCatch(_error: Error, _info: ErrorInfo): void {
    // Intentionally silent: UI errors can include untrusted data and this local server has no telemetry.
    queueMicrotask(() => this.heading.current?.focus());
  }

  override render(): ReactNode {
    if (!this.state.failed) return this.props.children;
    const chinese = document.documentElement.lang === "zh-CN";
    return (
      <main className="center-card" id="main" tabIndex={-1}>
        <section className="card" role="alert">
          <h1 ref={this.heading} tabIndex={-1}>{chinese ? "页面遇到问题" : "The page encountered a problem"}</h1>
          <p>{chinese ? "页面已安全停止。你可以重新加载控制台。" : "The page stopped safely. You can reload the console."}</p>
          <button type="button" onClick={() => window.location.reload()}>{chinese ? "重新加载" : "Reload"}</button>
        </section>
      </main>
    );
  }
}

import assert from "node:assert/strict";
import test from "node:test";
import { Window } from "happy-dom";
import { createElement } from "react";
import { createRoot } from "react-dom/client";
import { createApiClient, ApiError } from "../src/api";
import { App } from "../src/app";
import { ErrorBoundary } from "../src/error-boundary";
import { takeSession } from "../src/session";
import styles from "../src/styles.css";

const token = "A".repeat(43);

function installWindow(languages = ["en"]): Window {
  const window = new Window({ url: "http://127.0.0.1:1/status" });
  Object.defineProperty(window.navigator, "languages", { value: languages, configurable: true });
  window.document.head.innerHTML = "<title>into-markdown</title>";
  window.document.body.innerHTML = '<div id="app"></div>';
  for (const [name, value] of Object.entries({
    window, document: window.document, navigator: window.navigator,
    history: window.history, location: window.location, Node: window.Node,
    Element: window.Element, HTMLElement: window.HTMLElement,
  })) Object.defineProperty(globalThis, name, { value, writable: true, configurable: true });
  return window;
}

function waitFor(predicate: () => boolean, timeout = 1_000): Promise<void> {
  const started = Date.now();
  return new Promise((resolvePromise, reject) => {
    const check = () => {
      if (predicate()) resolvePromise();
      else if (Date.now() - started > timeout) reject(new Error("DOM condition timed out"));
      else setTimeout(check, 5);
    };
    check();
  });
}

const availableApi = {
  async status() {
    return {
      schemaVersion: 1 as const,
      localApi: { available: true, code: "available", detail: "ok" },
      documentConsole: { available: false, code: "componentUnavailable", detail: "not installed" },
    };
  },
};

test("session handoff clears every fragment before returning the in-memory token", () => {
  for (const hash of [
    `#into-md-session=${token}`,
    "",
    "#into-md-session=short",
    `#into-md-session=${token}&next=evil`,
    `#other=${token}`,
  ]) {
    const calls: unknown[][] = [];
    const session = takeSession(
      { hash, pathname: "/status", search: "?language=en" },
      { replaceState: (...args: unknown[]) => { calls.push(args); } },
    );
    assert.deepEqual(calls, [[null, "", "/status?language=en"]]);
    assert.equal(session, hash === `#into-md-session=${token}` ? token : null);
  }
});

test("API client sends only the strict POST contract and validates bounded DTOs", async () => {
  let captured: [RequestInfo | URL, RequestInit | undefined] | undefined;
  const client = createApiClient(token, async (input, init) => {
    captured = [input, init];
    return new Response(JSON.stringify({
      schemaVersion: 1,
      localApi: { available: true, code: "available", detail: "ok" },
      documentConsole: { available: false, code: "componentUnavailable", detail: "not installed" },
    }), { headers: { "content-type": "application/json" } });
  });
  assert.equal((await client.status()).localApi.available, true);
  assert.equal(captured?.[0], "/api/status");
  assert.deepEqual(captured?.[1], {
    method: "POST",
    headers: { "X-Into-Md-Session": token },
    body: null,
    cache: "no-store",
    credentials: "omit",
    redirect: "error",
    referrerPolicy: "no-referrer",
  });
});

test("API client rejects malicious and oversized responses without reflecting secrets", async () => {
  for (const response of [
    new Response("<script>bad()</script>", { headers: { "content-type": "text/html" } }),
    new Response(JSON.stringify({ schemaVersion: 2 }), { headers: { "content-type": "application/json" } }),
    new Response("x".repeat(65 * 1024), { headers: { "content-type": "application/json" } }),
  ]) {
    const client = createApiClient(token, async () => response);
    await assert.rejects(client.status(), (error: unknown) => {
      assert.ok(error instanceof ApiError);
      assert.equal(error.message.includes(token), false);
      return true;
    });
  }
});

test("shell primitives expose keyboard focus and language-safe DOM behavior", () => {
  const window = new Window({ url: "http://127.0.0.1:1/status" });
  window.document.body.innerHTML = '<a class="skip-link" href="#main">Skip</a><main id="main" tabindex="-1"><h1>Status</h1></main>';
  const link = window.document.querySelector<HTMLAnchorElement>("a")!;
  const main = window.document.querySelector<HTMLElement>("main")!;
  link.focus();
  assert.equal(window.document.activeElement, link);
  main.focus();
  assert.equal(window.document.activeElement, main);
  assert.equal(main.getAttribute("tabindex"), "-1");
});

function luminance(hex: string): number {
  const channels = hex.slice(1).match(/.{2}/g)!.map((part) => Number.parseInt(part, 16) / 255);
  const [red, green, blue] = channels.map((value) => value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4);
  return 0.2126 * red! + 0.7152 * green! + 0.0722 * blue!;
}

function contrast(left: string, right: string): number {
  const [bright, dark] = [luminance(left), luminance(right)].sort((a, b) => b - a);
  return (bright! + 0.05) / (dark! + 0.05);
}

test("checked CSS color tokens meet WCAG AA normal-text contrast", () => {
  const light = styles.match(/--surface:\s*(#[0-9a-f]{6});[\s\S]*?--text:\s*(#[0-9a-f]{6});[\s\S]*?--muted:\s*(#[0-9a-f]{6});/i);
  const dark = styles.match(/:root\[data-theme="dark"\][\s\S]*?--surface:\s*(#[0-9a-f]{6});[\s\S]*?--text:\s*(#[0-9a-f]{6});[\s\S]*?--muted:\s*(#[0-9a-f]{6});/i);
  assert.ok(light && dark);
  for (const tokens of [light, dark]) {
    assert.ok(contrast(tokens[1]!, tokens[2]!) >= 4.5);
    assert.ok(contrast(tokens[1]!, tokens[3]!) >= 4.5);
  }
  assert.match(styles, /@media \(max-width: 44rem\)/);
  assert.match(styles, /@media \(prefers-reduced-motion: reduce\)/);
  assert.match(styles, /:focus-visible/);
});

test("real App mount synchronizes language without stealing preference focus", async () => {
  const window = installWindow(["zh-CN", "en"]);
  const root = createRoot(window.document.getElementById("app")!);
  root.render(createElement(App, { api: availableApi }));
  await waitFor(() => window.document.body.textContent.includes("本地 API 可用"));
  assert.equal(window.document.documentElement.lang, "zh-CN");
  assert.equal(window.document.documentElement.dir, "ltr");
  const language = window.document.querySelector<HTMLSelectElement>("select")!;
  language.focus();
  language.value = "en";
  language.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitFor(() => window.document.documentElement.lang === "en");
  assert.equal(window.document.activeElement, language);
  assert.match(window.document.title, /Service status/);
  root.unmount();
});

test("real mounted App has no axe violations; geometry-incomplete rules are not treated as coverage", async () => {
  const window = installWindow();
  const root = createRoot(window.document.getElementById("app")!);
  root.render(createElement(App, { api: availableApi }));
  await waitFor(() => window.document.body.textContent.includes("Local API available"));
  const axe = (await import("axe-core")).default;
  const result = await axe.run(window.document);
  assert.deepEqual(result.violations.map((violation) => violation.id), []);
  const incomplete = new Set(result.incomplete.map((item) => item.id));
  if (incomplete.has("color-contrast")) {
    assert.equal(result.passes.some((item) => item.id === "color-contrast"), false);
  }
  root.unmount();
});

test("API rejection renders a recoverable status error rather than the error boundary", async () => {
  const window = installWindow();
  const root = createRoot(window.document.getElementById("app")!);
  root.render(createElement(App, { api: { status: async () => { throw new ApiError("unreachable"); } } }));
  await waitFor(() => window.document.body.textContent.includes("Could not read service status"));
  assert.ok(window.document.querySelector('[role="alert"]'));
  assert.equal(window.document.body.textContent.includes("The page encountered a problem"), false);
  assert.ok([...window.document.querySelectorAll("button")].some((button) => button.textContent === "Retry"));
  root.unmount();
});

test("ErrorBoundary contains provider render errors and focuses its fallback heading", async () => {
  const window = installWindow();
  const root = createRoot(window.document.getElementById("app")!, { onCaughtError: () => undefined });
  function FailingProvider(): never { throw new Error("untrusted provider failure"); }
  root.render(createElement(ErrorBoundary, null, createElement(FailingProvider)));
  await waitFor(() => window.document.body.textContent.includes("The page encountered a problem"));
  const heading = window.document.querySelector("h1")!;
  assert.equal(window.document.activeElement, heading);
  assert.equal(heading.tabIndex, -1);
  assert.ok([...window.document.querySelectorAll("button")].some((button) => button.textContent === "Reload"));
  root.unmount();
});

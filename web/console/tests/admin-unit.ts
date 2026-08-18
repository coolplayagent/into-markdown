import assert from "node:assert/strict";
import test, { afterEach } from "node:test";
import { Window } from "happy-dom";
import { createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { App } from "../src/app";
import { ApiError, createApiClient, parseAdminSnapshot } from "../src/api";
import type { AdminAction, AdminSnapshot, ApiClient } from "../src/api";

const token = "A".repeat(43);
let activeWindow: Window | null = null; let activeRoot: Root | null = null;
const globals = ["window", "document", "navigator", "history", "location", "Node", "Element", "HTMLElement", "File"] as const;

function install(path: string): Window {
  const window = new Window({ url: `http://127.0.0.1:1${path}` }); activeWindow = window;
  Object.defineProperty(window.navigator, "languages", { value: ["en"], configurable: true });
  window.document.body.innerHTML = '<div id="app"></div>';
  for (const [name, value] of Object.entries({ window, document: window.document, navigator: window.navigator, history: window.history, location: window.location, Node: window.Node, Element: window.Element, HTMLElement: window.HTMLElement, File: window.File })) Object.defineProperty(globalThis, name, { value, writable: true, configurable: true });
  return window;
}
afterEach(async () => {
  activeRoot?.unmount(); activeRoot = null;
  await new Promise((resolve) => setTimeout(resolve, 0));
  activeWindow?.close(); activeWindow = null;
  for (const name of globals) Reflect.deleteProperty(globalThis, name);
});
function waitFor(predicate: () => boolean): Promise<void> { const started = Date.now(); return new Promise((resolve, reject) => { const poll = () => predicate() ? resolve() : Date.now() - started > 1_500 ? reject(new Error("DOM timeout")) : setTimeout(poll, 5); poll(); }); }

const snapshot: AdminSnapshot = {
  schemaVersion: 1,
  formats: [{ format: "pdf", family: "document", status: "available", source: "core", extensions: ["pdf"], runtimeComponent: "pdfium", installHint: "install runtime" }],
  models: { defaultBundle: "ocr", entries: [{ bundle: { id: "ocr", availability: "available" }, status: { id: "ocr", state: "installed", ownership: "writable" } }] },
  providers: [{ name: "vision", providerType: "openai-compatible", baseUrl: "https://example.com/v1", model: "vision", apiKeyEnv: "VISION_KEY", environmentSet: true, capabilities: ["image-description"], default: true }],
  plugins: [{ id: "local", source: "file:///redacted/plugin", protocol: "process-v1", enabled: true }],
  configuration: { schema_version: 1, providers: { vision: { api_key_env: "VISION_KEY" } } }, profiles: ["safe"],
  doctor: [{ id: "networkProbe", status: "skipped", detail: "offline by default" }],
};

function api(actions: AdminAction[] = []): ApiClient {
  const noop = async () => { throw new Error("not used"); };
  return { status: noop, listTasks: async () => [], getTask: noop, upload: noop, cancel: noop, watchTask: noop, preview: noop, download: noop,
    admin: async () => snapshot, adminAction: async (action) => { actions.push(action); return snapshot; } } as unknown as ApiClient;
}

test("admin DTO is bounded and never needs credential values", () => {
  assert.equal(parseAdminSnapshot(snapshot).providers[0]!.environmentSet, true);
  assert.equal("apiKey" in snapshot.providers[0]!, false);
  assert.throws(() => parseAdminSnapshot({ ...snapshot, providers: Array.from({ length: 129 }, () => snapshot.providers[0]) }), ApiError);
});

test("admin API uses the authenticated same-origin contract and stable error code", async () => {
  const calls: Array<[RequestInfo | URL, RequestInit | undefined]> = [];
  const client = createApiClient(token, async (input, init) => { calls.push([input, init]); return new Response(JSON.stringify(snapshot), { headers: { "content-type": "application/json" } }); });
  await client.admin(); assert.equal(calls[0]![0], "/api/admin");
  assert.deepEqual(calls[0]![1]?.headers, { "X-Into-Md-Session": token });
  const denied = createApiClient(token, async () => new Response('{"schemaVersion":1,"code":"networkAuthorizationRequired"}', { status: 403, headers: { "content-type": "application/json" } }));
  await assert.rejects(denied.adminAction({ schemaVersion: 1, action: "provider.test" }), (error: unknown) => error instanceof ApiError && error.code === "networkAuthorizationRequired");
});

test("administration pages are accessible, responsive, recoverable and require one-time grants", async () => {
  const window = install("/models"); const actions: AdminAction[] = [];
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: api(actions) }));
  await waitFor(() => window.document.body.textContent.includes("ocr"));
  const installButton = [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Install")!;
  assert.equal(installButton.disabled, true);
  const grants = [...window.document.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')]; grants[0]!.click();
  assert.equal(installButton.disabled, false); installButton.click(); await waitFor(() => actions.length === 1);
  assert.equal(actions[0]!.authorizeNetwork, true);
  await waitFor(() => !grants[0]!.checked);
  assert.equal(grants[0]!.checked, false, "grant resets after one operation");
  const axe = (await import("axe-core")).default; const result = await axe.run(window.document); assert.deepEqual(result.violations.map((item) => item.id), []);
  window.innerWidth = 375; window.dispatchEvent(new window.Event("resize"));
  assert.equal(window.document.querySelector("main") !== null, true);
});

test("failed initial load exposes an alert and retry recovery", async () => {
  const window = install("/doctor"); let calls = 0;
  const failed = api(); failed.admin = async () => { calls += 1; if (calls === 1) throw new ApiError("unreachable"); return snapshot; };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: failed }));
  await waitFor(() => window.document.querySelector('[role="alert"]') !== null);
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Retry")!.click();
  await waitFor(() => window.document.body.textContent.includes("networkProbe")); assert.equal(calls, 2);
});

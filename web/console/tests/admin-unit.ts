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
  configurationReadOnly: false,
  formats: [{ format: "pdf", family: "document", status: "available", source: "core", extensions: ["pdf"], runtimeComponent: "pdfium", installHint: "install runtime" }],
  models: { defaultBundle: "ocr", entries: [{ bundle: { id: "ocr", availability: "available" }, status: { id: "ocr", state: "installed", ownership: "writable" } }] },
  providers: [{ name: "vision", scope: "effective", actionScope: "project", providerType: "openai-compatible", baseUrl: "https://example.com/v1", model: "vision", apiKeyEnv: "VISION_KEY", environmentSet: true, capabilities: ["image-description"], default: true, effective: true }],
  plugins: [{ id: "local", scope: "effective", actionScope: "project", packageScope: "project", source: "file:///redacted/plugin", sha256: "1".repeat(64), protocol: "process-v1", enabled: true, effective: true, verification: "verified", version: "1.0.0", signingKeyId: "release-key", signingKeySha256: "2".repeat(64), target: "x86_64-pc-windows-msvc" }],
  configuration: { schema_version: 1, providers: { vision: { api_key_env: "VISION_KEY" } } }, profiles: [{ name: "safe", scope: "project", effective: true, active: false }],
  doctor: [{ id: "networkProbe", status: "skipped", detail: "offline by default" }],
};

function api(actions: AdminAction[] = [], value: AdminSnapshot = snapshot): ApiClient {
  const noop = async () => { throw new Error("not used"); };
  return { status: noop, listTasks: async () => [], getTask: noop, upload: noop, cancel: noop, watchTask: noop, preview: noop, download: noop,
    admin: async () => value, adminGrant: async () => "G".repeat(43), adminAction: async (action) => { actions.push(action); return {}; } } as unknown as ApiClient;
}

test("admin DTO is bounded and never needs credential values", () => {
  assert.equal(parseAdminSnapshot(snapshot).providers[0]!.environmentSet, true);
  assert.equal("apiKey" in snapshot.providers[0]!, false);
  assert.throws(() => parseAdminSnapshot({ ...snapshot, providers: Array.from({ length: 129 }, () => snapshot.providers[0]) }), ApiError);
  const plugin = snapshot.plugins[0]!;
  for (const mutation of [
    { ...plugin, effective: undefined },
    { ...plugin, effective: false },
    { ...plugin, shadowedBy: "global", effective: false },
    { ...plugin, sha256: "A".repeat(64) },
    { ...plugin, signingKeySha256: "0".repeat(63) },
    { ...plugin, signingKeyId: "bad key" },
    { ...plugin, target: "bad target" },
    { ...plugin, version: "x".repeat(129) },
  ]) assert.throws(() => parseAdminSnapshot({ ...snapshot, plugins: [mutation] }), ApiError);
  for (const mutation of [
    { ...snapshot.formats[0]!, source: "unknown" },
    { ...snapshot.formats[0]!, status: "ready" },
    { ...snapshot.formats[0]!, extensions: ["bad/path"] },
    { ...snapshot.formats[0]!, runtimeComponent: "x".repeat(129) },
  ]) assert.throws(() => parseAdminSnapshot({ ...snapshot, formats: [mutation] }), ApiError);
  for (const mutation of [
    { ...snapshot.models.entries[0]!, bundle: { id: "ocr", availability: "maybe" } },
    { ...snapshot.models.entries[0]!, status: { state: "x".repeat(65) } },
  ]) assert.throws(() => parseAdminSnapshot({ ...snapshot, models: { ...snapshot.models, entries: [mutation] } }), ApiError);
  for (const mutation of [
    { ...snapshot.providers[0]!, providerType: "shell" },
    { ...snapshot.providers[0]!, scope: "machine" },
    { ...snapshot.providers[0]!, effective: false },
    { ...snapshot.providers[0]!, model: undefined },
    { ...snapshot.providers[0]!, apiKeyEnv: "SECRET VALUE" },
    { ...snapshot.providers[0]!, capabilities: ["BAD"] },
    { ...snapshot.providers[0]!, default: undefined },
  ]) assert.throws(() => parseAdminSnapshot({ ...snapshot, providers: [mutation] }), ApiError);
  for (const mutation of [
    { ...snapshot.profiles[0]!, scope: "machine" },
    { ...snapshot.profiles[0]!, effective: false },
    { ...snapshot.profiles[0]!, name: "x".repeat(129) },
  ]) assert.throws(() => parseAdminSnapshot({ ...snapshot, profiles: [mutation] }), ApiError);
  const readOnly: AdminSnapshot = {
    ...snapshot,
    configurationReadOnly: true,
    providers: [{ ...snapshot.providers[0]!, actionScope: undefined }],
    plugins: [{ ...snapshot.plugins[0]!, actionScope: undefined, packageScope: undefined }],
    profiles: [{ ...snapshot.profiles[0]!, scope: "effective" }],
  };
  assert.equal(parseAdminSnapshot(readOnly).configurationReadOnly, true);
  for (const mutation of [
    { ...readOnly, providers: snapshot.providers },
    { ...readOnly, plugins: snapshot.plugins },
    { ...readOnly, profiles: snapshot.profiles },
  ]) assert.throws(() => parseAdminSnapshot(mutation), ApiError);
  const partial = {
    ...snapshot,
    providers: [
      { name: "vision", scope: "global", actionScope: "global", providerType: "openai-compatible", capabilities: [], default: false, effective: false, shadowedBy: "effective" },
      snapshot.providers[0]!,
    ],
    plugins: [
      { id: "local", scope: "project", actionScope: "project", packageScope: "global", protocol: "process-v1", enabled: true, effective: false, shadowedBy: "effective" },
      snapshot.plugins[0]!,
    ],
  };
  assert.equal(parseAdminSnapshot(partial).providers.length, 2);
  assert.doesNotThrow(() => parseAdminSnapshot({ ...snapshot, operationResult: { kind: "config", operation: "showResolved", value: { cli: {} } } }));
  assert.throws(() => parseAdminSnapshot({ ...snapshot, operationResult: { kind: "config", operation: "showSecret", value: {} } }), ApiError);
  assert.throws(() => parseAdminSnapshot({ ...snapshot, operationResult: { kind: "detection", sourceSize: 1, candidates: [{ format: "pdf", confidence: 2, explicit: false, detectorId: "x", reason: "x", diagnostics: [] }] } }), ApiError);
});

test("admin API uses the authenticated same-origin contract and stable error code", async () => {
  const calls: Array<[RequestInfo | URL, RequestInit | undefined]> = [];
  const client = createApiClient(token, async (input, init) => { calls.push([input, init]); return new Response(JSON.stringify(snapshot), { headers: { "content-type": "application/json" } }); });
  await client.admin(); assert.equal(calls[0]![0], "/api/admin");
  assert.deepEqual(calls[0]![1]?.headers, { "X-Into-Md-Session": token });
  const denied = createApiClient(token, async () => new Response('{"schemaVersion":1,"code":"networkAuthorizationRequired"}', { status: 403, headers: { "content-type": "application/json" } }));
  await assert.rejects(denied.adminAction({ schemaVersion: 1, action: "provider.test" }), (error: unknown) => error instanceof ApiError && error.code === "networkAuthorizationRequired");
});

test("administration responses share one exact one-MiB wire limit", async () => {
  const value: Record<string, string> = {};
  for (let index = 0; index < 255; index += 1) value[`p${index}`] = "x".repeat(4096);
  value.last = "";
  const response = { schemaVersion: 1, code: "ok", operationResult: { kind: "profile", name: "x", value } };
  const base = JSON.stringify(response);
  value.last = "x".repeat(1024 * 1024 - base.length);
  const body = JSON.stringify(response);
  assert.equal(new TextEncoder().encode(body).byteLength, 1024 * 1024);
  const accepted = createApiClient(token, async () => new Response(body, { headers: { "content-type": "application/json", "content-length": String(body.length) } }));
  await accepted.adminAction({ schemaVersion: 1, action: "profile.show" });
  const rejected = createApiClient(token, async () => new Response(`${body} `, { headers: { "content-type": "application/json", "content-length": String(body.length + 1) } }));
  await assert.rejects(rejected.adminAction({ schemaVersion: 1, action: "profile.show" }), (error: unknown) => error instanceof ApiError && error.code === "responseTooLarge");
});

test("administration pages are accessible, responsive, recoverable and require one-time grants", async () => {
  const window = install("/admin/models"); const actions: AdminAction[] = [];
  const missingModel: AdminSnapshot = { ...snapshot, models: { ...snapshot.models, entries: [{ bundle: { id: "whisper-small-multilingual", availability: "planned" }, status: { state: "missing" } }] } };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: api(actions, missingModel) }));
  await waitFor(() => window.document.body.textContent.includes("Multilingual transcription"));
  const installButton = [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Install")!;
  assert.equal(installButton.disabled, false); installButton.click(); installButton.click(); await waitFor(() => actions.length === 1);
  assert.equal(actions[0]!.authorizeNetwork, true);
  assert.equal(actions[0]!.authorizeDangerous, false);
  assert.equal(actions[0]!.allowPrivateNetwork, false);
  assert.equal(actions[0]!.insecure, false);
  assert.equal(window.document.body.textContent.includes("manifest"), false);
  assert.equal(window.document.body.textContent.includes("invocation capabilities"), false);
  const axe = (await import("axe-core")).default; const result = await axe.run(window.document); assert.deepEqual(result.violations.map((item) => item.id), []);
  window.innerWidth = 375; window.dispatchEvent(new window.Event("resize"));
  assert.equal(window.document.querySelector("main") !== null, true);
});

test("failed initial load exposes an alert and retry recovery", async () => {
  const window = install("/admin/doctor"); let calls = 0;
  const failed = api(); failed.admin = async () => { calls += 1; if (calls === 1) throw new ApiError("unreachable"); return snapshot; };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: failed }));
  await waitFor(() => window.document.querySelector('[role="alert"]') !== null);
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Reload")!.click();
  await waitFor(() => window.document.body.textContent.includes("Diagnostics")); assert.equal(calls, 2);
});

test("partial records expose exact-layer mutation while read-only authority stays disabled", async () => {
  const partial: AdminSnapshot = {
    ...snapshot,
    providers: [
      { name: "vision", scope: "global", actionScope: "global", model: "base", capabilities: [], default: false, effective: false, shadowedBy: "effective" },
      { name: "vision", scope: "project", actionScope: "project", model: "override", capabilities: [], default: false, effective: false, shadowedBy: "effective" },
      snapshot.providers[0]!,
    ],
    plugins: [
      { id: "local", scope: "global", actionScope: "global", packageScope: "global", protocol: "process-v1", enabled: false, effective: false, shadowedBy: "effective" },
      { id: "local", scope: "project", actionScope: "project", packageScope: "global", protocol: "process-v1", enabled: true, effective: false, shadowedBy: "effective" },
      snapshot.plugins[0]!,
    ],
  };
  const window = install("/admin/providers");
  Object.defineProperty(window.navigator, "languages", { value: ["zh-CN"], configurable: true });
  activeRoot = createRoot(window.document.getElementById("app")!);
  activeRoot.render(createElement(App, { api: api([], parseAdminSnapshot(partial)) }));
  await waitFor(() => window.document.body.textContent.includes("https://example.com/v1"));
  assert.equal(window.document.querySelectorAll(".admin-entity-card button.danger").length, 2);

  activeRoot.unmount(); activeRoot = null; window.history.pushState({}, "", "/admin/plugins");
  const readOnly: AdminSnapshot = {
    ...snapshot,
    configurationReadOnly: true,
    providers: [{ ...snapshot.providers[0]!, actionScope: undefined }],
    plugins: [{ ...snapshot.plugins[0]!, actionScope: undefined, packageScope: undefined }],
    profiles: [{ ...snapshot.profiles[0]!, scope: "effective" }],
  };
  activeRoot = createRoot(window.document.getElementById("app")!);
  activeRoot.render(createElement(App, { api: api([], readOnly) }));
  await waitFor(() => window.document.body.textContent.includes("当前只能查看"));
  assert.equal([...window.document.querySelectorAll("button")].find((button) => button.textContent?.includes("安装扩展插件"))?.disabled, true);
  assert.equal([...window.document.querySelectorAll("button")].find((button) => button.textContent === "验证")?.disabled, true);
});

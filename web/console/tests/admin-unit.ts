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
  capabilities: [
    { id: "legacy-office", status: "not-installed", localStatus: "not-installed", currentSource: "off", sources: ["plugin:official.legacy-office.libreoffice/legacy-office", "off"] },
    { id: "ocr", status: "ready", localStatus: "ready", currentSource: "plugin:official.ocr.ppocrv6/ocr", sources: ["plugin:official.ocr.ppocrv6/ocr", "off"], version: "1.0.0", localVersion: "1.0.0" },
    { id: "transcription", status: "ready", localStatus: "ready", currentSource: "plugin:official.media.whisper/transcription", sources: ["plugin:official.media.whisper/transcription", "off"], version: "1.0.0", localVersion: "1.0.0" },
    { id: "diarization", status: "ready", localStatus: "ready", currentSource: "plugin:official.media.whisper/diarization", sources: ["plugin:official.media.whisper/diarization", "off"], version: "1.0.0", localVersion: "1.0.0" },
  ],
  providers: [{ name: "vision", scope: "effective", actionScope: "project", providerType: "openai-compatible", baseUrl: "https://example.com/v1", model: "vision", models: {}, apiKeyEnv: "VISION_KEY", environmentSet: true, capabilities: ["image-description"], allowedHosts: ["example.com"], allowPrivateNetwork: false, default: true, effective: true }],
  plugins: [{ id: "local", scope: "effective", actionScope: "project", packageScope: "project", source: "file:///redacted/plugin", sha256: "1".repeat(64), protocol: "process-v1", enabled: true, effective: true, verification: "verified", version: "1.0.0", signingKeyId: "release-key", signingKeySha256: "2".repeat(64), target: "x86_64-pc-windows-msvc" }],
  configuration: { schema_version: 1, providers: { vision: { api_key_env: "VISION_KEY" } } }, profiles: [{ name: "safe", scope: "project", effective: true, active: false }],
  doctor: [{ id: "networkProbe", status: "skipped", detail: "offline by default" }],
};

function api(actions: AdminAction[] = [], value: AdminSnapshot = snapshot): ApiClient {
  const noop = async () => { throw new Error("not used"); };
  return { status: noop, capabilitySnapshot: async () => ({ schemaVersion: 2, generation: 1, checking: false, capabilities: value.capabilities.map((item) => ({ ...item, name: item.id, currentSourceName: item.currentSource })) }), listTasks: async () => [], getTask: noop, upload: noop, cancel: noop, watchTask: noop, preview: noop, download: noop,
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
    { ...snapshot.capabilities[0]!, id: "unknown" },
    { ...snapshot.capabilities[0]!, status: "installed" },
    { ...snapshot.capabilities[0]!, currentSource: "core:ocr" },
    { ...snapshot.capabilities[0]!, sources: ["plugin:../escape/ocr"] },
  ]) assert.throws(() => parseAdminSnapshot({ ...snapshot, capabilities: [mutation, ...snapshot.capabilities.slice(1)] }), ApiError);
  for (const mutation of [
    { ...snapshot.providers[0]!, providerType: "shell" },
    { ...snapshot.providers[0]!, scope: "machine" },
    { ...snapshot.providers[0]!, effective: false },
    { ...snapshot.providers[0]!, model: undefined },
    { ...snapshot.providers[0]!, apiKeyEnv: "SECRET VALUE" },
    { ...snapshot.providers[0]!, capabilities: ["BAD"] },
    { ...snapshot.providers[0]!, allowedHosts: ["bad host"] },
    { ...snapshot.providers[0]!, allowPrivateNetwork: undefined },
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
      { name: "vision", scope: "global", actionScope: "global", providerType: "openai-compatible", models: {}, capabilities: [], allowedHosts: [], allowPrivateNetwork: false, default: false, effective: false, shadowedBy: "effective" },
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

test("plugin picker uploads the selected package through the authenticated staging endpoint", async () => {
  const window = install("/admin/plugins");
  const calls: Array<[RequestInfo | URL, RequestInit | undefined]> = [];
  const response = { schemaVersion: 1, source: "/private/plugin-staging/upload.imp", filename: "本地 OCR.imp", byteLen: 3 };
  const client = createApiClient(token, async (input, init) => {
    calls.push([input, init]);
    return new Response(JSON.stringify(response), { headers: { "content-type": "application/json" } });
  });
  const file = new window.File(["imp"], "本地 OCR.imp", { type: "application/octet-stream" });
  assert.deepEqual(await client.stagePluginPackage!(file as unknown as File), response);
  assert.equal(calls.length, 1);
  assert.equal(calls[0]![0], "/api/admin/plugin-package");
  assert.equal(calls[0]![1]?.method, "POST");
  assert.equal(calls[0]![1]?.body, file);
  assert.deepEqual(calls[0]![1]?.headers, {
    "X-Into-Md-Session": token,
    "Content-Type": "application/octet-stream",
    "X-Into-Md-Plugin-Filename-B64": "5pys5ZywIE9DUi5pbXA",
  });
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
  const window = install("/admin/capabilities"); const actions: AdminAction[] = [];
  const missingOcr: AdminSnapshot = { ...snapshot, capabilities: snapshot.capabilities.map((item) => item.id === "ocr" ? { ...item, status: "not-installed", localStatus: "not-installed", currentSource: "off", sources: ["plugin:official.ocr.ppocrv6/ocr", "off"], version: undefined, localVersion: undefined } : item) };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: api(actions, missingOcr) }));
  await waitFor(() => window.document.body.textContent.includes("Read scanned PDFs and images"));
  const installButton = [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Install")!;
  assert.equal(installButton.disabled, false); installButton.click(); installButton.click(); await waitFor(() => actions.length === 1);
  assert.equal(actions[0]!.authorizeNetwork, true);
  assert.equal(actions[0]!.authorizeDangerous, true);
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

test("preferences render a stable five-section shell while the admin snapshot is delayed", async () => {
  const window = install("/admin/configuration");
  let resolveAdmin!: (value: AdminSnapshot) => void;
  const delayed = api(); delayed.admin = async () => new Promise<AdminSnapshot>((resolve) => { resolveAdmin = resolve; });
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: delayed }));
  await waitFor(() => window.document.body.textContent.includes("Documents and recognition"));
  for (const title of ["Documents and recognition", "Output", "Speech transcription", "Performance", "Privacy and network"]) assert.equal(window.document.body.textContent.includes(title), true);
  assert.equal(window.document.querySelector('[aria-busy="true"]') !== null, true);
  assert.equal([...window.document.querySelectorAll("button")].find((button) => button.textContent === "Save")?.disabled, true);
  await waitFor(() => typeof resolveAdmin === "function");
  resolveAdmin(snapshot);
  await waitFor(() => window.document.querySelector('[aria-busy="true"]') === null);
  assert.equal(window.document.querySelectorAll(".preference-group").length, 5);
});

test("legacy administration URLs redirect into the matching capability context", async () => {
  const window = install("/admin/providers");
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: api() }));
  await waitFor(() => window.document.querySelector('[role="dialog"]') !== null);
  assert.equal(window.location.pathname, "/admin/capabilities");
  assert.equal(window.document.querySelector('[role="dialog"]')?.textContent?.includes("AI services"), true);
  assert.equal(window.document.querySelectorAll(".admin-tabs a").length, 3);
});

test("nested administration dialogs close one layer at a time and restore focus", async () => {
  const window = install("/admin/capabilities");
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: api() }));
  await waitFor(() => window.document.body.textContent.includes("Capabilities & sources"));
  const sourceTrigger = [...window.document.querySelectorAll("button")].find((button) => button.textContent === "AI services")!;
  sourceTrigger.focus(); sourceTrigger.click();
  await waitFor(() => window.document.querySelectorAll('[role="dialog"]').length === 1);
  const providerTrigger = [...window.document.querySelectorAll<HTMLElement>('[role="dialog"] button')].find((button) => button.textContent === "Connect AI service")!;
  providerTrigger.focus(); providerTrigger.click();
  await waitFor(() => window.document.querySelectorAll('[role="dialog"]').length === 2);
  window.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  await waitFor(() => window.document.querySelectorAll('[role="dialog"]').length === 1 && window.document.activeElement === providerTrigger);
  window.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  await waitFor(() => window.document.querySelectorAll('[role="dialog"]').length === 0 && window.document.activeElement === sourceTrigger);
});

test("diagnostics group failures by remediation target rather than check id", async () => {
  const window = install("/admin/doctor");
  const grouped: AdminSnapshot = { ...snapshot, doctor: [
    { id: "runtime.asr", status: "failed", detail: "transcriber unavailable" },
    { id: "runtime.diarization", status: "failed", detail: "speaker resources unavailable" },
    { id: "runtime.ocr", status: "failed", detail: "ocr unavailable" },
  ] };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: api([], grouped) }));
  await waitFor(() => window.document.body.textContent.includes("2 need attention"));
  assert.equal(window.document.querySelectorAll(".doctor-card").length, 2);
  const speech = [...window.document.querySelectorAll<HTMLElement>(".doctor-card")].find((card) => card.textContent?.includes("Speech transcription"))!;
  assert.equal(speech.textContent?.includes("Speaker identification"), true);
});

test("provider failures explain the cause beside the provider that triggered them", async () => {
  const window = install("/admin/providers");
  Object.defineProperty(window.navigator, "languages", { value: ["zh-CN"], configurable: true });
  let failureCode = "privateNetworkDenied";
  const failed = api();
  failed.adminAction = async () => { throw new ApiError(failureCode); };
  activeRoot = createRoot(window.document.getElementById("app")!);
  activeRoot.render(createElement(App, { api: failed }));
  await waitFor(() => window.document.body.textContent.includes("AI 服务"));
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "测试连接")!.click();
  await waitFor(() => window.document.body.textContent.includes("连接被安全策略阻止"));
  const feedback = window.document.querySelector(".admin-entity-card .capability-feedback.error");
  assert.match(feedback?.textContent ?? "", /局域网地址/);
  assert.equal(window.document.querySelector(".admin-section-stack > .admin-feedback"), null);
  failureCode = "providerSecretMissing";
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "测试连接")!.click();
  await waitFor(() => window.document.body.textContent.includes("密钥环境变量"));
  assert.equal(window.document.body.textContent.includes("providerSecretMissing"), false);
});

test("provider tests use each persisted network policy and ignore another dialog draft", async () => {
  const providers: AdminSnapshot = {
    ...snapshot,
    providers: [
      { ...snapshot.providers[0]!, name: "alpha", baseUrl: "https://alpha.example/v1", allowedHosts: ["alpha.example"], allowPrivateNetwork: false, default: true },
      { ...snapshot.providers[0]!, name: "beta", baseUrl: "http://127.0.0.1:9443/v1", allowedHosts: ["127.0.0.1"], allowPrivateNetwork: true, default: false },
    ],
  };
  const window = install("/admin/providers"); const actions: AdminAction[] = [];
  activeRoot = createRoot(window.document.getElementById("app")!);
  activeRoot.render(createElement(App, { api: api(actions, providers) }));
  await waitFor(() => window.document.body.textContent.includes("alpha.example"));
  const cards = [...window.document.querySelectorAll<HTMLElement>(".admin-entity-card")];
  const alpha = cards.find((card) => card.textContent?.includes("alpha.example"))!;
  const beta = cards.find((card) => card.textContent?.includes("127.0.0.1"))!;

  [...alpha.querySelectorAll("button")].find((button) => button.textContent === "Edit")!.click();
  await waitFor(() => window.document.querySelectorAll('[role="dialog"]').length === 2);
  const dialog = [...window.document.querySelectorAll<HTMLElement>('[role="dialog"]')].at(-1)!;
  const hostField = [...dialog.querySelectorAll("label")].find((label) => label.textContent?.includes("Allowed hosts"))!;
  const hostInput = hostField.querySelector("input")!;
  hostInput.value = "draft-only.example";
  hostInput.dispatchEvent(new window.Event("input", { bubbles: true }));
  [...dialog.querySelectorAll("button")].find((button) => button.textContent === "Cancel")!.click();
  await waitFor(() => window.document.querySelectorAll('[role="dialog"]').length === 1);

  [...beta.querySelectorAll("button")].find((button) => button.textContent === "Test connection")!.click();
  await waitFor(() => actions.length === 1);
  assert.equal(actions[0]!.action, "provider.test");
  assert.equal(actions[0]!.scope, "project");
  assert.equal(actions[0]!.target, "beta");
  assert.equal(actions[0]!.authorizeNetwork, true);
  assert.equal(actions[0]!.authorizeDangerous, true);
  assert.equal(actions[0]!.authorizationGrant, "G".repeat(43));
  assert.equal("allowHosts" in actions[0]!, false);
  assert.equal("allowPrivateNetwork" in actions[0]!, false);
});

test("partial records expose exact-layer mutation while read-only authority stays disabled", async () => {
  const partial: AdminSnapshot = {
    ...snapshot,
    providers: [
      { name: "vision", scope: "global", actionScope: "global", model: "base", models: {}, capabilities: [], allowedHosts: [], allowPrivateNetwork: false, default: false, effective: false, shadowedBy: "effective" },
      { name: "vision", scope: "project", actionScope: "project", model: "override", models: {}, capabilities: [], allowedHosts: [], allowPrivateNetwork: false, default: false, effective: false, shadowedBy: "effective" },
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
  assert.equal([...window.document.querySelectorAll("button")].find((button) => button.textContent?.includes("安装扩展"))?.disabled, true);
  assert.equal([...window.document.querySelectorAll("button")].find((button) => button.textContent === "验证")?.disabled, true);
});

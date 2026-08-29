import assert from "node:assert/strict";
import test, { afterEach } from "node:test";
import { Window } from "happy-dom";
import { createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { App } from "../src/app";
import { friendlyError } from "../src/admin-page";
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
function waitFor(predicate: () => boolean, label = "DOM timeout"): Promise<void> { const started = Date.now(); return new Promise((resolve, reject) => { const poll = () => predicate() ? resolve() : Date.now() - started > 1_500 ? reject(new Error(label)) : setTimeout(poll, 5); poll(); }); }

const snapshot: AdminSnapshot = {
  schemaVersion: 1,
  configurationReadOnly: false,
  formats: [{ format: "pdf", family: "document", status: "available", source: "core", extensions: ["pdf"], runtimeComponent: "pdfium", installHint: "install runtime" }],
  capabilities: [
    { id: "ocr", status: "ready", localStatus: "ready", currentSource: "core:ocr", sources: ["core:ocr", "off"], version: "1.0.0", localVersion: "1.0.0" },
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
    { ...snapshot.capabilities[0]!, currentSource: "core:media" },
    { ...snapshot.capabilities[0]!, sources: ["plugin:../escape/ocr"] },
  ]) assert.throws(() => parseAdminSnapshot({ ...snapshot, capabilities: [mutation, ...snapshot.capabilities.slice(1)] }), ApiError);
  assert.doesNotThrow(() => parseAdminSnapshot(snapshot));
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
  assert.doesNotThrow(() => parseAdminSnapshot({
    ...snapshot,
    capabilities: snapshot.capabilities.map((item) => item.id === "transcription" ? { ...item, status: "disabled", localStatus: "disabled" } : item),
  }));
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

test("known administration failures explain the next action without a generic incomplete message", () => {
  const codes = [
    "requestFailed", "resourceLimit", "transactionIndeterminate", "notFound", "io", "conflict",
    "hashMismatch", "signature", "componentUnavailable", "invalidPackage", "invalidPluginUrl",
    "networkDenied", "dns", "connect", "tls", "invalidHttp", "networkUnavailable", "pluginDownload",
    "storeChanged", "plaintextSecretRejected", "adminConfigContextReadOnly",
  ];
  for (const code of codes) {
    const message = friendlyError(code, "zh-CN");
    assert.equal(message.includes("操作未完成"), false, `${code} used the old generic message`);
    assert.ok(message.length >= 12, `${code} did not explain a recovery action`);
  }
  assert.match(friendlyError("signature", "zh-CN"), /受信任来源/);
  assert.match(friendlyError("resourceLimit", "zh-CN"), /磁盘或内存/);
  assert.match(friendlyError("connect", "zh-CN"), /地址、端口和服务状态/);
});

test("plugin picker uploads the selected package through the authenticated staging endpoint", async () => {
  const window = install("/admin/plugins");
  const calls: Array<[RequestInfo | URL, RequestInit | undefined]> = [];
  const response = { schemaVersion: 1, source: "/private/plugin-staging/upload.imp", filename: "本地 OCR.imp", byteLen: 3, sha256: "a".repeat(64) };
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

test("administration pages keep built-in OCR free of plugin lifecycle controls", async () => {
  const window = install("/admin/capabilities"); const actions: AdminAction[] = [];
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: api(actions) }));
  await waitFor(() => window.document.body.textContent.includes("Read scanned PDFs and images"));
  const ocrCard = [...window.document.querySelectorAll<HTMLElement>(".capability-row")].find((card) => card.textContent?.includes("Image OCR"))!;
  assert.ok(ocrCard.textContent?.includes("Built-in OCR"));
  assert.equal([...ocrCard.querySelectorAll("button")].some((button) => ["Install", "Repair", "Verify", "Remove"].includes(button.textContent ?? "")), false);
  assert.equal(ocrCard.querySelector(".capability-install-dialog"), null);
  assert.equal(actions.length, 0);
  assert.equal(window.document.body.textContent.includes("manifest"), false);
  assert.equal(window.document.body.textContent.includes("invocation capabilities"), false);
  const axe = (await import("axe-core")).default; const result = await axe.run(window.document); assert.deepEqual(result.violations.map((item) => item.id), []);
  window.innerWidth = 375; window.dispatchEvent(new window.Event("resize"));
  assert.equal(window.document.querySelector("main") !== null, true);
});

test("built-in OCR offers the compatible off route without plugin management", async () => {
  const window = install("/admin/capabilities"); const actions: AdminAction[] = [];
  const client = api(actions);
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: client }));
  await waitFor(() => window.document.body.textContent.includes("Read scanned PDFs and images"));
  const ocrCard = [...window.document.querySelectorAll<HTMLElement>(".capability-row")].find((card) => card.textContent?.includes("Image OCR"))!;
  const select = ocrCard.querySelector<HTMLSelectElement>("select")!;
  assert.deepEqual([...select.options].map((option) => [option.value, option.textContent]), [["core:ocr", "Built-in OCR"], ["off", "Off"]]);
  assert.equal(select.options[1]!.disabled, false);
  assert.equal(actions.length, 0);
  assert.equal([...ocrCard.querySelectorAll("button")].some((button) => ["Install", "Repair", "Verify", "Remove"].includes(button.textContent ?? "")), false);
});

test("speech install chooser derives official trust and keeps package errors beside the picker", async () => {
  const window = install("/admin/capabilities"); const actions: AdminAction[] = [];
  const missingSpeech: AdminSnapshot = { ...snapshot, capabilities: snapshot.capabilities.map((item) => ["transcription", "diarization"].includes(item.id) ? { ...item, status: "not-installed", localStatus: "not-installed", currentSource: "off", version: undefined, localVersion: undefined } : item) };
  const client = api(actions, missingSpeech);
  client.stagePluginPackage = async (file) => ({ schemaVersion: 1, source: "/private/plugin-staging/upload.imp", filename: file.name, byteLen: file.size, sha256: "a".repeat(64), officialPluginId: "official.ocr.ppocrv6", signingKeyId: "official-test", signingKeySha256: "b".repeat(64) });
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: client }));
  await waitFor(() => window.document.body.textContent.includes("Turn audio and video into timestamped transcripts"));
  const speechCard = [...window.document.querySelectorAll<HTMLElement>(".capability-row")].find((card) => card.textContent?.includes("Speech transcription"))!;
  [...speechCard.querySelectorAll("button")].find((button) => button.textContent === "Install")!.click();
  await waitFor(() => window.document.querySelector(".capability-install-dialog") !== null);
  const dialog = window.document.querySelector<HTMLElement>(".capability-install-dialog")!;
  const fileInput = dialog.querySelector<HTMLInputElement>('input[type="file"]')!;
  Object.defineProperty(fileInput, "files", { value: [new window.File(["imp"], "official.media.whisper.imp", { type: "application/octet-stream" })], configurable: true });
  fileInput.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitFor(() => [...dialog.querySelectorAll("button")].some((button) => button.textContent === "Install selected plugin" && !button.disabled));
  [...dialog.querySelectorAll("button")].find((button) => button.textContent === "Install selected plugin")!.click();
  await waitFor(() => dialog.querySelector(".plugin-install-status [role=alert]")?.textContent?.includes("not the official package for this capability") === true);
  assert.equal(window.document.querySelector(".capability-install-dialog"), dialog);
  assert.equal(dialog.querySelectorAll(".plugin-install-status [role=alert]").length, 1);
  assert.equal(actions.length, 0);

  client.stagePluginPackage = async (file) => ({ schemaVersion: 1, source: "/private/plugin-staging/upload.imp", filename: file.name, byteLen: file.size, sha256: "c".repeat(64), officialPluginId: "official.media.whisper", signingKeyId: "official-test", signingKeySha256: "d".repeat(64) });
  [...dialog.querySelectorAll("button")].find((button) => button.textContent === "Install selected plugin")!.click();
  await waitFor(() => actions.length === 1);
  assert.equal(actions[0]!.target, "transcription");
  assert.equal(actions[0]!.scope, "global");
  assert.equal(actions[0]!.sha256, "c".repeat(64));
  assert.equal(actions[0]!.signingKeyId, "official-test");
  assert.equal(actions[0]!.signingKeySha256, "d".repeat(64));
  assert.equal(actions[0]!.authorizeNetwork, false);
});

test("capability install chooser locks before staging a large local package", async () => {
  const window = install("/admin/capabilities"); const actions: AdminAction[] = []; let stageCalls = 0;
  let resolveStage!: (value: { schemaVersion: 1; source: string; filename: string; byteLen: number; sha256: string; officialPluginId?: string; signingKeyId?: string; signingKeySha256?: string }) => void;
  const missingSpeech: AdminSnapshot = { ...snapshot, capabilities: snapshot.capabilities.map((item) => ["transcription", "diarization"].includes(item.id) ? { ...item, status: "not-installed", localStatus: "not-installed", currentSource: "off", version: undefined, localVersion: undefined } : item) };
  const client = api(actions, missingSpeech);
  client.stagePluginPackage = async () => { stageCalls += 1; return new Promise((resolve) => { resolveStage = resolve; }); };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: client }));
  await waitFor(() => window.document.body.textContent.includes("Turn audio and video into timestamped transcripts"));
  const speechCard = [...window.document.querySelectorAll<HTMLElement>(".capability-row")].find((card) => card.textContent?.includes("Speech transcription"))!;
  [...speechCard.querySelectorAll("button")].find((button) => button.textContent === "Install")!.click();
  await waitFor(() => window.document.body.textContent.includes("Install a plugin from this computer"));
  const dialog = window.document.querySelector<HTMLElement>(".capability-install-dialog")!;
  const fileInput = dialog.querySelector<HTMLInputElement>('input[type="file"]')!;
  Object.defineProperty(fileInput, "files", { value: [new window.File(["imp"], "large.imp", { type: "application/octet-stream" })], configurable: true });
  fileInput.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitFor(() => [...dialog.querySelectorAll("button")].some((button) => button.textContent === "Install selected plugin" && !button.disabled));
  const installButton = [...dialog.querySelectorAll("button")].find((button) => button.textContent === "Install selected plugin")!;
  installButton.click(); installButton.click();
  await waitFor(() => stageCalls === 1 && dialog.textContent?.includes("Reading and verifying the plugin package") === true);
  assert.equal(installButton.disabled, true);
  assert.equal(dialog.querySelectorAll(".plugin-install-status").length, 1);
  assert.equal(dialog.getAttribute("aria-busy"), "true");
  resolveStage({ schemaVersion: 1, source: "/private/plugin-staging/large.imp", filename: "large.imp", byteLen: 3, sha256: "a".repeat(64), officialPluginId: "official.media.whisper", signingKeyId: "official-test", signingKeySha256: "b".repeat(64) });
  await waitFor(() => actions.length === 1);
  assert.equal(actions[0]!.authorizeNetwork, false);
});

test("local plugin installation locks its stable action slot before a large package is staged", async () => {
  const window = install("/admin/plugins"); const actions: AdminAction[] = [];
  const client = api(actions); let stageCalls = 0; let resolveStage!: (value: { schemaVersion: 1; source: string; filename: string; byteLen: number; sha256: string }) => void;
  client.stagePluginPackage = async () => { stageCalls += 1; return new Promise((resolve) => { resolveStage = resolve; }); };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: client }));
  await waitFor(() => window.document.querySelector('[role="dialog"]')?.textContent?.includes("Local extensions") === true);
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Install extension")!.click();
  await waitFor(() => window.document.querySelector(".plugin-install-dialog") !== null);
  const installer = window.document.querySelector<HTMLElement>(".plugin-install-dialog")!;
  const fileInput = installer.querySelector<HTMLInputElement>('input[type="file"]')!;
  Object.defineProperty(fileInput, "files", { value: [new window.File(["imp"], "large.imp", { type: "application/octet-stream" })], configurable: true });
  fileInput.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitFor(() => [...installer.querySelectorAll("button")].some((button) => button.textContent === "Install" && !button.disabled));
  const installButton = [...installer.querySelectorAll("button")].find((button) => button.textContent === "Install")!;
  installButton.click(); installButton.click();
  await waitFor(() => stageCalls === 1 && installer.textContent?.includes("Reading and verifying the plugin package") === true);
  assert.equal(installButton.disabled, true);
  assert.equal(installer.querySelectorAll(".plugin-install-status").length, 1);
  resolveStage({ schemaVersion: 1, source: "/private/plugin-staging/large.imp", filename: "large.imp", byteLen: 3, sha256: "a".repeat(64) });
  await waitFor(() => actions.length === 1);
  assert.equal(actions[0]!.authorizeNetwork, false);
  await waitFor(() => window.document.querySelector(".source-manager-feedback")?.textContent === "Extension installed");
  assert.equal(window.document.querySelectorAll(".source-manager-feedback").length, 1);
});

test("local plugin installation reports backend rejection inside the unchanged installer", async () => {
  const window = install("/admin/plugins");
  const client = api();
  client.stagePluginPackage = async () => ({ schemaVersion: 1, source: "/private/plugin-staging/bad.imp", filename: "bad.imp", byteLen: 3, sha256: "a".repeat(64) });
  client.adminAction = async () => { throw new ApiError("signature"); };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: client }));
  await waitFor(() => window.document.querySelector('[role="dialog"]')?.textContent?.includes("Local extensions") === true);
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Install extension")!.click();
  await waitFor(() => window.document.querySelector(".plugin-install-dialog") !== null);
  const installer = window.document.querySelector<HTMLElement>(".plugin-install-dialog")!;
  const fileInput = installer.querySelector<HTMLInputElement>('input[type="file"]')!;
  Object.defineProperty(fileInput, "files", { value: [new window.File(["imp"], "bad.imp", { type: "application/octet-stream" })], configurable: true });
  fileInput.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitFor(() => [...installer.querySelectorAll("button")].some((button) => button.textContent === "Install" && !button.disabled));
  [...installer.querySelectorAll("button")].find((button) => button.textContent === "Install")!.click();
  await waitFor(() => installer.textContent?.includes("trusted source") === true);
  assert.equal(window.document.querySelector(".plugin-install-dialog"), installer);
  assert.equal(installer.querySelectorAll(".plugin-install-status").length, 1);
});

test("capability verification keeps progress and cancellation in one stable action slot", async () => {
  const window = install("/admin/capabilities"); let cancelled = false;
  const client = api();
  const check = (status: "running" | "cancelled") => ({ schemaVersion: 1 as const, id: "check-1", capability: "transcription", capabilityName: "Speech transcription", plugin: "official.media.whisper", pluginName: "Local speech", status, stage: "package" as const, progress: status === "running" ? 10 : 0 });
  client.startCapabilityCheck = async () => check("running");
  client.capabilityCheck = async () => check(cancelled ? "cancelled" : "running");
  client.cancelCapabilityCheck = async () => { cancelled = true; return check("cancelled"); };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: client }));
  await waitFor(() => [...window.document.querySelectorAll<HTMLElement>(".capability-row")].some((card) => card.textContent?.includes("Speech transcription") && card.querySelector(".capability-verify")));
  const speechCard = [...window.document.querySelectorAll<HTMLElement>(".capability-row")].find((card) => card.textContent?.includes("Speech transcription"))!;
  [...speechCard.querySelectorAll("button")].find((button) => button.textContent === "Verify")!.click();
  await waitFor(() => speechCard.querySelector(".capability-verify button")?.textContent?.includes("10% · Cancel") === true);
  assert.equal(speechCard.querySelectorAll(".capability-verify button").length, 1);
  assert.equal(speechCard.querySelector(".capability-feedback"), null);
  (speechCard.querySelector(".capability-verify button") as HTMLButtonElement).click();
  await waitFor(() => cancelled && speechCard.querySelector(".capability-verify button")?.textContent === "Verify");
  await new Promise((resolve) => setTimeout(resolve, 400));
  assert.equal(speechCard.querySelectorAll(".capability-verify button").length, 1);
});

test("capability verification confirms success without adding or moving controls", async () => {
  const window = install("/admin/capabilities");
  const client = api();
  const running = { schemaVersion: 1 as const, id: "check-2", capability: "transcription", capabilityName: "Speech transcription", plugin: "official.media.whisper", pluginName: "Local speech", status: "running" as const, stage: "package" as const, progress: 10 };
  const completed = { ...running, status: "completed" as const, stage: "completed" as const, progress: 100 };
  client.startCapabilityCheck = async () => running;
  client.capabilityCheck = async () => completed;
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: client }));
  await waitFor(() => [...window.document.querySelectorAll<HTMLElement>(".capability-row")].some((card) => card.textContent?.includes("Speech transcription") && card.querySelector(".capability-verify")));
  const speechCard = [...window.document.querySelectorAll<HTMLElement>(".capability-row")].find((card) => card.textContent?.includes("Speech transcription"))!;
  const slot = speechCard.querySelector<HTMLElement>(".capability-verify")!;
  const button = slot.querySelector<HTMLButtonElement>("button")!;
  button.click();
  await waitFor(() => button.textContent === "Verified");
  assert.equal(slot.querySelectorAll("button").length, 1);
  assert.equal(slot.querySelectorAll("p").length, 0);
  assert.equal(button.dataset.state, "success");
});

test("failed initial load exposes an alert and retry recovery", async () => {
  const window = install("/admin/doctor"); let calls = 0;
  const failed = api(); failed.admin = async () => { calls += 1; if (calls === 1) throw new ApiError("unreachable"); return snapshot; };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: failed }));
  await waitFor(() => window.document.querySelector('[role="alert"]') !== null);
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Reload")!.click();
  await waitFor(() => window.document.body.textContent.includes("Diagnostics")); assert.equal(calls, 2);
});

test("a busy administration refresh waits quietly and retries without a global error", async () => {
  const window = install("/admin/capabilities"); let calls = 0;
  const client = api();
  client.admin = async () => { calls += 1; if (calls === 1) throw new ApiError("adminBusy"); return snapshot; };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: client }));
  await new Promise((resolve) => setTimeout(resolve, 100));
  assert.equal(window.document.querySelector('[role="alert"]'), null);
  assert.equal(window.document.body.textContent.includes("operation did not complete"), false);
  await waitFor(() => window.document.body.textContent.includes("Image OCR") && calls === 2, "busy administration refresh did not recover");
  assert.equal(window.document.querySelector('[role="alert"]'), null);
});

test("a successful action waits for a busy post-action refresh without reporting failure", async () => {
  const window = install("/admin/plugins"); let adminCalls = 0;
  const actions: AdminAction[] = []; const client = api(actions);
  client.admin = async () => { adminCalls += 1; if (adminCalls === 2) throw new ApiError("adminBusy"); return snapshot; };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: client }));
  await waitFor(() => window.document.querySelector('[role="dialog"]')?.textContent?.includes("Local extensions") === true);
  const dialog = window.document.querySelector<HTMLElement>('[role="dialog"]')!;
  [...dialog.querySelectorAll("button")].find((button) => button.textContent === "Verify")!.click();
  await waitFor(() => dialog.textContent?.includes("Verification completed") === true && adminCalls >= 3, "post-action refresh did not recover");
  assert.equal(dialog.textContent?.includes("operation did not complete"), false);
  assert.equal(actions[0]?.action, "plugin.verify");
});

test("navigating while diagnostics run does not issue a conflicting refresh or disturb the next page", async () => {
  const window = install("/admin/doctor");
  const sections: Array<string | undefined> = [];
  let resolveAction!: (value: {}) => void;
  let actionStarted = false;
  const client = api();
  client.admin = async (_signal, section) => { sections.push(section); return snapshot; };
  client.adminAction = async () => { actionStarted = true; return new Promise((resolve) => { resolveAction = resolve; }); };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: client }));
  await waitFor(() => window.document.body.textContent.includes("Check this computer"));
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Check this computer")!.click();
  await waitFor(() => actionStarted);
  [...window.document.querySelectorAll<HTMLAnchorElement>(".admin-tabs a")].find((link) => link.textContent === "Capabilities & sources")!.click();
  await waitFor(() => window.location.pathname === "/admin/capabilities");
  await new Promise((resolve) => setTimeout(resolve, 50));
  assert.deepEqual(sections, ["doctor"]);
  assert.equal(window.document.querySelector('[role="alert"]'), null);
  resolveAction({});
  await waitFor(() => sections.includes("capabilities") && window.document.body.textContent.includes("Image OCR"), "capability page did not refresh after diagnostics");
  assert.equal(window.document.querySelector('[role="alert"]'), null);
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
  await waitFor(() => window.document.body.textContent.includes("Concurrent tasks"));
  assert.equal(window.document.querySelectorAll(".preference-group").length, 5);
  assert.equal(window.document.body.textContent.includes("Block private networks"), false);
  assert.equal(window.document.body.textContent.includes("Allowed hosts"), true);
});

test("legacy administration URLs redirect into the matching capability context", async () => {
  const window = install("/admin/providers");
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: api() }));
  await waitFor(() => window.document.querySelector('[role="dialog"]') !== null);
  assert.equal(window.location.pathname, "/admin/capabilities");
  assert.equal(window.document.querySelector('[role="dialog"]')?.textContent?.includes("AI services"), true);
  assert.equal(window.document.querySelectorAll(".admin-tabs a").length, 3);

  window.history.pushState({}, "", "/admin/plugins");
  window.dispatchEvent(new window.PopStateEvent("popstate"));
  await waitFor(() => window.document.querySelector('[role="dialog"]')?.textContent?.includes("Local extensions") === true);
  assert.equal(window.document.querySelector('[role="dialog"]')?.textContent?.includes("AI services"), false);
});

test("administration source manager swaps list and editor without stacking overlays", async () => {
  const window = install("/admin/capabilities");
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: api() }));
  await waitFor(() => [...window.document.querySelectorAll("button")].some((button) => button.textContent === "AI services"), "source manager trigger did not render");
  const sourceTrigger = [...window.document.querySelectorAll("button")].find((button) => button.textContent === "AI services")!;
  sourceTrigger.focus(); sourceTrigger.click();
  await waitFor(() => window.document.querySelectorAll('[role="dialog"]').length === 1, "source manager dialog did not open");
  const providerTrigger = [...window.document.querySelectorAll<HTMLElement>('[role="dialog"] button')].find((button) => button.textContent === "Connect AI service")!;
  providerTrigger.focus(); providerTrigger.click();
  await waitFor(() => window.document.querySelectorAll('[role="dialog"]').length === 2 && window.document.querySelector(".admin-inline-editor"), "provider editor did not replace the list view");
  assert.equal(window.document.querySelectorAll(".sheet-backdrop").length, 1);
  assert.equal(window.document.querySelectorAll(".source-manager-dialog > .admin-section-stack > .admin-grid .admin-entity-card").length, 0);
  const editor = window.document.querySelector<HTMLElement>(".admin-inline-editor")!;
  const editorFocusable = [...editor.querySelectorAll<HTMLElement>("button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled)")];
  const firstEditor = editorFocusable[0]!; const lastEditor = editorFocusable[editorFocusable.length - 1]!;
  lastEditor.focus(); window.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Tab", bubbles: true }));
  assert.equal(window.document.activeElement, firstEditor);
  firstEditor.focus(); window.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true }));
  assert.equal(window.document.activeElement, lastEditor);
  window.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  await waitFor(() => window.document.querySelectorAll('[role="dialog"]').length === 1 && window.document.activeElement?.textContent === "Connect AI service", "editor did not return to the list and restore the matching action");
  window.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  await waitFor(() => window.document.querySelectorAll('[role="dialog"]').length === 0 && window.document.activeElement === sourceTrigger, "source dialog did not close and restore its trigger");
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

test("a missing Core PDF runtime stays a Core repair issue without internal setup instructions", async () => {
  const window = install("/admin/doctor");
  const missingPdfium: AdminSnapshot = { ...snapshot, doctor: [
    { id: "runtime.pdfium", status: "missing", detail: "install the pinned PDFium runtime and set PDFIUM_LIBRARY to its exact file" },
    { id: "providerEnvironment:qa", status: "missing", detail: "QA_SERVICE_API_KEY" },
  ] };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: api([], missingPdfium) }));
  await waitFor(() => window.document.body.textContent.includes("current Core installation is incomplete"));
  const card = window.document.querySelector<HTMLElement>(".doctor-card")!;
  assert.equal(card.textContent?.includes("runtime.pdfium"), false);
  assert.equal(card.textContent?.includes("PDFIUM_LIBRARY"), false);
  assert.equal(card.textContent?.includes("Open preferences"), false);
  assert.equal(card.textContent?.includes("Reinstall or repair into-md Core"), true);
  assert.equal(window.document.body.textContent.includes("providerEnvironment:qa"), false);
  assert.equal(window.document.body.textContent.includes("QA_SERVICE_API_KEY"), false);
});

test("diagnostics clamp pagination after a rerun removes the last page", async () => {
  const window = install("/admin/doctor");
  let current: AdminSnapshot = { ...snapshot, doctor: Array.from({ length: 6 }, (_, index) => ({ id: `check-${index}`, status: "failed", detail: `failure ${index}` })) };
  const client = api();
  client.admin = async () => current;
  client.adminAction = async () => { current = { ...current, doctor: current.doctor.slice(0, 1) }; return {}; };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: client }));
  await waitFor(() => window.document.body.textContent.includes("6 need attention"));
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Next")!.click();
  await waitFor(() => window.document.body.textContent.includes("failure 5"));
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Check this computer")!.click();
  await waitFor(() => window.document.body.textContent.includes("1 need attention") && window.document.body.textContent.includes("failure 0"));
  assert.equal(window.document.querySelector(".admin-pagination"), null);
});

test("preferences keep invalid numeric feedback beside its control and block saving", async () => {
  const window = install("/admin/configuration"); const actions: AdminAction[] = [];
  const invalid: AdminSnapshot = { ...snapshot, configuration: { ...snapshot.configuration, cli: { jobs: 0 } } };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: api(actions, invalid) }));
  await waitFor(() => window.document.body.textContent.includes("Concurrent tasks"));
  const performance = [...window.document.querySelectorAll<HTMLDetailsElement>(".preference-group")].find((group) => group.textContent?.includes("Concurrent tasks"))!;
  performance.open = true;
  const jobs = [...performance.querySelectorAll<HTMLInputElement>('input[type="number"]')].at(-1)!;
  await waitFor(() => jobs.getAttribute("aria-invalid") === "true", "invalid concurrent task count was not reported");
  assert.match(jobs.closest(".preference-control")?.textContent ?? "", /whole number from 1 to 64/);
  assert.equal([...window.document.querySelectorAll("button")].find((button) => button.textContent === "Save")?.disabled, true);
  assert.equal(actions.length, 0);
});

test("OCR language preferences expose combinable choices instead of a free-form code field", async () => {
  const window = install("/admin/configuration"); const actions: AdminAction[] = [];
  const combined: AdminSnapshot = { ...snapshot, configuration: { ...snapshot.configuration, conversion: { ocr: { languages: ["zh-Hans", "en"] } } } };
  activeRoot = createRoot(window.document.getElementById("app")!); activeRoot.render(createElement(App, { api: api(actions, combined) }));
  await waitFor(() => window.document.body.textContent.includes("OCR languages"));
  const choices = window.document.querySelector<HTMLSelectElement>('[aria-label="OCR language options"]')!;
  assert.equal(choices.options.length, 8);
  assert.equal(choices.value, "zh-Hans,en");
  choices.selectedIndex = 1; assert.equal(choices.value, "zh-Hans,en");
  choices.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitFor(() => window.document.body.textContent.includes("1 unsaved change"));
  assert.equal(window.document.querySelector<HTMLSelectElement>('[aria-label="OCR language options"]')?.value, "zh-Hans,en");
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Save")!.click();
  await waitFor(() => actions.length === 1);
  assert.equal(actions[0]!.target, "conversion.ocr.languages");
  assert.equal(actions[0]!.value, '["zh-Hans","en"]');
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

  [...alpha.querySelectorAll("button")].find((button) => button.textContent === "Edit")!.click();
  await waitFor(() => window.document.querySelectorAll('[role="dialog"]').length === 2);
  const dialog = [...window.document.querySelectorAll<HTMLElement>('[role="dialog"]')].at(-1)!;
  const hostField = [...dialog.querySelectorAll("label")].find((label) => label.textContent?.includes("Allowed hosts"))!;
  const hostInput = hostField.querySelector("input")!;
  hostInput.value = "draft-only.example";
  hostInput.dispatchEvent(new window.Event("input", { bubbles: true }));
  [...dialog.querySelectorAll("button")].find((button) => button.textContent === "Cancel")!.click();
  await waitFor(() => window.document.querySelectorAll('[role="dialog"]').length === 1 && window.document.querySelectorAll(".source-manager-dialog .admin-entity-card").length === 2);

  const refreshedBeta = [...window.document.querySelectorAll<HTMLElement>(".source-manager-dialog .admin-entity-card")].find((card) => card.textContent?.includes("127.0.0.1"))!;
  [...refreshedBeta.querySelectorAll("button")].find((button) => button.textContent === "Test connection")!.click();
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

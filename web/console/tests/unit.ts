import assert from "node:assert/strict";
import nodeTest, { afterEach } from "node:test";
import { Window } from "happy-dom";
import { createElement } from "react";
import { createRoot } from "react-dom/client";
import { createApiClient, ApiError, defaultWorkbenchOptions, parseTask } from "../src/api";
import type { ApiClient, TaskRecord } from "../src/api";
import { App } from "../src/app";
import { JsonTree, SafeMarkdownPreview } from "../src/preview";
import { ErrorBoundary } from "../src/error-boundary";
import { takeSession } from "../src/session";
import styles from "../src/styles.css";

const token = "A".repeat(43);
const globalNames = ["window", "document", "navigator", "history", "location", "Node", "Element", "HTMLElement", "File"] as const;
const originalGlobals = new Map(globalNames.map((name) => [name, Object.getOwnPropertyDescriptor(globalThis, name)]));
const activeWindows = new Set<Window>();
const activeRoots = new Set<ReturnType<typeof createRoot>>();
const activeTimers = new Set<ReturnType<typeof setTimeout>>();

const testGroups = {
  preview: new Set([
    "Markdown preview never creates executable or resource-loading DOM",
    "completed workbench exposes accessible artifact preview and resource browser",
  ]),
  history_actions: new Set([
    "history controls expose accessible pinning and irreversible deletion confirmation",
  ]),
  history_cleanup: new Set([
    "immediate cleanup requires irreversible confirmation and reports reclaimed capacity",
  ]),
  workbench: new Set([
    "workbench exposes batch, folder, limits, keyboard, restored tasks, cancel and retry",
    "workbench presents network access as one bounded switch",
    "workbench reports the bounded upload rejection code",
    "API rejection renders a recoverable status error rather than the error boundary",
    "ErrorBoundary contains provider render errors and focuses its fallback heading",
  ]),
  accessibility: new Set([
    "shell primitives expose keyboard focus and language-safe DOM behavior",
    "checked CSS color tokens meet WCAG AA normal-text contrast",
    "real App mount synchronizes language without stealing preference focus",
    "real mounted App has no axe violations; geometry-incomplete rules are not treated as coverage",
  ]),
} as const;

type TestGroup = keyof typeof testGroups | "core";
const selectedGroup = (process.env.INTO_MD_TEST_GROUP ?? "core") as TestGroup;
if (!(selectedGroup === "core" || selectedGroup in testGroups)) {
  throw new Error(`Unknown INTO_MD_TEST_GROUP: ${selectedGroup}`);
}

function groupFor(name: string): TestGroup {
  for (const [group, names] of Object.entries(testGroups)) {
    if ((names as Set<string>).has(name)) return group as keyof typeof testGroups;
  }
  return "core";
}

function test(name: string, fn: () => unknown | Promise<unknown>): void {
  void nodeTest(name, { skip: groupFor(name) !== selectedGroup }, fn);
}

function trackedRoot(container: Element): ReturnType<typeof createRoot> {
  const root = createRoot(container);
  activeRoots.add(root);
  return root;
}

function trackedWindow(window: Window): Window {
  activeWindows.add(window);
  return window;
}

afterEach(async () => {
  for (const root of activeRoots) root.unmount();
  activeRoots.clear();
  await new Promise<void>((resolve) => setImmediate(resolve));
  for (const timer of activeTimers) clearTimeout(timer);
  activeTimers.clear();
  for (const window of activeWindows) window.close();
  activeWindows.clear();
  await new Promise<void>((resolve) => setImmediate(resolve));
  for (const name of globalNames) {
    const descriptor = originalGlobals.get(name);
    if (descriptor) Object.defineProperty(globalThis, name, descriptor);
    else Reflect.deleteProperty(globalThis, name);
  }
});

function installWindow(languages = ["en"]): Window {
  const window = trackedWindow(new Window({ url: "http://127.0.0.1:1/status" }));
  Object.defineProperty(window.navigator, "languages", { value: languages, configurable: true });
  window.document.head.innerHTML = "<title>into-markdown</title>";
  window.document.body.innerHTML = '<div id="app"></div>';
  for (const [name, value] of Object.entries({
    window, document: window.document, navigator: window.navigator,
    history: window.history, location: window.location, Node: window.Node,
    Element: window.Element, HTMLElement: window.HTMLElement, File: window.File,
  })) Object.defineProperty(globalThis, name, { value, writable: true, configurable: true });
  return window;
}

function waitFor(predicate: () => boolean, timeout = 1_000): Promise<void> {
  const started = Date.now();
  return new Promise((resolvePromise, reject) => {
    const check = () => {
      if (predicate()) resolvePromise();
      else if (Date.now() - started > timeout) reject(new Error("DOM condition timed out"));
      else {
        const timer = setTimeout(() => {
          activeTimers.delete(timer);
          check();
        }, 5);
        activeTimers.add(timer);
      }
    };
    check();
  });
}

async function waitForText(window: Window, value: string): Promise<void> {
  await waitFor(() => window.document.body.textContent.includes(value)).catch(() => {
    throw new Error(`Missing text ${JSON.stringify(value)} in ${JSON.stringify(window.document.body.textContent)}`);
  });
}

const task = (status: TaskRecord["status"] = "running", id = "a".repeat(32)): TaskRecord => ({
  id, createdAtMs: 1, updatedAtMs: 2, status, progressMillionths: status === "succeeded" ? 1_000_000 : 250_000,
  diagnostics: status === "failed" ? [{ code: "conversionFailed" }] : [], artifacts: [],
  pinned: false,
  configuration: { schemaVersion: 1, ocrEnabled: true, preserveLayout: true },
});

const availableApi: ApiClient = {
  async status() {
    return {
      schemaVersion: 1 as const,
      localApi: { available: true, code: "available", detail: "ok" },
      documentConsole: { available: true, code: "available", detail: "ok" },
    };
  },
  async listTasks() { return { tasks: [] }; },
  async getTask(id) { return task("running", id); },
  async upload() { return task(); },
  async cancel(id) { return task("cancelled", id); },
  async retry(id) { return task("pending", id); },
  async setPinned(id, pinned) { return { ...task("succeeded", id), pinned }; },
  async deleteTask() {},
  async cleanup() { return { schemaVersion: 1 as const, deletedTasks: 0, reclaimedBytes: 0 }; },
  async watchTask(_id, _onEvent, signal) {
    await new Promise<void>((resolve) => signal.addEventListener("abort", () => resolve(), { once: true }));
  },
  async preview() { return { text: "", truncated: false, contentType: "text/plain" }; },
  async download() { return { blob: new Blob(), filename: "result.md" }; },
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
  const maliciousTask = task("succeeded"); maliciousTask.artifacts = [{ storageKey: "b".repeat(32), kind: "asset", byteLen: 1, sha256: "c".repeat(64), filename: "../escape.png" }];
  assert.throws(() => parseTask(maliciousTask), ApiError);
  const nullableArtifactTask = task("succeeded");
  nullableArtifactTask.artifacts = [{ storageKey: "b".repeat(32), kind: "markdown", byteLen: 1, sha256: "c".repeat(64), assetId: null, filename: null, mediaType: null }];
  assert.equal(parseTask(nullableArtifactTask).artifacts[0]?.filename, null);
});

test("workbench API sends shared conversion options and resumes SSE from Last-Event-ID", async () => {
  const calls: Array<[RequestInfo | URL, RequestInit | undefined]> = [];
  let stream = 0;
  const responseTask = task();
  const client = createApiClient(token, async (input, init) => {
    calls.push([input, init]);
    if (String(input).endsWith("/events")) {
      stream += 1;
      const status = stream === 1 ? "running" : "succeeded";
      const terminal = stream !== 1;
      return new Response(`id: ${stream}\ndata: ${JSON.stringify({ schemaVersion: 1, sequence: stream, taskId: responseTask.id, kind: "progress", status, progressMillionths: terminal ? 1_000_000 : 500_000, terminal, execution: { stage: terminal ? "complete" : "convert", basisPoints: terminal ? 10_000 : 5_000 } })}\n\n`, { headers: { "content-type": "text/event-stream" } });
    }
    return new Response(JSON.stringify(responseTask), { headers: { "content-type": "application/json" } });
  });
  const options = { ...defaultWorkbenchOptions, format: "pdf" as const, ocrPolicy: "always" as const, networkMode: "unrestricted" as const };
  await client.upload(new File(["pdf"], "报告.pdf"), options);
  const uploadHeaders = calls[0]![1]!.headers as Record<string, string>;
  const filename = uploadHeaders["X-Into-Md-Filename-B64"]!;
  assert.equal(new TextDecoder().decode(Uint8Array.from(atob(filename.replaceAll("-", "+").replaceAll("_", "/")), (char) => char.charCodeAt(0))), "报告.pdf");
  const encoded = uploadHeaders["X-Into-Md-Request"]!;
  const request = JSON.parse(new TextDecoder().decode(Uint8Array.from(atob(encoded.replaceAll("-", "+").replaceAll("_", "/")), (char) => char.charCodeAt(0)))) as Record<string, any>;
  assert.equal(request.schemaVersion, 1);
  assert.equal(request.format, "pdf");
  assert.equal(request.options.ocr.policy, "always");
  assert.deepEqual(request.options.asr, {
    model_bundle: "whisper-small-multilingual", language: null, max_threads: 4,
    max_duration_ms: 600_000, max_segments: 10_000, max_native_memory_bytes: 900 * 1024 * 1024,
  });
  assert.deepEqual(request.options.network, { enabled: true, max_redirects: 3, deny_private_networks: false, allowed_hosts: [] });
  assert.equal(request.authorization.network, true);
  assert.equal(request.authorization.privateNetwork, true);
  const events: string[] = [];
  await client.watchTask(responseTask.id, (event) => events.push(event.status), new AbortController().signal);
  assert.deepEqual(events, ["running", "succeeded"]);
  const reconnectHeaders = calls[2]![1]!.headers as Record<string, string>;
  assert.equal(reconnectHeaders["Last-Event-ID"], "1");
});

test("artifact preview is range-bounded and download filename follows safe Content-Disposition", async () => {
  const calls: Array<[RequestInfo | URL, RequestInit | undefined]> = [];
  const client = createApiClient(token, async (input, init) => {
    calls.push([input, init]);
    if ((init?.headers as Record<string, string>).Range) return new Response("preview", { status: 206, headers: { "content-type": "text/markdown; charset=utf-8", "content-range": "bytes 0-6/999999" } });
    return new Response(new Blob(["complete"]), { headers: { "content-type": "application/octet-stream", "content-disposition": "attachment; filename=\"fallback.bin\"; filename*=UTF-8''%E6%8A%A5%E5%91%8A.md" } });
  });
  const preview = await client.preview("a".repeat(32), "b".repeat(32));
  assert.equal((calls[0]![1]!.headers as Record<string, string>).Range, "bytes=0-262143");
  assert.deepEqual(preview, { text: "preview", truncated: true, contentType: "text/markdown" });
  const download = await client.download("a".repeat(32), "b".repeat(32));
  assert.equal(download.filename, "报告.md"); assert.equal(await download.blob.text(), "complete");
  const oversized = createApiClient(token, async () => new Response("x".repeat(262_145), { status: 206, headers: { "content-type": "text/markdown" } }));
  await assert.rejects(oversized.preview("a".repeat(32), "b".repeat(32)), (error: unknown) => error instanceof ApiError && error.code === "responseTooLarge");
});

test("history API paginates, filters, pins, retries and permanently deletes explicitly", async () => {
  const calls: Array<[string, RequestInit | undefined]> = [];
  const current = task("succeeded");
  const client = createApiClient(token, async (input, init) => {
    calls.push([String(input), init]);
    if (init?.method === "DELETE") return new Response(null, { status: 204 });
    if (String(input).startsWith("/api/tasks?")) return new Response(JSON.stringify({ schemaVersion: 1, tasks: [current], nextCursor: { updatedAtMs: 2, id: current.id } }), { headers: { "content-type": "application/json" } });
    return new Response(JSON.stringify(current), { headers: { "content-type": "application/json" } });
  });
  const page = await client.listTasks({ limit: 1, status: "succeeded", pinned: true, after: { updatedAtMs: 3, id: "b".repeat(32) } });
  assert.equal(page.nextCursor?.id, current.id);
  assert.equal(calls[0]![0], `/api/tasks?limit=1&afterUpdatedAtMs=3&afterId=${"b".repeat(32)}&status=succeeded&pinned=true`);
  await client.setPinned(current.id, true); await client.retry(current.id); await client.deleteTask(current.id);
  assert.equal(calls[1]![0], `/api/tasks/${current.id}/pin`);
  assert.equal(calls[1]![1]?.body, JSON.stringify({ pinned: true }));
  assert.equal(calls[2]![0], `/api/tasks/${current.id}/retry`);
  assert.equal(calls[3]![0], `/api/tasks/${current.id}/history`);
});

test("history delete preserves bounded server errors and normalizes network failures", async () => {
  const id = "a".repeat(32);
  const server = createApiClient(token, async () => new Response(JSON.stringify({ code: "notFound" }), {
    status: 404, headers: { "content-type": "application/json" },
  }));
  await assert.rejects(server.deleteTask(id), (error: unknown) => error instanceof ApiError && error.code === "notFound");
  const network = createApiClient(token, async () => { throw new TypeError("offline"); });
  await assert.rejects(network.deleteTask(id), (error: unknown) => error instanceof ApiError && error.code === "unreachable");
  const oversized = createApiClient(token, async () => new Response(JSON.stringify({ code: "x".repeat(70_000) }), {
    status: 500, headers: { "content-type": "application/json" },
  }));
  await assert.rejects(oversized.deleteTask(id), (error: unknown) => error instanceof ApiError && error.code === "responseTooLarge");
});

test("Markdown preview never creates executable or resource-loading DOM", async () => {
  const window = installWindow(); const root = trackedRoot(window.document.getElementById("app")!);
  const malicious = "# Safe\n<script>globalThis.pwned=1</script>\n![x](file:///etc/passwd)\n<img src=http://evil.invalid/x onerror=alert(1)>\n[jump](javascript:alert(1))";
  root.render(createElement(SafeMarkdownPreview, { source: malicious }));
  await waitFor(() => window.document.body.textContent.includes("file:///etc/passwd"));
  assert.equal(window.document.querySelector("script,img,iframe,object,embed,link,a"), null);
  assert.equal((globalThis as { pwned?: number }).pwned, undefined);
  assert.ok(window.document.body.textContent.includes("<script>"));
  root.render(createElement(SafeMarkdownPreview, { source: Array.from({ length: 2_100 }, () => "line").join("\n") }));
  await waitFor(() => window.document.body.textContent.includes("preview block limit reached"));
  root.render(createElement(JsonTree, { value: { provenance: { source: "local" }, blocks: Array.from({ length: 250 }, (_, index) => ({ index })) } }));
  await waitFor(() => window.document.body.textContent.includes("provenance"));
  assert.ok(window.document.body.textContent.includes("more entries"));
});

test("completed workbench exposes accessible artifact preview and resource browser", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  const completed = task("succeeded"); completed.artifacts = [
    { storageKey: "b".repeat(32), kind: "markdown", byteLen: 40, sha256: "c".repeat(64) },
    { storageKey: "d".repeat(32), kind: "asset", byteLen: 12, sha256: "e".repeat(64), assetId: "image-1", filename: "diagram.png", mediaType: "image/png" },
  ];
  const api: ApiClient = { ...availableApi, async listTasks() { return { tasks: [completed] }; }, async preview() { return { text: "<img src=file:///secret>\n<script>alert(1)</script>", truncated: false, contentType: "text/markdown" }; } };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitFor(() => window.document.body.textContent.includes("Preview result.md"));
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Preview result.md")!.click();
  await waitFor(() => window.document.body.textContent.includes("file:///secret"));
  assert.equal(window.document.querySelector(".preview-panel img,.preview-panel script,.preview-panel a"), null);
  assert.ok(window.document.body.textContent.includes("Resources (1)"));
  const axe = (await import("axe-core")).default; const result = await axe.run(window.document);
  assert.deepEqual(result.violations.map((violation) => violation.id), []);
});

test("history controls expose accessible pinning and irreversible deletion confirmation", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  const completed = task("succeeded"); let pinned = false; let deleted = false; let warning = ""; let listCalls = 0;
  window.confirm = (message?: string) => { warning = message ?? ""; return true; };
  const api: ApiClient = {
    ...availableApi,
    async listTasks() { listCalls += 1; return { tasks: [completed] }; },
    async setPinned(id, value) { pinned = value; return { ...completed, id, pinned: value }; },
    async deleteTask() { deleted = true; },
  };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "Task details");
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Pin")!.click();
  await waitFor(() => pinned && window.document.body.textContent.includes("Unpin"));
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Delete permanently")!.click();
  await waitFor(() => deleted && window.document.querySelector(".task-card") === null);
  assert.ok(warning.includes("cannot be undone"));
  assert.equal(listCalls, 1);
  assert.equal(window.document.querySelector(".task-card"), null);
});
test("immediate cleanup requires irreversible confirmation and reports reclaimed capacity", async () => {
  const window2 = installWindow();
  window2.history.replaceState(null, "", "/workbench");
  let cleanups = 0;
  let warning = "";
  window2.confirm = (message) => {
    warning = message ?? "";
    return true;
  };
  const api = {
    ...availableApi,
    async listTasks() { return { tasks: [] }; },
    async cleanup() {
      cleanups += 1;
      return { schemaVersion: 1 as const, deletedTasks: 2, reclaimedBytes: 1572864 };
    }
  };
  const root = trackedRoot(window2.document.getElementById("app")!);
  root.render(createElement(App, { api }));
  await waitForText(window2, "No tasks yet");
  [...window2.document.querySelectorAll("button")].find((button) => button.textContent === "Clean up now")!.click();
  await waitFor(() => cleanups === 1 && window2.document.body.textContent.includes("1.5 MiB"));
  assert.ok(warning.includes("cannot be undone"));
  const axe = (await import("axe-core")).default;
  assert.deepEqual((await axe.run(window2.document)).violations.map((violation) => violation.id), []);
});

test("workbench exposes batch, folder, limits, keyboard, restored tasks, cancel and retry", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  let cancelled = 0; let uploaded = 0;
  const failed = task("failed", "b".repeat(32)); const running = task("running", "c".repeat(32));
  const api: ApiClient = {
    ...availableApi,
    async listTasks() { return { tasks: [running, failed] }; },
    async upload() { uploaded += 1; return task("running", "d".repeat(32)); },
    async cancel(id) { cancelled += 1; return task("cancelled", id); },
    async retry() { throw new ApiError("originalUnavailable"); },
  };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitFor(() => window.document.body.textContent.includes("Restored task"), 2_000).catch(() => { throw new Error(window.document.body.textContent); });
  const inputs = [...window.document.querySelectorAll<HTMLInputElement>('input[type="file"]')];
  assert.equal(inputs.length, 2); assert.equal(inputs[0]!.multiple, true); assert.equal(inputs[1]!.hasAttribute("webkitdirectory"), true);
  const zone = window.document.getElementById("upload-zone")!; let pickerClicks = 0; inputs[0]!.click = () => { pickerClicks += 1; };
  zone.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  zone.dispatchEvent(new window.KeyboardEvent("keydown", { key: " ", bubbles: true }));
  assert.equal(pickerClicks, 2);
  const drop = new window.Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: { files: [new File(["one"], "one.md"), new File(["two"], "two.md")] } });
  zone.dispatchEvent(drop); await waitForText(window, "Selected (2)");
  const convert = [...window.document.querySelectorAll("button")].find((button) => button.textContent?.startsWith("Start conversion 2"))!;
  convert.click(); await waitFor(() => uploaded === 2);
  const cancel = [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Cancel")!;
  cancel.click(); await waitFor(() => cancelled === 1);
  const retry = [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Retry")!;
  retry.click(); await waitForText(window, "select the original file again");
  const oversized = new File(["x"], "huge.pdf"); Object.defineProperty(oversized, "size", { value: 513 * 1024 * 1024 });
  const hugeDrop = new window.Event("drop", { bubbles: true, cancelable: true }); Object.defineProperty(hugeDrop, "dataTransfer", { value: { files: [oversized] } });
  zone.dispatchEvent(hugeDrop); await waitForText(window, "exceeds the selected per-file limit");
  const many = Array.from({ length: 101 }, (_, index) => new File(["x"], `${index}.txt`));
  const manyDrop = new window.Event("drop", { bubbles: true, cancelable: true }); Object.defineProperty(manyDrop, "dataTransfer", { value: { files: many } });
  zone.dispatchEvent(manyDrop); await waitForText(window, "at most 100 files");
});

test("workbench presents network access as one bounded switch", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api: availableApi }));
  await waitForText(window, "When off, conversions process local content only.");
  assert.ok(!window.document.body.textContent.includes("Allowed hosts"));
  assert.ok(!window.document.body.textContent.includes("private-network targets"));
  const label = [...window.document.querySelectorAll("label")].find((item) => item.textContent?.includes("Allow network access"))!;
  const toggle = label.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
  assert.equal(toggle.checked, false);
  toggle.click();
  await waitForText(window, "When on, conversions may access internet and local-network services.");
  assert.equal(toggle.checked, true);
});

test("workbench reports the bounded upload rejection code", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  const api: ApiClient = {
    ...availableApi,
    async upload() { throw new ApiError("invalidTaskOptions"); },
  };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "No tasks yet");
  const zone = window.document.getElementById("upload-zone")!;
  const drop = new window.Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: { files: [new File(["contract"], "contract.md")] } });
  zone.dispatchEvent(drop); await waitForText(window, "Selected (1)");
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Start conversion 1")!.click();
  await waitForText(window, "Upload failed: contract.md (invalidTaskOptions)");
});

test("shell primitives expose keyboard focus and language-safe DOM behavior", () => {
  const window = trackedWindow(new Window({ url: "http://127.0.0.1:1/status" }));
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
  const lightButtons = styles.match(/^:root\s*\{([^}]*)\}/i)?.[1];
  const explicitDark = styles.match(/:root\[data-theme="dark"\]\s*\{([^}]*)\}/i)?.[1];
  const systemDark = styles.match(/@media \(prefers-color-scheme: dark\)\s*\{\s*:root:not\(\[data-theme="light"\]\)\s*\{([^}]*)\}/i)?.[1];
  assert.ok(lightButtons && explicitDark && systemDark);
  for (const [mode, block] of [
    ["light", lightButtons],
    ["explicit dark", explicitDark],
    ["system dark", systemDark],
  ] as const) {
    const background = block.match(/--accent-strong:\s*(#[0-9a-f]{6});/i)?.[1];
    const foreground = block.match(/--button-text:\s*(#[0-9a-f]{6});/i)?.[1];
    assert.ok(background && foreground);
    assert.ok(contrast(background, foreground) >= 4.5, `${mode} Retry/Reload contrast`);
  }
  assert.match(styles, /button\s*\{[^}]*color:\s*var\(--button-text\);[^}]*background:\s*var\(--accent-strong\);/i);
  assert.match(styles, /@media \(max-width: 44rem\)/);
  assert.match(styles, /@media \(prefers-reduced-motion: reduce\)/);
  assert.match(styles, /:focus-visible/);
});

test("real App mount synchronizes language without stealing preference focus", async () => {
  const window = installWindow(["zh-CN", "en"]);
  const root = trackedRoot(window.document.getElementById("app")!);
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
});

test("real mounted App has no axe violations; geometry-incomplete rules are not treated as coverage", async () => {
  const window = installWindow();
  const root = trackedRoot(window.document.getElementById("app")!);
  root.render(createElement(App, { api: availableApi }));
  await waitFor(() => window.document.body.textContent.includes("Local API available"));
  const axe = (await import("axe-core")).default;
  const result = await axe.run(window.document);
  assert.deepEqual(result.violations.map((violation) => violation.id), []);
  const incomplete = new Set(result.incomplete.map((item) => item.id));
  if (incomplete.has("color-contrast")) {
    assert.equal(result.passes.some((item) => item.id === "color-contrast"), false);
  }
});

test("API rejection renders a recoverable status error rather than the error boundary", async () => {
  const window = installWindow();
  const root = trackedRoot(window.document.getElementById("app")!);
  root.render(createElement(App, { api: { status: async () => { throw new ApiError("unreachable"); } } }));
  await waitFor(() => window.document.body.textContent.includes("Could not read service status"));
  assert.ok(window.document.querySelector('[role="alert"]'));
  assert.equal(window.document.body.textContent.includes("The page encountered a problem"), false);
  const retry = [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Retry");
  assert.ok(retry?.matches("button"));
});

test("ErrorBoundary contains provider render errors and focuses its fallback heading", async () => {
  const window = installWindow();
  const root = createRoot(window.document.getElementById("app")!, { onCaughtError: () => undefined });
  activeRoots.add(root);
  function FailingProvider(): never { throw new Error("untrusted provider failure"); }
  root.render(createElement(ErrorBoundary, null, createElement(FailingProvider)));
  await waitFor(() => window.document.body.textContent.includes("The page encountered a problem"));
  const heading = window.document.querySelector("h1")!;
  assert.equal(window.document.activeElement, heading);
  assert.equal(heading.tabIndex, -1);
  const reload = [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Reload");
  assert.ok(reload?.matches("button"));
  root.unmount();
});

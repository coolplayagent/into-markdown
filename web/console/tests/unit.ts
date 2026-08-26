import assert from "node:assert/strict";
import nodeTest, { afterEach } from "node:test";
import { Window } from "happy-dom";
import { createElement } from "react";
import { createRoot } from "react-dom/client";
import {
  createApiClient, ApiError, defaultMeetingOptions, defaultWorkbenchOptions, meetingTaskRequest,
  meetingOptionsForLocale, parseTask,
} from "../src/api";
import type { ApiClient, MeetingOptions, TaskRecord, WorkbenchOptions } from "../src/api";
import { App } from "../src/app";
import { requestMicrophone, requestSystemAudio } from "../src/meeting-page";
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
    "result dialog provides reading and source views with a closed details drawer",
    "failed results keep the document area stable and expose retry beside the failure",
    "meeting speaker names rerender artifacts through generation CAS without rerunning transcription",
  ]),
  history_actions: new Set([
    "recent history opens a result dialog with irreversible task actions",
  ]),
  history_cleanup: new Set([
    "immediate cleanup requires irreversible confirmation and reports reclaimed capacity",
  ]),
  workbench: new Set([
    "workbench keeps the current batch and conversion controls in one route",
    "workbench keeps OCR neutral while the fast capability snapshot is pending",
    "workbench rejects unsupported files before upload and explains terminal failures",
    "completed current-batch rows open their result from the whole row",
    "workbench separates the current batch from scrollable recent history",
    "history paginates in place and loads records beyond the first server page",
    "root workbench automatically opens the first successful result dialog",
    "local workbench keeps implementation limits and network policy out of the normal flow",
    "remote OCR requires nearby network and provider authorization without enabling unrelated AI modes",
    "meeting recording is an independent route and media never enters the document workbench",
    "Chinese meeting UI defaults to Simplified Chinese without overriding explicit choices",
    "meeting page keeps recording primary and setup feedback beside transcript controls",
    "meeting keeps speech capabilities neutral while the fast snapshot is pending",
    "remote transcription requires a one-upload grant beside transcript controls",
    "workbench explains upload rejection without exposing an internal code",
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
  const window = trackedWindow(new Window({ url: "http://127.0.0.1:1/workbench" }));
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
  pinned: false, artifactGeneration: 0, workflow: "conversion",
  configuration: { schemaVersion: 1, ocrEnabled: true, preserveLayout: true },
});

const availableApi: ApiClient = {
  async capabilitySnapshot() { return capabilitySnapshot(false); },
  async status() {
    return {
      schemaVersion: 1 as const,
      localApi: { available: true, code: "available", detail: "ok" },
      documentConsole: { available: true, code: "available", detail: "ok" },
      imageOcr: { available: false, code: "componentUnavailable", detail: "setup" },
      audioTranscription: { available: false, code: "componentUnavailable", detail: "setup" },
      speakerDiarization: { available: false, code: "componentUnavailable", detail: "setup" },
    };
  },
  async installCapability() {},
  async listTasks() { return { tasks: [] }; },
  async getTask(id) { return task("running", id); },
  async upload() { return task(); },
  async uploadMeeting() { return { ...task(), workflow: "meetingTranscript" }; },
  async cancel(id) { return task("cancelled", id); },
  async retry(id) { return task("pending", id); },
  async setPinned(id, pinned) { return { ...task("succeeded", id), pinned }; },
  async speakerLabels() { return { schemaVersion: 1 as const, artifactGeneration: 0, speakers: [] }; },
  async relabelSpeakers(id) { return { ...task("succeeded", id), workflow: "meetingTranscript" }; },
  async deleteTask() {},
  async cleanup() { return { schemaVersion: 1 as const, deletedTasks: 0, reclaimedBytes: 0 }; },
  async watchTask(_id, _onEvent, signal) {
    await new Promise<void>((resolve) => signal.addEventListener("abort", () => resolve(), { once: true }));
  },
  async preview() { return { text: "", truncated: false, contentType: "text/plain" }; },
  async download() { return { blob: new Blob(), filename: "result.md" }; },
  async admin() { return { schemaVersion: 1, formats: [], capabilities: [], providers: [], plugins: [], configuration: {}, profiles: [], doctor: [], configurationReadOnly: false }; },
  async adminAction() { return {}; },
};

function capabilitySnapshot(ready: boolean) {
  const status = ready ? "ready" as const : "not-installed" as const;
  const source = (id: "ocr" | "transcription" | "diarization") => ready ? `plugin:official.${id}/${id}` : "off";
  return { schemaVersion: 2 as const, generation: 1, checking: false, checkedAtMs: 1, capabilities: (["ocr", "transcription", "diarization"] as const).map((id) => ({ id, name: id, status, localStatus: status, currentSource: source(id), currentSourceName: ready ? "Local plugin" : "Off", sources: [source(id), "off"] })) };
}

test("session handoff clears every fragment and survives only in the current tab", () => {
  const values = new Map<string, string>();
  const storage = {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => { values.set(key, value); },
    removeItem: (key: string) => { values.delete(key); },
  };
  for (const hash of [
    `#into-md-session=${token}`,
    "",
    "#into-md-session=short",
    `#into-md-session=${token}&next=evil`,
    `#other=${token}`,
  ]) {
    values.clear();
    const calls: unknown[][] = [];
    const session = takeSession(
      { hash, pathname: "/status", search: "?language=en" },
      { replaceState: (...args: unknown[]) => { calls.push(args); } },
      storage,
    );
    assert.deepEqual(calls, [[null, "", "/status?language=en"]]);
    assert.equal(session, hash === `#into-md-session=${token}` ? token : null);
  }
  assert.equal(takeSession(
    { hash: `#into-md-session=${token}`, pathname: "/meetings", search: "" },
    { replaceState() {} }, storage,
  ), token);
  assert.equal(takeSession(
    { hash: "", pathname: "/meetings", search: "" },
    { replaceState() {} }, storage,
  ), token);
  assert.equal(takeSession(
    { hash: "#bad", pathname: "/meetings", search: "" },
    { replaceState() {} }, storage,
  ), null);
  assert.equal(takeSession(
    { hash: "", pathname: "/meetings", search: "" },
    { replaceState() {} }, storage,
  ), null);
});

test("microphone requests time out without leaking a stream that resolves late", async () => {
  const window = installWindow();
  let resolveStream!: (stream: MediaStream) => void;
  const pending = new Promise<MediaStream>((resolve) => { resolveStream = resolve; });
  let stopped = 0;
  const lateStream = { getTracks: () => [{ stop: () => { stopped += 1; } }] } as unknown as MediaStream;
  Object.defineProperty(window.navigator, "mediaDevices", {
    configurable: true, value: { getUserMedia: () => pending },
  });
  await assert.rejects(requestMicrophone({ audio: true }, 5),
    (error: unknown) => error instanceof DOMException && error.name === "TimeoutError");
  resolveStream(lateStream);
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.equal(stopped, 1);
});

test("computer audio capture rejects a share without audio and releases every shared track", async () => {
  const window = installWindow();
  let stopped = 0;
  let constraints: DisplayMediaStreamOptions | undefined;
  const shared = {
    getAudioTracks: () => [],
    getTracks: () => [{ stop: () => { stopped += 1; } }, { stop: () => { stopped += 1; } }],
  } as unknown as MediaStream;
  Object.defineProperty(window.navigator, "mediaDevices", {
    configurable: true,
    value: { getDisplayMedia: async (value: DisplayMediaStreamOptions) => { constraints = value; return shared; } },
  });
  await assert.rejects(requestSystemAudio(50),
    (error: unknown) => error instanceof DOMException && error.name === "NotFoundError");
  assert.deepEqual(constraints, { audio: true, video: true });
  assert.equal(stopped, 2);
});

test("meeting upload format prefers the recorder MIME type over ambiguous extensions", () => {
  const window = installWindow();
  const format = (file: File) => {
    const request = meetingTaskRequest(file, defaultMeetingOptions) as {
      format: "audio" | "video";
    };
    return request.format;
  };
  assert.equal(format(new window.File(["audio"], "recording.webm", { type: "audio/webm" })), "audio");
  assert.equal(format(new window.File(["audio"], "recording.mp4", { type: "audio/mp4" })), "audio");
  assert.equal(format(new window.File(["video"], "camera.webm", { type: "video/webm" })), "video");
  assert.equal(format(new window.File(["video"], "camera.webm")), "video");
});

test("meeting request binds language hints to deterministic Chinese script output", () => {
  const window = installWindow();
  const file = new window.File(["audio"], "meeting.m4a", { type: "audio/mp4" });
  const simplified = meetingTaskRequest(file, meetingOptionsForLocale("zh-CN")) as any;
  assert.equal(simplified.options.asr.language, "zh");
  assert.equal(simplified.options.asr.chinese_script, "simplified");
  const traditional = meetingTaskRequest(file, {
    ...defaultMeetingOptions, transcriptLanguage: "zh-Hant",
  }) as any;
  assert.equal(traditional.options.asr.language, "zh");
  assert.equal(traditional.options.asr.chinese_script, "traditional");
  const automatic = meetingTaskRequest(file, defaultMeetingOptions) as any;
  assert.equal(automatic.options.asr.language, null);
  assert.equal(automatic.options.asr.chinese_script, "preserve");
  const remote = meetingTaskRequest(file, { ...defaultMeetingOptions, authorizeProvider: true }) as any;
  assert.equal(remote.options.network.enabled, true);
  assert.equal(remote.options.network.deny_private_networks, false);
  assert.deepEqual(remote.authorization, { network: true, privateNetwork: true, provider: true });
});

test("API client sends only the strict POST contract and validates bounded DTOs", async () => {
  let captured: [RequestInfo | URL, RequestInit | undefined] | undefined;
  const client = createApiClient(token, async (input, init) => {
    captured = [input, init];
    return new Response(JSON.stringify({
      schemaVersion: 1,
      localApi: { available: true, code: "available", detail: "ok" },
      documentConsole: { available: false, code: "componentUnavailable", detail: "not installed" },
      imageOcr: { available: false, code: "componentUnavailable", detail: "not installed" },
      audioTranscription: { available: false, code: "componentUnavailable", detail: "not installed" },
      speakerDiarization: { available: false, code: "componentUnavailable", detail: "not installed" },
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
      return new Response(`id: ${stream}\ndata: ${JSON.stringify({ schemaVersion: 1, sequence: stream, taskId: responseTask.id, kind: "progress", status, progressMillionths: terminal ? 1_000_000 : 500_000, terminal, execution: { stage: terminal ? "completed" : "converting", basisPoints: terminal ? 10_000 : 5_000, completedUnits: terminal ? 1 : 5, totalUnits: terminal ? 1 : 10, message: null } })}\n\n`, { headers: { "content-type": "text/event-stream" } });
    }
    return new Response(JSON.stringify(responseTask), { headers: { "content-type": "application/json" } });
  });
  const options = { ...defaultWorkbenchOptions, format: "pdf" as const, ocrPolicy: "always" as const, networkMode: "unrestricted" as const };
  const batchId = "d".repeat(32);
  await client.upload(new File(["pdf"], "报告.pdf"), options, batchId);
  const uploadHeaders = calls[0]![1]!.headers as Record<string, string>;
  const filename = uploadHeaders["X-Into-Md-Filename-B64"]!;
  assert.equal(new TextDecoder().decode(Uint8Array.from(atob(filename.replaceAll("-", "+").replaceAll("_", "/")), (char) => char.charCodeAt(0))), "报告.pdf");
  const encoded = uploadHeaders["X-Into-Md-Request"]!;
  const request = JSON.parse(new TextDecoder().decode(Uint8Array.from(atob(encoded.replaceAll("-", "+").replaceAll("_", "/")), (char) => char.charCodeAt(0)))) as Record<string, any>;
  assert.equal(request.schemaVersion, 1);
  assert.equal(request.batchId, batchId);
  assert.equal(request.format, "pdf");
  assert.equal(request.options.ocr.policy, "always");
  assert.equal(request.options.limits.max_memory_bytes, 1024 * 1024 * 1024);
  assert.deepEqual(request.options.asr, {
    language: null, chinese_script: "preserve", max_threads: 4,
    max_duration_ms: null, max_segments: 100_000, max_native_memory_bytes: 900 * 1024 * 1024,
  });
  assert.equal(request.options.ai.audio_transcription, "off");
  assert.deepEqual(request.options.network, { enabled: true, max_redirects: 3, deny_private_networks: false, allowed_hosts: [] });
  assert.equal(request.authorization.network, true);
  assert.equal(request.authorization.privateNetwork, true);
  assert.equal(request.authorization.provider, false);
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
  const page = await client.listTasks({ limit: 1, status: "succeeded", pinned: true, batchId: "c".repeat(32), after: { updatedAtMs: 3, id: "b".repeat(32) } });
  assert.equal(page.nextCursor?.id, current.id);
  assert.equal(calls[0]![0], `/api/tasks?limit=1&afterUpdatedAtMs=3&afterId=${"b".repeat(32)}&status=succeeded&pinned=true&batchId=${"c".repeat(32)}`);
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
  const malicious = "# Safe\n<em>\\[</em><em>Image OCR</em><em>\\]</em> <em>safe emphasis</em> <em>\\[</em><em>End OCR</em><em>\\]</em>\n1. first item\n<script>globalThis.pwned=1</script>\n![x](file:///etc/passwd)\n<img src=http://evil.invalid/x onerror=alert(1)>\n[jump](javascript:alert(1))";
  root.render(createElement(SafeMarkdownPreview, { source: malicious }));
  await waitFor(() => window.document.body.textContent.includes("file:///etc/passwd"));
  assert.equal(window.document.querySelector("script,img,iframe,object,embed,link,a"), null);
  assert.equal((globalThis as { pwned?: number }).pwned, undefined);
  assert.ok(window.document.body.textContent.includes("<script>"));
  assert.equal(window.document.body.textContent.includes("<em>"), false);
  assert.equal(window.document.body.textContent.includes("Image OCR"), false);
  assert.equal(window.document.body.textContent.includes("End OCR"), false);
  assert.ok(window.document.body.textContent.includes("safe emphasis"));
  assert.equal(window.document.querySelectorAll(".markdown-preview em").length, 1);
  assert.equal(window.document.querySelector(".preview-list-item.ordered .preview-list-marker")?.textContent, "1.");
  root.render(createElement(SafeMarkdownPreview, { source: "\\*[Image OCR\\] Visible content \\[End OCR\\]\\*" }));
  await waitFor(() => window.document.body.textContent.includes("Visible content"));
  assert.equal(window.document.body.textContent.includes("Image OCR"), false);
  assert.equal(window.document.body.textContent.includes("End OCR"), false);
  root.render(createElement(SafeMarkdownPreview, { source: Array.from({ length: 2_100 }, () => "line").join("\n") }));
  await waitFor(() => window.document.body.textContent.includes("preview block limit reached"));
  root.render(createElement(JsonTree, { value: { provenance: { source: "local" }, blocks: Array.from({ length: 250 }, (_, index) => ({ index })) } }));
  await waitFor(() => window.document.body.textContent.includes("provenance"));
  assert.ok(window.document.body.textContent.includes("more entries"));
});

test("result dialog provides reading and source views with a closed details drawer", async () => {
  const window = installWindow(); const completed = task("succeeded");
  window.history.replaceState(null, "", `/results/${completed.id}`);
  Object.assign(completed, { displayName: "Quarterly report.pdf", format: "pdf", batchId: "b".repeat(32) });
  completed.artifacts = [
    { storageKey: "b".repeat(32), kind: "markdown", byteLen: 40, sha256: "c".repeat(64) },
    { storageKey: "f".repeat(32), kind: "diagnostics", byteLen: 28, sha256: "a".repeat(64) },
    { storageKey: "d".repeat(32), kind: "asset", byteLen: 12, sha256: "e".repeat(64), assetId: "image-1", filename: "diagram.png", mediaType: "image/png" },
  ];
  const sibling = { ...completed, id: "c".repeat(32), displayName: "Appendix.pdf" };
  let previewCalls = 0;
  const markdown = "<a id=\"pdf-page-1\"></a>\n# Quarterly report\n\n| <strong>Item</strong> | **Amount** |\n| --- | ---: |\n| Revenue | 120 |\n\n<img src=file:///secret>";
  const api: ApiClient = { ...availableApi, async getTask() { return completed; }, async listTasks(filters) { assert.equal(filters?.batchId, completed.batchId); return { tasks: [completed, sibling] }; }, async preview() { previewCalls += 1; return { text: markdown, truncated: true, contentType: "text/markdown" }; } };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitFor(() => previewCalls === 1 && window.document.body.textContent.includes("Quarterly report"));
  assert.ok(window.document.querySelector('.result-dialog[role="dialog"]'));
  assert.equal(window.document.querySelector(".result-dialog-backdrop")?.parentElement, window.document.body, "the viewport dialog must escape animated route containers");
  assert.ok(window.document.body.textContent.includes("Add documents"), "the workbench remains mounted behind the result");
  assert.equal(window.document.querySelector(".result-drawer"), null, "details must not compete with the document by default");
  assert.ok(window.document.querySelector(".preview-table-scroll table"));
  assert.equal(window.document.querySelectorAll(".preview-table-scroll th strong").length, 2);
  assert.equal(window.document.body.textContent.includes('pdf-page-1'), false);
  assert.equal(window.document.querySelector(".markdown-preview img,.markdown-preview script,.markdown-preview a"), null);
  assert.ok(window.document.body.textContent.includes("Large preview truncated"));
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Markdown source")!.click();
  await waitFor(() => Boolean(window.document.querySelector(".markdown-source")));
  assert.ok(window.document.querySelector(".markdown-source")?.textContent.includes("| Revenue |"));
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Details and resources")!.click();
  await waitFor(() => window.document.body.textContent.includes("Resources (1)"));
  const axe = (await import("axe-core")).default; const result = await axe.run(window.document);
  assert.deepEqual(result.violations.map((violation) => violation.id), []);
});

test("failed results keep the document area stable and expose retry beside the failure", async () => {
  const window = installWindow();
  const failed = { ...task("failed"), displayName: "recording.webm", workflow: "meetingTranscript" as const,
    diagnostics: [{ code: "componentUnavailable" }] };
  window.history.replaceState(null, "", `/meetings/results/${failed.id}`);
  const api: ApiClient = { ...availableApi, async getTask() { return failed; } };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "A required local dependency is not ready");
  assert.equal(window.document.querySelector(".result-drawer"), null);
  assert.equal(window.document.querySelector(".result-body")?.classList.contains("drawer-open"), false);
  assert.ok([...window.document.querySelectorAll(".result-empty button")].some((button) => button.textContent === "Retry"));
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Details and resources")!.click();
  await waitFor(() => Boolean(window.document.querySelector(".result-drawer")));
});

test("meeting speaker names rerender artifacts through generation CAS without rerunning transcription", async () => {
  const window = installWindow();
  const completed = { ...task("succeeded"), workflow: "meetingTranscript" as const,
    displayName: "meeting.webm", format: "audio" as const };
  completed.artifacts = [
    { storageKey: "b".repeat(32), kind: "markdown", byteLen: 40, sha256: "c".repeat(64) },
  ];
  window.history.replaceState(null, "", `/meetings/results/${completed.id}`);
  let generation = 0;
  let name = "Speaker 1";
  let relabel: [number, Record<string, string>] | undefined;
  let transcriptUploads = 0;
  const api: ApiClient = {
    ...availableApi,
    async capabilitySnapshot() { return capabilitySnapshot(true); },
    async getTask() { return { ...completed, artifactGeneration: generation }; },
    async listTasks() { return { tasks: [completed] }; },
    async preview() { return { text: `# Transcript\n\n[00:00] ${name}: Hello`, truncated: false, contentType: "text/markdown" }; },
    async speakerLabels() { return { schemaVersion: 1, artifactGeneration: generation,
      speakers: [{ id: "speaker-1", name }] }; },
    async relabelSpeakers(_id, expectedGeneration, speakers) {
      relabel = [expectedGeneration, speakers];
      assert.equal(expectedGeneration, generation);
      name = speakers["speaker-1"]!; generation += 1;
      return { ...completed, artifactGeneration: generation };
    },
    async uploadMeeting() { transcriptUploads += 1; return completed; },
  };
  const root = trackedRoot(window.document.getElementById("app")!);
  root.render(createElement(App, { api }));
  await waitForText(window, "Speaker names");
  const input = window.document.querySelector<HTMLInputElement>(".speaker-editor input")!;
  input.focus();
  Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!.call(input, "张三");
  input.dispatchEvent(new window.InputEvent("input", { bubbles: true, data: "张三", inputType: "insertText" }));
  input.dispatchEvent(new window.Event("change", { bubbles: true }));
  const save = [...window.document.querySelectorAll<HTMLButtonElement>("button")]
    .find((button) => button.textContent === "Save names")!;
  await waitFor(() => save.disabled === false);
  save.click();
  await waitForText(window, "Speaker names and downloadable artifacts were updated.");
  assert.deepEqual(relabel, [0, { "speaker-1": "张三" }]);
  assert.equal(generation, 1);
  assert.equal(transcriptUploads, 0);
  assert.ok(window.document.body.textContent.includes("张三: Hello"));
});

test("recent history opens a result dialog with irreversible task actions", async () => {
  const window = installWindow();
  const completed = { ...task("succeeded"), displayName: "Quarterly report.pdf", format: "pdf" as const }; let pinned = false; let deleted = false; let warning = "";
  window.confirm = (message?: string) => { warning = message ?? ""; return true; };
  const api: ApiClient = {
    ...availableApi,
    async listTasks() { return { tasks: [completed] }; },
    async getTask() { return completed; },
    async setPinned(id, value) { pinned = value; return { ...completed, id, pinned: value }; },
    async deleteTask() { deleted = true; },
  };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "Quarterly report.pdf");
  window.document.querySelector<HTMLButtonElement>(".recent-task-link")!.click();
  await waitFor(() => Boolean(window.document.querySelector(".result-dialog")));
  const menu = window.document.querySelector<HTMLElement>(".task-menu")!;
  const trigger = menu.querySelector<HTMLButtonElement>(".menu-trigger")!;
  trigger.click();
  await waitFor(() => Boolean(menu.querySelector(".task-menu-popover"))).catch(() => { throw new Error("menu did not open"); });
  window.document.body.dispatchEvent(new window.MouseEvent("pointerdown", { bubbles: true }));
  await waitFor(() => !menu.querySelector(".task-menu-popover")).catch(() => { throw new Error("outside press did not close menu"); });
  trigger.click();
  await waitFor(() => Boolean(menu.querySelector(".task-menu-popover")));
  window.document.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  await waitFor(() => !menu.querySelector(".task-menu-popover") && window.document.activeElement === trigger).catch(() => { throw new Error("Escape did not close and refocus menu"); });
  trigger.click();
  await waitFor(() => Boolean(menu.querySelector(".task-menu-popover")));
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Pin")!.click();
  await waitFor(() => pinned).catch(() => { throw new Error("pin action was not invoked"); });
  await waitFor(() => !menu.querySelector(".task-menu-popover")).catch(() => { throw new Error("menu did not close after pin"); });
  trigger.click();
  await waitFor(() => Boolean(menu.querySelector(".task-menu-popover")) && menu.textContent.includes("Unpin")).catch(() => { throw new Error("pinned state was not rendered when the menu reopened"); });
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Delete permanently")!.click();
  await waitFor(() => deleted && window.document.querySelector(".result-dialog") === null);
  assert.ok(warning.includes("cannot be undone"));
  assert.equal(window.document.querySelector(".recent-task-link"), null);
});
test("immediate cleanup requires irreversible confirmation and reports reclaimed capacity", async () => {
  const window2 = installWindow();
  let cleanups = 0;
  let listCalls = 0;
  let warning = "";
  window2.confirm = (message) => {
    warning = message ?? "";
    return true;
  };
  const api = {
    ...availableApi,
    async listTasks() { listCalls += 1; return { tasks: listCalls === 1 ? [{ ...task("succeeded"), displayName: "old.md", format: "markdown" as const }] : [] }; },
    async cleanup() {
      cleanups += 1;
      return { schemaVersion: 1 as const, deletedTasks: 2, reclaimedBytes: 1572864 };
    }
  };
  const root = trackedRoot(window2.document.getElementById("app")!);
  root.render(createElement(App, { api }));
  await waitForText(window2, "old.md");
  window2.document.querySelector<HTMLButtonElement>('button[aria-label="Clean up now"]')!.click();
  await waitFor(() => cleanups === 1 && window2.document.body.textContent.includes("1.5 MiB"));
  assert.ok(warning.includes("cannot be undone"));
  assert.ok(window2.document.querySelector(".history-rail .history-rail-feedback")?.textContent.includes("1.5 MiB"));
  assert.equal(window2.document.querySelector(".upload-card .picker-feedback"), null);
  const axe = (await import("axe-core")).default;
  assert.deepEqual((await axe.run(window2.document)).violations.map((violation) => violation.id), []);
});

test("workbench keeps the current batch and conversion controls in one route", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  let cancelled = 0; let uploaded = 0;
  const api: ApiClient = {
    ...availableApi,
    async upload(_file, _options, batchId) { uploaded += 1; return { ...task("running", String(uploaded).repeat(32)), batchId }; },
    async cancel(id) { cancelled += 1; return task("cancelled", id); },
    async watchTask() {},
  };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "Add documents");
  assert.equal(window.document.querySelector(".history-route,.result-route"), null);
  const inputs = [...window.document.querySelectorAll<HTMLInputElement>('input[type="file"]')];
  assert.equal(inputs.length, 2); assert.equal(inputs[0]!.multiple, true); assert.equal(inputs[1]!.hasAttribute("webkitdirectory"), true);
  const zone = window.document.getElementById("upload-zone")!; let pickerClicks = 0; inputs[0]!.click = () => { pickerClicks += 1; };
  zone.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  zone.dispatchEvent(new window.KeyboardEvent("keydown", { key: " ", bubbles: true }));
  assert.equal(pickerClicks, 2);
  const drop = new window.Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: { files: [new File(["one"], "one.md"), new File(["two"], "two.md")] } });
  zone.dispatchEvent(drop); await waitForText(window, "Selected (2)");
  const convert = [...window.document.querySelectorAll("button")].find((button) => button.textContent?.startsWith("Start conversion (2)"))!;
  convert.click(); await waitFor(() => uploaded === 2);
  const cancel = window.document.querySelector<HTMLButtonElement>('button[aria-label="Cancel one.md"]')!;
  cancel.click(); await waitFor(() => cancelled === 1);
  assert.equal(window.document.querySelectorAll(".current-batch li").length, 2);
  assert.equal(window.document.querySelector(".history-table-shell"), null);
});

test("workbench keeps OCR neutral while the fast capability snapshot is pending", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  let legacyStatusRequests = 0;
  let resolveSnapshot!: (snapshot: ReturnType<typeof capabilitySnapshot>) => void;
  const pending = new Promise<ReturnType<typeof capabilitySnapshot>>((resolve) => { resolveSnapshot = resolve; });
  const api: ApiClient = { ...availableApi, async capabilitySnapshot() { return pending; }, async status() { legacyStatusRequests += 1; return availableApi.status(); } };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "Add documents");
  assert.ok(window.document.querySelector(".capability-item")?.parentElement?.textContent?.includes("Checking"));
  assert.equal(window.document.body.textContent.includes("Install and verify"), false);
  assert.equal(legacyStatusRequests, 0, "the workbench must not block the fast snapshot on the legacy full status route");
  resolveSnapshot(capabilitySnapshot(true));
  await waitForText(window, "Local plugin");
  assert.equal(window.document.body.textContent.includes("Install and verify"), false);
});

test("workbench rejects unsupported files before upload and explains terminal failures", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  let uploads = 0;
  const failed = { ...task("failed"), displayName: "broken.pdf", format: "pdf" as const, diagnostics: [{ code: "malformed" }] };
  const api: ApiClient = {
    ...availableApi,
    async listTasks() { return { tasks: [failed] }; },
    async upload() { uploads += 1; return task("running"); },
  };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "broken.pdf");
  assert.ok(window.document.body.textContent.includes("The file is damaged or its format is invalid"));
  const input = window.document.querySelector<HTMLInputElement>('input[type="file"]')!;
  assert.ok(input.accept.includes(".pdf"));
  assert.ok(input.accept.includes(".pptm"));
  assert.ok(input.accept.includes(".msg"));
  assert.equal(input.accept.includes(".plist"), false);
  const drop = new window.Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", {
    value: { files: [new File(["plist"], "settings.plist"), new File(["markdown"], "notes.md")] },
  });
  window.document.getElementById("upload-zone")!.dispatchEvent(drop);
  await waitForText(window, "Skipped unsupported files: settings.plist");
  assert.equal(window.document.querySelectorAll(".current-batch li").length, 1);
  assert.ok(window.document.body.textContent.includes("notes.md"));
  assert.equal(window.document.body.textContent.includes("settings.plist"), true, "the rejection message should name the skipped input");
  assert.ok(window.document.querySelector(".upload-card .picker-feedback")?.textContent?.includes("settings.plist"));
  assert.equal(window.document.querySelector(".control-column .message-bar")?.textContent?.includes("settings.plist"), false);
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Start conversion (1)")!.click();
  await waitFor(() => uploads === 1);
});

test("workbench separates the current batch from scrollable recent history", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  const historyTasks = [
    { ...task("succeeded", "1".repeat(32)), displayName: "recent.pdf", format: "pdf" as const, updatedAtMs: 4 },
    { ...task("failed", "2".repeat(32)), displayName: "failed.docx", format: "docx" as const, updatedAtMs: 3 },
    { ...task("succeeded", "3".repeat(32)), displayName: "older.png", format: "image" as const, updatedAtMs: 2 },
    { ...task("succeeded", "4".repeat(32)), displayName: "hidden.md", format: "markdown" as const, updatedAtMs: 1 },
  ];
  const api: ApiClient = { ...availableApi, async listTasks() { return { tasks: historyTasks }; } };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "Recent");
  const drop = new window.Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: { files: [new File(["current"], "current.md")] } });
  window.document.getElementById("upload-zone")!.dispatchEvent(drop);
  await waitForText(window, "Selected (1)");
  assert.equal(window.document.querySelectorAll(".recent-history li").length, 4);
  assert.equal(window.document.body.textContent.includes("hidden.md"), true);
  assert.ok(window.document.querySelector(".current-batch-scroll"));
  assert.ok(window.document.querySelector(".recent-history-scroll"));
  assert.equal(window.document.querySelector(".queue-scroll"), null);
  const current = window.document.querySelector(".current-batch")!;
  const recent = window.document.querySelector(".recent-history")!;
  assert.ok(current.compareDocumentPosition(recent) & window.Node.DOCUMENT_POSITION_FOLLOWING);
  window.document.querySelector<HTMLButtonElement>(".recent-task-link")!.click();
  await waitFor(() => Boolean(window.document.querySelector(".result-dialog")));
  assert.equal(window.location.pathname, `/results/${historyTasks[0]!.id}`);
});

test("history paginates in place and loads records beyond the first server page", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  const records = Array.from({ length: 8 }, (_, index) => ({
    ...task("succeeded", String(index + 1).padStart(32, "0")),
    displayName: `archive-${index + 1}.pdf`, format: "pdf" as const, updatedAtMs: 100 - index,
  }));
  let requests = 0;
  const api: ApiClient = {
    ...availableApi,
    async listTasks(filters) {
      requests += 1;
      if (!filters?.after) return { tasks: records.slice(0, 7), nextCursor: { updatedAtMs: 94, id: records[6]!.id } };
      return { tasks: records.slice(7) };
    },
  };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "archive-1.pdf");
  await waitFor(() => requests >= 2);
  assert.equal(window.document.querySelectorAll(".recent-history li").length, 6);
  assert.equal(window.document.body.textContent.includes("archive-8.pdf"), false);
  window.document.querySelector<HTMLButtonElement>('.history-rail-footer button[aria-label="Next"]')!.click();
  await waitForText(window, "archive-8.pdf");
  assert.equal(window.document.querySelectorAll(".recent-history li").length, 2);
  assert.equal(window.document.querySelector(".history-rail")?.textContent?.includes("2/2"), true);
  assert.equal(window.document.querySelector(".history-rail")?.textContent?.includes("View all"), false);
});

test("completed current-batch rows open their result from the whole row", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  const completed = { ...task("succeeded"), displayName: "contract.md", format: "markdown" as const };
  const api: ApiClient = { ...availableApi, async upload(_file, _options, batchId) { return { ...completed, batchId }; } };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "Add documents");
  const drop = new window.Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: { files: [new File(["contract"], "contract.md")] } });
  window.document.getElementById("upload-zone")!.dispatchEvent(drop);
  await waitForText(window, "Selected (1)");
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Start conversion (1)")!.click();
  await waitFor(() => Boolean(window.document.querySelector(".result-dialog")));
  window.document.querySelector<HTMLButtonElement>('.result-dialog button[aria-label="Close"]')!.click();
  await waitFor(() => window.document.querySelector(".result-dialog") === null);
  const row = window.document.querySelector<HTMLButtonElement>(".current-task-link")!;
  assert.ok(row.textContent.includes("contract.md"));
  row.focus(); row.click();
  await waitFor(() => Boolean(window.document.querySelector(".result-dialog")));
  window.document.querySelector<HTMLButtonElement>('.result-dialog button[aria-label="Close"]')!.click();
  await waitFor(() => window.document.activeElement === row);
});

test("root workbench automatically opens the first successful result dialog", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/");
  const completed = { ...task("succeeded"), displayName: "contract.md", format: "markdown" as const };
  const api: ApiClient = { ...availableApi, async upload(_file, _options, batchId) { return { ...completed, batchId }; } };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "Add documents");
  const drop = new window.Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: { files: [new File(["contract"], "contract.md")] } });
  window.document.getElementById("upload-zone")!.dispatchEvent(drop);
  await waitForText(window, "Selected (1)");
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Start conversion (1)")!.click();
  await waitFor(() => Boolean(window.document.querySelector(".result-dialog")));
  assert.equal(window.location.pathname, `/results/${completed.id}`);
});

test("local workbench keeps implementation limits and network policy out of the normal flow", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api: availableApi }));
  await waitForText(window, "Conversion settings");
  assert.ok(!window.document.body.textContent.includes("Open advanced settings"));
  assert.ok(!window.document.body.textContent.includes("Input file limit"));
  assert.ok(!window.document.body.textContent.includes("Memory limit"));
  assert.ok(!window.document.body.textContent.includes("Allowed hosts"));
  assert.ok(!window.document.body.textContent.includes("Allow network access"));
});

test("remote OCR requires nearby network and provider authorization without enabling unrelated AI modes", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  const uploaded: WorkbenchOptions[] = [];
  const api: ApiClient = {
    ...availableApi,
    async capabilitySnapshot() { const base = capabilitySnapshot(false); return { ...base, capabilities: base.capabilities.map((item) => item.id === "ocr" ? { ...item, status: "ready", currentSource: "provider:bailian/ocr", currentSourceName: "Bailian", sources: ["provider:bailian/ocr", "plugin:official.ocr/ocr", "off"] } : item) }; },
    async status() { return { ...(await availableApi.status()), imageOcr: { available: true, code: "available", detail: "ready" } }; },
    async admin() { return { ...(await availableApi.admin()), capabilities: [{ id: "ocr", status: "ready", localStatus: "ready", currentSource: "provider:bailian/ocr", sources: ["provider:bailian/ocr", "plugin:official.ocr/ocr", "off"] }] }; },
    async upload(_file, options) { uploaded.push(options); return task(); },
    async watchTask() {},
  };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "Bailian");
  const drop = new window.Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: { files: [new File(["image"], "scan.jpg", { type: "image/jpeg" })] } });
  window.document.getElementById("upload-zone")!.dispatchEvent(drop);
  await waitForText(window, "Selected (1)");
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Start conversion (1)")!.click();
  await waitForText(window, "The selected image-recognition source needs network access");
  const grant = [...window.document.querySelectorAll("label")]
    .find((label) => label.textContent?.includes("Allow this conversion to use the selected AI service"))!
    .querySelector<HTMLInputElement>('input[type="checkbox"]')!;
  assert.equal(grant.checked, false);
  grant.click();
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Start conversion (1)")!.click();
  await waitFor(() => uploaded.length === 1);
  assert.equal(uploaded[0]!.aiMode, "off");
  assert.equal(uploaded[0]!.networkMode, "unrestricted");
  assert.equal(uploaded[0]!.authorizeProvider, true);
});

test("meeting recording is an independent route and media never enters the document workbench", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  const uploads: Array<[string, boolean]> = []; let cancellations = 0; let cancelRequested = false;
  let emitTaskEvent: Parameters<ApiClient["watchTask"]>[1] | undefined;
  const api: ApiClient = {
    ...availableApi,
    async capabilitySnapshot() { return capabilitySnapshot(true); },
    async status() { return { ...(await availableApi.status()), audioTranscription: { available: true, code: "available", detail: "ready" }, speakerDiarization: { available: true, code: "available", detail: "ready" } }; },
    async uploadMeeting(file, options) {
      uploads.push([file.name, options.diarize]);
      return { ...task("running", "b".repeat(32)), workflow: "meetingTranscript" };
    },
    async cancel(id) { cancellations += 1; cancelRequested = true; return { ...task("running", id), workflow: "meetingTranscript" }; },
    async getTask(id) { return { ...task(uploads.length > 1 ? "failed" : cancelRequested ? "cancelled" : "running", id), workflow: "meetingTranscript" }; },
    async watchTask(_id, onEvent, signal) {
      emitTaskEvent = onEvent;
      await new Promise<void>((resolve) => signal.addEventListener("abort", () => resolve(), { once: true }));
    },
  };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "Add documents");
  const drop = new window.Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: { files: [new File(["audio"], "recording.m4a")] } });
  window.document.getElementById("upload-zone")!.dispatchEvent(drop);
  await waitForText(window, "recording.m4a");
  assert.equal(window.document.body.textContent.includes("Selected (1)"), false);
  [...window.document.querySelectorAll("a")]
    .find((link) => link.textContent === "Speech transcription")!
    .dispatchEvent(new window.MouseEvent("click", { bubbles: true, cancelable: true }));
  await waitForText(window, "Record or import");
  assert.equal(window.document.getElementById("upload-zone"), null);
  assert.ok([...window.document.querySelectorAll("button")].some((button) => button.textContent?.includes("Start recording")));
  const input = window.document.querySelector<HTMLInputElement>('.meeting-route input[type="file"]')!;
  Object.defineProperty(input, "files", { value: [new File(["audio"], "meeting.m4a", { type: "audio/mp4" })], configurable: true });
  input.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitForText(window, "meeting.m4a");
  const stableActionPanel = window.document.querySelector(".transcript-action-panel");
  const speechCapabilityStrip = window.document.querySelector(".speech-capability-strip");
  assert.ok(speechCapabilityStrip?.closest(".transcript-control-column"));
  assert.ok(stableActionPanel?.parentElement?.classList.contains("transcript-control-column"));
  assert.equal(stableActionPanel?.closest(".transcript-card"), null);
  [...window.document.querySelectorAll("button")]
    .find((button) => button.textContent?.includes("Create transcript"))!
    .click();
  await waitFor(() => uploads.length === 1);
  assert.deepEqual(uploads, [["meeting.m4a", true]]);
  await waitForText(window, "Cancel transcription");
  assert.equal(window.document.querySelector(".transcript-action-panel"), stableActionPanel);
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Cancel transcription")!.click();
  await waitFor(() => cancellations === 1 && window.document.body.textContent.includes("Cancelling"));
  assert.ok([...window.document.querySelectorAll("button")].some((button) => button.textContent === "Cancelling" && button.disabled));
  emitTaskEvent?.({ schemaVersion: 1, sequence: 1, taskId: "b".repeat(32), kind: "progress", status: "cancelled",
    progressMillionths: 250_000, terminal: true, execution: { stage: "completed", basisPoints: 2_500,
      completedUnits: 1, totalUnits: 4, message: null } });
  await waitForText(window, "Cancelled");
  assert.equal(window.document.querySelector(".transcript-action-panel"), stableActionPanel);
  await waitForText(window, "Task details");
  assert.ok(stableActionPanel?.textContent?.includes("Task details"));
  Object.defineProperty(input, "files", { value: [new File(["audio"], "another.webm", { type: "audio/webm" })], configurable: true });
  input.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitForText(window, "another.webm");
  assert.ok(stableActionPanel?.textContent?.includes("Create transcript"));
  assert.equal(stableActionPanel?.textContent?.includes("Transcribe again"), false);
  [...stableActionPanel!.querySelectorAll("button")].find((button) => button.textContent?.includes("Create transcript"))!.click();
  await waitFor(() => uploads.length === 2);
  await waitForText(window, "Failed");
  await waitFor(() => stableActionPanel?.textContent?.includes("Task details") === true);
  assert.ok(stableActionPanel?.textContent?.includes("Task details"));
});

test("meeting page keeps recording primary and setup feedback beside transcript controls", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/meetings");
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api: availableApi }));
  await waitForText(window, "Record or import");
  await waitForText(window, "Prepare audio components");
  const start = [...window.document.querySelectorAll("button")].find((button) => button.textContent?.includes("Start recording"));
  const prepare = [...window.document.querySelectorAll<HTMLAnchorElement>("a")].find((link) => link.textContent?.includes("Prepare audio components"));
  assert.ok(start?.closest(".recorder-console"));
  assert.ok(prepare?.closest(".meeting-options"));
  assert.equal(prepare?.getAttribute("href"), "/admin/capabilities");
  const source = window.document.querySelector<HTMLSelectElement>(".recording-source select")!;
  assert.deepEqual([...source.options].map((option) => option.textContent), [
    "Microphone only", "Computer audio only", "Microphone + computer audio",
  ]);
  source.value = "system"; source.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitForText(window, "Video is never saved.");
  const diarize = window.document.querySelector<HTMLInputElement>('.meeting-options input[type="checkbox"]')!;
  assert.equal(diarize.disabled, true);
  assert.equal(diarize.checked, false);
});

test("meeting keeps speech capabilities neutral while the fast snapshot is pending", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/meetings");
  let resolveSnapshot!: (snapshot: ReturnType<typeof capabilitySnapshot>) => void;
  const pending = new Promise<ReturnType<typeof capabilitySnapshot>>((resolve) => { resolveSnapshot = resolve; });
  const api: ApiClient = { ...availableApi, async capabilitySnapshot() { return pending; } };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "Record or import");
  assert.ok(window.document.body.textContent.includes("Checking"));
  assert.equal(window.document.body.textContent.includes("Prepare audio components"), false);
  assert.equal(window.document.body.textContent.includes("Speaker component is not ready"), false);
  resolveSnapshot(capabilitySnapshot(true));
  await waitForText(window, "Local plugin");
  assert.equal(window.document.body.textContent.includes("Prepare audio components"), false);
});

test("remote transcription requires a one-upload grant beside transcript controls", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/meetings");
  const uploads: MeetingOptions[] = [];
  const api: ApiClient = {
    ...availableApi,
    async capabilitySnapshot() { const base = capabilitySnapshot(false); return { ...base, capabilities: base.capabilities.map((item) => item.id === "transcription" ? { ...item, status: "ready", currentSource: "provider:bailian/transcription", currentSourceName: "Bailian", sources: ["provider:bailian/transcription", "plugin:official.media/transcription", "off"] } : item.id === "diarization" ? { ...item, status: "ready", localStatus: "ready", currentSource: "plugin:official.media/diarization", currentSourceName: "Local speech", sources: ["plugin:official.media/diarization", "off"] } : item) }; },
    async status() { return { ...(await availableApi.status()), audioTranscription: { available: true, code: "available", detail: "ready" }, speakerDiarization: { available: true, code: "available", detail: "ready" } }; },
    async admin() { return { ...(await availableApi.admin()), capabilities: [
      { id: "transcription", status: "ready", localStatus: "ready", currentSource: "provider:bailian/transcription", sources: ["provider:bailian/transcription", "plugin:official.media/transcription", "off"] },
      { id: "diarization", status: "ready", localStatus: "ready", currentSource: "plugin:official.media/diarization", sources: ["plugin:official.media/diarization", "off"] },
    ] }; },
    async uploadMeeting(_file, options) { uploads.push(options); return { ...task(), workflow: "meetingTranscript" }; },
    async watchTask() {},
  };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "Allow this audio to use the selected AI service and network transcription");
  const input = window.document.querySelector<HTMLInputElement>('.meeting-route input[type="file"]')!;
  Object.defineProperty(input, "files", { value: [new File(["audio"], "meeting.webm", { type: "audio/webm" })] });
  input.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitForText(window, "meeting.webm");
  [...window.document.querySelectorAll("button")].find((button) => button.textContent?.includes("Create transcript"))!.click();
  await waitForText(window, "Confirm use of the selected AI service for this upload.");
  const grant = window.document.querySelector<HTMLInputElement>(".meeting-provider-grant input")!;
  grant.click();
  [...window.document.querySelectorAll("button")].find((button) => button.textContent?.includes("Create transcript"))!.click();
  await waitFor(() => uploads.length === 1);
  assert.equal(uploads[0]!.authorizeProvider, true);
});

test("Chinese meeting UI defaults to Simplified Chinese without overriding explicit choices", async () => {
  const window = installWindow(["zh-CN"]); window.history.replaceState(null, "", "/meetings");
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api: availableApi }));
  await waitForText(window, "转写语言");
  const transcriptLanguage = window.document.querySelector<HTMLSelectElement>(".transcript-language select")!;
  assert.equal(transcriptLanguage.value, "zh-Hans");
  transcriptLanguage.value = "zh-Hant";
  transcriptLanguage.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitFor(() => transcriptLanguage.value === "zh-Hant");
  const locale = [...window.document.querySelectorAll<HTMLSelectElement>("select")]
    .find((select) => [...select.options].some((option) => option.value === "en"))!;
  locale.value = "en";
  locale.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitFor(() => window.document.documentElement.lang === "en");
  assert.equal(transcriptLanguage.value, "zh-Hant");
});

test("workbench explains upload rejection without exposing an internal code", async () => {
  const window = installWindow(); window.history.replaceState(null, "", "/workbench");
  const api: ApiClient = {
    ...availableApi,
    async upload() { throw new ApiError("invalidTaskOptions"); },
  };
  const root = trackedRoot(window.document.getElementById("app")!); root.render(createElement(App, { api }));
  await waitForText(window, "Add documents");
  const zone = window.document.getElementById("upload-zone")!;
  const drop = new window.Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: { files: [new File(["contract"], "contract.md")] } });
  zone.dispatchEvent(drop); await waitForText(window, "Selected (1)");
  [...window.document.querySelectorAll("button")].find((button) => button.textContent === "Start conversion (1)")!.click();
  await waitForText(window, "contract.md: The conversion settings are invalid");
  assert.equal(window.document.body.textContent.includes("invalidTaskOptions"), false);
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
  window.close();
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
  await waitFor(() => window.document.body.textContent.includes("系统就绪"));
  assert.equal(window.document.documentElement.lang, "zh-CN");
  assert.equal(window.document.documentElement.dir, "ltr");
  const language = window.document.querySelector<HTMLSelectElement>("select")!;
  language.focus();
  language.value = "en";
  language.dispatchEvent(new window.Event("change", { bubbles: true }));
  await waitFor(() => window.document.documentElement.lang === "en");
  assert.equal(window.document.activeElement, language);
  assert.match(window.document.title, /Conversion workbench/);
});

test("real mounted App has no axe violations; geometry-incomplete rules are not treated as coverage", async () => {
  const window = installWindow();
  const root = trackedRoot(window.document.getElementById("app")!);
  root.render(createElement(App, { api: availableApi }));
  await waitFor(() => window.document.body.textContent.includes("System ready"));
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
  root.render(createElement(App, { api: { ...availableApi, capabilitySnapshot: async () => { throw new ApiError("unreachable"); } } }));
  await waitFor(() => window.document.body.textContent.includes("Needs attention"));
  assert.ok(window.document.querySelector('.service-badge[role="status"]'));
  assert.equal(window.document.body.textContent.includes("The page encountered a problem"), false);
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

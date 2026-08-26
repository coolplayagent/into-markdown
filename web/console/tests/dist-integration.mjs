import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test, { afterEach } from "node:test";
import { pathToFileURL } from "node:url";
import { dirname, isAbsolute, resolve } from "node:path";
import { Window } from "happy-dom";

const configuredDist = process.env.INTO_MD_DIST;
const logicalManifest = `${configuredDist.replaceAll("\\", "/")}/asset-manifest.json`;

async function resolveRunfile(logical) {
  if (isAbsolute(logical)) return logical;
  if (process.env.RUNFILES_MANIFEST_FILE) {
    const entries = await readFile(process.env.RUNFILES_MANIFEST_FILE, "utf8");
    const prefix = `_main/${logical} `;
    const match = entries.split(/\r?\n/u).find((entry) => entry.startsWith(prefix));
    if (match) return match.slice(prefix.length);
  }
  if (process.env.RUNFILES_DIR) return resolve(process.env.RUNFILES_DIR, "_main", logical);
  return resolve(logical);
}

const manifestPath = await resolveRunfile(logicalManifest);
const distDirectory = dirname(manifestPath);
const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
const app = manifest.assets.find((asset) => /^\/assets\/app\.[a-f0-9]{16}\.js$/.test(asset.path));
const bootstrap = manifest.assets.find((asset) => /^\/assets\/bootstrap\.[a-f0-9]{16}\.js$/.test(asset.path));
assert.ok(app && bootstrap);
let activeWindow;

afterEach(() => {
  activeWindow?.close();
  activeWindow = undefined;
});

function installWindow(fragment = "") {
  const window = new Window({ url: `http://127.0.0.1:1/workbench${fragment}` });
  activeWindow = window;
  window.setInterval = () => 0;
  window.clearInterval = () => {};
  Object.defineProperty(window.navigator, "languages", { value: ["en"], configurable: true });
  window.document.body.innerHTML = '<div id="app"></div>';
  for (const [name, value] of Object.entries({
    window, document: window.document, navigator: window.navigator,
    history: window.history, location: window.location, Node: window.Node,
    Element: window.Element, HTMLElement: window.HTMLElement,
  })) Object.defineProperty(globalThis, name, { value, writable: true, configurable: true });
  Reflect.deleteProperty(globalThis, "React");
  return window;
}

function waitFor(predicate, timeout = 1_000) {
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

function mockPayload(input) {
  const path = String(input);
  if (path.includes("/api/tasks?")) return { schemaVersion: 1, tasks: [] };
  if (path.includes("/api/capabilities/status")) return {
    schemaVersion: 2,
    generation: 1,
    checking: false,
    capabilities: [
      { id: "legacy-office", name: "Legacy Office", status: "not-installed", localStatus: "not-installed", currentSource: "off", currentSourceName: "Off", sources: ["off"] },
      { id: "ocr", name: "Image OCR", status: "not-installed", localStatus: "not-installed", currentSource: "off", currentSourceName: "Off", sources: ["off"] },
      { id: "transcription", name: "Speech transcription", status: "not-installed", localStatus: "not-installed", currentSource: "off", currentSourceName: "Off", sources: ["off"] },
      { id: "diarization", name: "Speaker identification", status: "not-installed", localStatus: "not-installed", currentSource: "off", currentSourceName: "Off", sources: ["off"] },
    ],
  };
  return {
    schemaVersion: 1,
    localApi: { available: true, code: "available", detail: "ok" },
    documentConsole: { available: true, code: "available", detail: "ok" },
    imageOcr: { available: false, code: "notInstalled", detail: "setup required" },
    audioTranscription: { available: false, code: "notInstalled", detail: "setup required" },
    speakerDiarization: { available: false, code: "notInstalled", detail: "setup required" },
  };
}

test("checked-in production app mounts without a React global", async () => {
  const window = installWindow();
  globalThis.fetch = async (input) => new Response(JSON.stringify(mockPayload(input)), { headers: { "content-type": "application/json" } });
  const module = await import(pathToFileURL(resolve(distDirectory, app.path.slice(1))).href);
  module.startConsole("A".repeat(43));
  await waitFor(() => window.document.body.textContent.includes("System ready"));
  assert.equal("React" in globalThis, false);
});

test("checked-in production root contains token-bearing render failures without console or storage disclosure", async () => {
  const token = "R".repeat(43);
  const window = installWindow();
  const observed = [];
  const methods = ["debug", "error", "info", "log", "warn"];
  const originals = new Map(methods.map((method) => [method, console[method]]));
  for (const method of methods) {
    console[method] = (...values) => observed.push(values.map((value) => {
      if (value instanceof Error) return `${value.name}:${value.message}:${value.stack ?? ""}`;
      return String(value);
    }).join(" "));
  }
  try {
    const module = await import(pathToFileURL(resolve(distDirectory, app.path.slice(1))).href);
    function TokenBearingProviderFailure() { throw new Error(`provider rejected ${token}`); }
    module.startConsole(token, TokenBearingProviderFailure);
    await waitFor(() => window.document.body.textContent.includes("The page encountered a problem"));
    assert.equal(window.document.documentElement.outerHTML.includes(token), false);
    assert.equal(observed.join("\n").includes(token), false);
    assert.equal(window.localStorage.length, 0);
    assert.equal(window.sessionStorage.length, 0);
    assert.equal(JSON.stringify([window.localStorage, window.sessionStorage]).includes(token), false);
  } finally {
    for (const [method, original] of originals) console[method] = original;
  }
});

test("checked-in bootstrap completes hash clear, dynamic import, mount, and authenticated API", async () => {
  const token = "B".repeat(43);
  const window = installWindow(`#into-md-session=${token}`);
  let request;
  globalThis.fetch = async (input, init) => {
    request = [input, init];
    return new Response(JSON.stringify(mockPayload(input)), { headers: { "content-type": "application/json" } });
  };
  let bootstrapText = await readFile(resolve(distDirectory, bootstrap.path.slice(1)), "utf8");
  bootstrapText = bootstrapText.replace(app.path, pathToFileURL(resolve(distDirectory, app.path.slice(1))).href);
  await import(`data:text/javascript;base64,${Buffer.from(bootstrapText).toString("base64")}`);
  await waitFor(() => window.document.body.textContent.includes("System ready"));
  assert.equal(window.location.hash, "");
  assert.equal(window.document.documentElement.outerHTML.includes(token), false);
  assert.equal(window.localStorage.length, 0);
  assert.equal(window.sessionStorage.getItem("into-md.session"), token);
  assert.equal(request[1].headers["X-Into-Md-Session"], token);
});

test("bootstrap distinguishes handoff and generic startup failures without token reflection", async () => {
  const bootstrapText = await readFile(resolve(distDirectory, bootstrap.path.slice(1)), "utf8");
  const missing = installWindow("#bad");
  await import(`data:text/javascript;base64,${Buffer.from(bootstrapText).toString("base64")}#handoff`);
  assert.match(missing.document.body.textContent, /Session handoff is missing or invalid/);

  const token = "C".repeat(43);
  const failed = installWindow(`#into-md-session=${token}`);
  const unavailable = bootstrapText.replace(app.path, "data:text/javascript,throw%20new%20Error('secret')");
  await import(`data:text/javascript;base64,${Buffer.from(unavailable).toString("base64")}#startup`);
  await waitFor(() => failed.document.body.textContent.includes("could not start"));
  assert.equal(failed.document.body.textContent.includes("Session handoff"), false);
  assert.equal(failed.document.documentElement.outerHTML.includes(token), false);
});

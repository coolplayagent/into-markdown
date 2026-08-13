import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { pathToFileURL } from "node:url";
import { resolve } from "node:path";
import { Window } from "happy-dom";

const distDirectory = resolve(process.env.INTO_MD_DIST);
const manifest = JSON.parse(await readFile(resolve(distDirectory, "asset-manifest.json"), "utf8"));
const app = manifest.assets.find((asset) => /^\/assets\/app\.[a-f0-9]{16}\.js$/.test(asset.path));
const bootstrap = manifest.assets.find((asset) => /^\/assets\/bootstrap\.[a-f0-9]{16}\.js$/.test(asset.path));
assert.ok(app && bootstrap);

function installWindow(fragment = "") {
  const window = new Window({ url: `http://127.0.0.1:1/status${fragment}` });
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

test("checked-in production app mounts without a React global", async () => {
  const window = installWindow();
  globalThis.fetch = async () => new Response(JSON.stringify({
    schemaVersion: 1,
    localApi: { available: true, code: "available", detail: "ok" },
    documentConsole: { available: false, code: "componentUnavailable", detail: "not installed" },
  }), { headers: { "content-type": "application/json" } });
  const module = await import(pathToFileURL(resolve(distDirectory, app.path.slice(1))).href);
  module.startConsole("A".repeat(43));
  await waitFor(() => window.document.body.textContent.includes("Local API available"));
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
    return new Response(JSON.stringify({
      schemaVersion: 1,
      localApi: { available: true, code: "available", detail: "ok" },
      documentConsole: { available: false, code: "componentUnavailable", detail: "not installed" },
    }), { headers: { "content-type": "application/json" } });
  };
  let bootstrapText = await readFile(resolve(distDirectory, bootstrap.path.slice(1)), "utf8");
  bootstrapText = bootstrapText.replace(app.path, pathToFileURL(resolve(distDirectory, app.path.slice(1))).href);
  await import(`data:text/javascript;base64,${Buffer.from(bootstrapText).toString("base64")}`);
  await waitFor(() => window.document.body.textContent.includes("Local API available"));
  assert.equal(window.location.hash, "");
  assert.equal(window.document.documentElement.outerHTML.includes(token), false);
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

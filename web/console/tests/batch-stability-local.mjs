// Local acceptance only: one sequential control and twenty six-file batches.
// Run with INTO_MD_CLI pointing to the candidate binary; uses installed Edge.
import { createRequire } from "node:module";
import { spawn } from "node:child_process";
import { mkdir, mkdtemp, realpath, readFile, writeFile } from "node:fs/promises";
import { resolve, join } from "node:path";
const root = resolve(import.meta.dirname, "../../.."), require2 = createRequire(join(root, "web/console/package.json")), { chromium } = require2("@playwright/test");
await mkdir(join(root, "target/batch-investigation"), { recursive: true });
const output = await mkdtemp(join(await realpath(join(root, "target/batch-investigation")), "run-"));
const data = join(output, "data");
let logs = "";
const server = spawn(process.env.INTO_MD_CLI ?? join(root, "target/debug/into-md"), ["ui", "--no-open", "--no-config", "--data-dir", data], { stdio: ["ignore", "pipe", "pipe"], env: { ...process.env, INTO_MD_WEB_TIMINGS: "1" } });
server.stderr.on("data", (c) => {
  logs += c;
  void writeFile(join(output, "server.log"), logs);
});
let browser;
const results = [], requests = [], pageErrors = [], receiptPaths = /* @__PURE__ */ new Set();
try {
  const url = await new Promise((resolve2, reject) => {
    let out = "";
    const t = setTimeout(() => reject(Error("startup timeout")), 3e4);
    server.stdout.on("data", (c) => {
      out += c;
      const l = out.split(/\r?\n/).find((x) => x.startsWith("open this private session URL: "));
      if (l) {
        clearTimeout(t);
        resolve2(l.slice("open this private session URL: ".length));
      }
    });
    server.once("exit", () => {
      clearTimeout(t);
      reject(Error("server exited"));
    });
  });
  browser = await chromium.launch({ channel: "msedge", headless: true, args: ["--js-flags=--max-old-space-size=512"] });
  const page = await browser.newPage();
  page.on("pageerror", (e) => pageErrors.push(e.message));
  page.on("request", (r) => {
    if (r.method() === "POST" && new URL(r.url()).pathname.startsWith("/api/uploads/")) receiptPaths.add(new URL(r.url()).pathname);
  });
  page.on("response", async (r) => {
    if (r.status() >= 400 && r.url().includes("/api/")) requests.push({ path: new URL(r.url()).pathname, status: r.status(), body: await r.text().catch(() => null) });
  });
  await page.goto(url);
  const small = await readFile(join(root, "fixtures/small/xlsx/normal.xlsx"));
  const big = process.env.REPRO_INPUT === "pptx" ? await readFile(join(root, "target/web-large.pptx")) : Buffer.from("ID,Description,Value\n" + Array.from({ length: Number(process.env.REPRO_ROWS ?? 4e3) }, (_, i) => `${i},${"ordinary table content ".repeat(4)},${i + 1}`).join("\n"));
  const ext = process.env.REPRO_INPUT === "pptx" ? "pptx" : "csv";
  const state = () => page.evaluate(async () => {
    const headers = { "X-Into-Md-Session": sessionStorage.getItem("into-md.session") };
    const r = await fetch("/api/tasks?limit=100", { headers });
    return { status: r.status, body: await r.json() };
  });
  for (let round = 0; round < 21; round++) {
    const mode = round === 0 ? "sequential" : "batch", prefix = `round-${round}-`, files = Array.from({ length: 6 }, (_, i) => ({ name: `${prefix}${i}.${i % 2 ? "xlsx" : ext}`, mimeType: "application/octet-stream", buffer: i % 2 ? small : big }));
    const settle = async (names) => {
      const deadline = Date.now() + 12e4;
      while (Date.now() < deadline) {
        const snapshot2 = await state();
        if (names.every((name) => snapshot2.body.tasks?.some((task) => task.displayName === name && ["succeeded", "failed", "interrupted", "cancelled"].includes(task.status)))) return;
        if (requests.some((r) => r.body?.includes("queueUnavailable"))) return;
        await new Promise((resolve2) => setTimeout(resolve2, 1e3));
      }
      throw Error("task completion timeout");
    };
    let timedOut = false;
    const start = Date.now();
    if (mode === "batch") {
      await page.locator('input[type="file"]:not([webkitdirectory])').setInputFiles(files);
      await page.locator(".convert-button").click();
      await settle(files.map((f) => f.name)).catch(() => timedOut = true);
    } else for (const file of files) {
      await page.locator('input[type="file"]:not([webkitdirectory])').setInputFiles([file]);
      await page.locator(".convert-button").click();
      await settle([file.name]).catch(() => timedOut = true);
      if (timedOut) break;
    }
    const snapshot = await state(), tasks = (snapshot.body.tasks ?? []).filter((t) => t.displayName?.startsWith(prefix));
    const rows = await page.locator(".current-batch-scroll li").evaluateAll((ns) => ns.map((n) => ({ class: n.className, text: n.textContent })));
    const failed = timedOut || tasks.length !== 6 || tasks.some((t) => t.status !== "succeeded") || rows.filter((r) => r.text.startsWith(prefix)).some((r) => r.class === "failed");
    const result = { round, mode, ms: Date.now() - start, timedOut, failed, tasks, rows, requests: [...requests], pageErrors: [...pageErrors] };
    results.push(result);
    console.log(JSON.stringify({ output, round, mode, ms: result.ms, failed, statuses: tasks.map((t) => ({ name: t.displayName, status: t.status, failure: t.failure, diagnostics: t.diagnostics })), requests }));
    await writeFile(join(output, "results.json"), JSON.stringify(results, null, 2));
    if (failed) {
      await page.locator(".current-batch-scroll li.failed").first().scrollIntoViewIfNeeded().catch(() => {
      });
      await page.screenshot({ path: join(output, "failure.png") });
      const receipts = await page.evaluate(async (paths) => {
        const headers = { "X-Into-Md-Session": sessionStorage.getItem("into-md.session") };
        return Promise.all(paths.map(async (path) => {
          const r = await fetch(path, { headers });
          return { path, status: r.status, body: await r.json() };
        }));
      }, [...receiptPaths]);
      await writeFile(join(output, "receipts.json"), JSON.stringify(receipts, null, 2));
      break;
    }
  }
} finally {
  await browser?.close();
  server.kill("SIGTERM");
  await new Promise((r) => {
    if (server.exitCode !== null) r();
    else {
      server.once("exit", r);
      setTimeout(() => {
        server.kill("SIGKILL");
        r();
      }, 5e3).unref();
    }
  });
  await writeFile(join(output, "server.log"), logs);
}
if (results.length !== 21 || results.some((r) => r.failed || r.pageErrors.length)) process.exitCode = 1;

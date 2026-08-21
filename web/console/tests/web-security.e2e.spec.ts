import { spawn, type ChildProcessByStdio } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Readable } from "node:stream";
import { test, expect, type Page } from "@playwright/test";

const CLI = process.env.INTO_MD_CLI;
const PRIVATE_URL_PREFIX = "open this private session URL: ";
type LocalServer = ChildProcessByStdio<null, Readable, Readable>;

test.describe("local Web security boundary", () => {
  test.skip(!CLI, "INTO_MD_CLI must point to the production into-md binary");
  test.setTimeout(90_000);

  test("clears the launch secret and renders hostile Markdown as inert text", async ({ page }) => {
    const dataDirectory = await mkdtemp(join(tmpdir(), "into-md-browser-security-"));
    const server = spawn(CLI!, ["ui", "--no-open", "--no-config", "--data-dir", dataDirectory], {
      stdio: ["ignore", "pipe", "pipe"],
    });
    server.stderr.resume();

    try {
      const launchUrl = await privateLaunchUrl(server);
      const expectedOrigin = new URL(launchUrl).origin;
      const externalRequests = new Set<string>();
      page.on("request", (request) => recordExternalOrigin(request.url(), expectedOrigin, externalRequests));

      const response = await openPrivateConsole(page, launchUrl);
      expect(response.headers()["cache-control"]).toContain("no-store");
      expect(response.headers()["content-security-policy"]).toContain("default-src 'none'");
      expect(response.headers()["cross-origin-opener-policy"]).toBe("same-origin");
      expect(response.headers()["cross-origin-resource-policy"]).toBe("same-origin");
      expect(response.headers()["permissions-policy"]).toContain("camera=()");
      assertLaunchSecretCleared(page);

      await page.locator('input[type="file"]:not([webkitdirectory])').setInputFiles({
        name: "hostile.txt",
        mimeType: "text/plain",
        buffer: Buffer.from(hostileMarkdown()),
      });
      await page.locator(".convert-button").click();
      await page.locator(".markdown-preview").waitFor({ state: "visible" });

      const preview = page.locator(".markdown-preview");
      expect(await preview.textContent()).toContain("__intoMdBrowserSecurityProbe");
      expect(await preview.locator("a, img, script, iframe, object, embed, link, style, video, audio, source").count()).toBe(0);
      expect(await page.evaluate(() => "__intoMdBrowserSecurityProbe" in globalThis)).toBe(false);
      if (externalRequests.size !== 0) throw new Error("hostile preview initiated an external request");

      await page.reload({ waitUntil: "domcontentloaded" });
      await page.locator(".markdown-preview").waitFor({ state: "visible" });
      assertLaunchSecretCleared(page);
    } finally {
      await stopServer(server);
      await rm(dataDirectory, { recursive: true, force: true });
    }
  });
});

async function privateLaunchUrl(server: LocalServer): Promise<string> {
  return await new Promise((resolve, reject) => {
    let stdout = "";
    const timeout = setTimeout(() => reject(new Error("local Web server did not become ready")), 30_000);
    const finish = (callback: () => void) => { clearTimeout(timeout); callback(); };
    server.stdout.setEncoding("utf8");
    server.stdout.on("data", (chunk: string) => {
      stdout += chunk;
      const line = stdout.split(/\r?\n/).find((entry) => entry.startsWith(PRIVATE_URL_PREFIX));
      if (!line) return;
      const value = line.slice(PRIVATE_URL_PREFIX.length);
      if (!/^http:\/\/127\.0\.0\.1:\d+\/#into-md-session=[A-Za-z0-9_-]{43}$/.test(value)) {
        finish(() => reject(new Error("local Web server returned an invalid private URL")));
        return;
      }
      finish(() => resolve(value));
    });
    server.once("exit", () => finish(() => reject(new Error("local Web server stopped before it became ready"))));
    server.once("error", () => finish(() => reject(new Error("local Web server could not start"))));
  });
}

async function openPrivateConsole(page: Page, launchUrl: string) {
  try {
    const response = await page.goto(launchUrl, { waitUntil: "domcontentloaded" });
    if (!response) throw new Error("missing navigation response");
    return response;
  } catch {
    // Playwright navigation errors include the requested URL. Replace them so
    // the fragment-delivered session value cannot enter CI output.
    throw new Error("failed to open the private local console");
  }
}

function assertLaunchSecretCleared(page: Page): void {
  if (new URL(page.url()).hash !== "") throw new Error("private launch fragment was not cleared");
}

function recordExternalOrigin(url: string, expectedOrigin: string, external: Set<string>): void {
  try {
    const parsed = new URL(url);
    if ((parsed.protocol === "http:" || parsed.protocol === "https:") && parsed.origin !== expectedOrigin) {
      external.add(parsed.origin);
    }
  } catch {
    throw new Error("browser emitted an invalid request URL");
  }
}

function hostileMarkdown(): string {
  return [
    "# Safe heading",
    '<script>globalThis.__intoMdBrowserSecurityProbe = true</script>',
    "![remote](https://browser-security.invalid/tracker.png)",
    "[javascript](javascript:alert(document.domain))",
    "[local](file:///etc/passwd)",
    '<img src="data:text/html,<script>globalThis.__intoMdBrowserSecurityProbe=true</script>" onerror="globalThis.__intoMdBrowserSecurityProbe=true">',
    '<iframe src="https://browser-security.invalid/frame"></iframe>',
  ].join("\n");
}

async function stopServer(server: LocalServer): Promise<void> {
  if (server.exitCode !== null || server.signalCode !== null) return;
  server.kill("SIGINT");
  await Promise.race([
    new Promise<void>((resolve) => server.once("exit", () => resolve())),
    new Promise<void>((resolve) => setTimeout(resolve, 5_000)),
  ]);
  if (server.exitCode === null && server.signalCode === null) {
    const forcedExit = new Promise<void>((resolve) => {
      if (server.exitCode !== null || server.signalCode !== null) resolve();
      else server.once("exit", () => resolve());
    });
    server.kill("SIGKILL");
    await forcedExit;
  }
}

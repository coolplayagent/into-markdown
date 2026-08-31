import { build } from "esbuild-wasm";
import { createHash } from "node:crypto";
import { mkdir, readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { basename, delimiter, dirname, relative, resolve } from "node:path";

// esbuild-wasm launches its service through the literal `node` command. Make
// that child resolve to the same hermetic runtime that launched this builder.
process.env.PATH = `${dirname(process.execPath)}${delimiter}${process.env.PATH ?? ""}`;

const workspace = resolve(import.meta.dirname, "../..");
const outputDirectory = resolve(process.argv[2] ?? resolve(import.meta.dirname, "dist"));
const sourceDirectory = resolve(import.meta.dirname, "src");
// Inputs and outputs are normalized so the same asset bytes are produced on every host.
const verifyIndex = process.argv.indexOf("--verify");
const goldenDirectory = verifyIndex === -1 ? null : resolve(process.argv[verifyIndex + 1]);

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

async function bundle(entryPoint, options = {}) {
  const result = await build({
    absWorkingDir: workspace,
    bundle: true,
    charset: "utf8",
    entryPoints: [entryPoint],
    format: "esm",
    jsx: "automatic",
    jsxImportSource: "react",
    legalComments: "eof",
    logLevel: "silent",
    // Remove CommonJS debug labels containing dependency-store paths. The Bazel
    // inputs include package.json so ESM interop matches the pnpm workspace.
    minifyIdentifiers: true,
    minifySyntax: true,
    minifyWhitespace: true,
    platform: "browser",
    resolveExtensions: [".tsx", ".ts", ".jsx", ".js", ".css", ".json"],
    sourcemap: false,
    target: ["es2022"],
    treeShaking: true,
    write: false,
    define: { "process.env.NODE_ENV": '"production"' },
    ...options,
  });
  return result.outputFiles;
}

function select(files, suffix) {
  const file = files.find((candidate) => candidate.path.endsWith(suffix));
  if (!file) throw new Error(`expected esbuild output ${suffix}`);
  return file.contents;
}

async function emitAsset(logicalName, extension, bytes, mime, cache, assets) {
  const digest = sha256(bytes);
  const path = `assets/${logicalName}.${digest.slice(0, 16)}.${extension}`;
  await writeFile(resolve(outputDirectory, path), bytes);
  assets.push({ path: `/${path}`, sha256: digest, bytes: bytes.byteLength, mime, cache });
  return `/${path}`;
}

await rm(outputDirectory, { recursive: true, force: true });
await mkdir(resolve(outputDirectory, "assets"), { recursive: true });

const assets = [];
const appFiles = await bundle(resolve(sourceDirectory, "main.tsx"), { outfile: "app.js" });
const appPath = await emitAsset("app", "js", select(appFiles, "app.js"), "text/javascript; charset=utf-8", "immutable", assets);
const stylePath = await emitAsset("app", "css", select(appFiles, "app.css"), "text/css; charset=utf-8", "immutable", assets);

const bootstrapFiles = await bundle(resolve(sourceDirectory, "bootstrap.ts"), {
  define: {
    INTO_MD_APP_MODULE: JSON.stringify(appPath),
    "process.env.NODE_ENV": '"production"',
  },
  external: ["/assets/*"],
  outfile: "bootstrap.js",
});
const bootstrapPath = await emitAsset("bootstrap", "js", select(bootstrapFiles, "bootstrap.js"), "text/javascript; charset=utf-8", "immutable", assets);

const html = `<!doctype html>\n<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="light dark"><title>into-markdown</title><link rel="stylesheet" href="${stylePath}"><script type="module" src="${bootstrapPath}"></script></head><body><div id="app"><noscript><main><h1>into-markdown</h1><p>JavaScript is required for the local console.</p></main></noscript></div></body></html>\n`;
const htmlBytes = new TextEncoder().encode(html);
await writeFile(resolve(outputDirectory, "index.html"), htmlBytes);
assets.unshift({ path: "/index.html", sha256: sha256(htmlBytes), bytes: htmlBytes.byteLength, mime: "text/html; charset=utf-8", cache: "no-store" });

assets.sort((left, right) => left.path.localeCompare(right.path, "en"));
const manifest = `${JSON.stringify({ schemaVersion: 1, assets }, null, 2)}\n`;
await writeFile(resolve(outputDirectory, "asset-manifest.json"), manifest, "utf8");

async function filesUnder(directory) {
  const result = [];
  async function visit(current) {
    for (const entry of await readdir(current, { withFileTypes: true })) {
      const path = resolve(current, entry.name);
      if (entry.isDirectory()) await visit(path);
      else if (entry.isFile() || (entry.isSymbolicLink() && (await stat(path)).isFile())) result.push(relative(directory, path));
      else throw new Error(`asset tree contains a non-file entry: ${entry.name}`);
    }
  }
  await visit(directory);
  return result.sort();
}

if (goldenDirectory !== null) {
  const actualFiles = await filesUnder(outputDirectory);
  const goldenFiles = await filesUnder(goldenDirectory);
  if (JSON.stringify(actualFiles) !== JSON.stringify(goldenFiles)) {
    throw new Error(`checked-in console asset file list is stale; generated=${actualFiles.join(",")} checked=${goldenFiles.join(",")}`);
  }
  for (const path of actualFiles) {
    const [actual, golden] = await Promise.all([
      readFile(resolve(outputDirectory, path)),
      readFile(resolve(goldenDirectory, path)),
    ]);
    if (!actual.equals(golden)) throw new Error(`checked-in console asset is stale: ${path}`);
  }
}

process.stdout.write(`generated ${assets.length} deterministic assets in ${basename(outputDirectory)}\n`);

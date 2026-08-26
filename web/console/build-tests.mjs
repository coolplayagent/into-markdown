import { build } from "esbuild-wasm";
import { delimiter, dirname } from "node:path";

// Keep esbuild-wasm's child on the Bazel-provided Node runtime even when the
// action environment intentionally has no developer PATH entries.
process.env.PATH = `${dirname(process.execPath)}${delimiter}${process.env.PATH ?? ""}`;

await build({
  bundle: true,
  define: { "process.env.NODE_ENV": '"test"' },
  entryPoints: [process.argv[3] ?? "web/console/tests/unit.ts"],
  format: "esm",
  jsx: "automatic",
  jsxImportSource: "react",
  legalComments: "eof",
  loader: { ".css": "text" },
  minify: false,
  outfile: process.argv[2],
  packages: "external",
  platform: "node",
  sourcemap: false,
  target: ["node24"],
});

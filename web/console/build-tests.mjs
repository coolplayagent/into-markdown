import { build } from "esbuild-wasm";

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

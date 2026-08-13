import { build } from "esbuild-wasm";

await build({
  bundle: true,
  define: { "process.env.NODE_ENV": '"test"' },
  entryPoints: ["web/console/tests/unit.ts"],
  format: "esm",
  legalComments: "eof",
  loader: { ".css": "text" },
  minify: false,
  outfile: process.argv[2],
  packages: "external",
  platform: "node",
  sourcemap: false,
  target: ["node24"],
});

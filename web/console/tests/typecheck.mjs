import ts from "typescript";
import { dirname, resolve } from "node:path";

const configPath = resolve(import.meta.dirname, "../tsconfig.json");
const loaded = ts.readConfigFile(configPath, ts.sys.readFile);
if (loaded.error) {
  process.stderr.write(ts.formatDiagnostics([loaded.error], formatHost()));
  process.exit(1);
}

const parsed = ts.parseJsonConfigFileContent(
  loaded.config,
  ts.sys,
  dirname(configPath),
  undefined,
  configPath,
);
if (parsed.errors.length > 0) {
  process.stderr.write(ts.formatDiagnostics(parsed.errors, formatHost()));
  process.exit(1);
}

const program = ts.createProgram({
  rootNames: parsed.fileNames,
  options: parsed.options,
  projectReferences: parsed.projectReferences,
});
const diagnostics = ts.getPreEmitDiagnostics(program);
if (diagnostics.length > 0) {
  process.stderr.write(ts.formatDiagnostics(diagnostics, formatHost()));
  process.exit(1);
}

function formatHost() {
  return {
    getCanonicalFileName: (path) => path,
    getCurrentDirectory: () => process.cwd(),
    getNewLine: () => "\n",
  };
}

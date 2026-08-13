import { spawnSync } from "node:child_process";
import { join } from "node:path";

const workspace = process.env.BUILD_WORKSPACE_DIRECTORY;
const runfiles = process.env.JS_BINARY__RUNFILES;
const repository = process.env.JS_BINARY__WORKSPACE;
if (!workspace || !runfiles || !repository) {
  throw new Error("asset update must run through bazel run");
}

const suffix = process.platform === "win32" ? ".exe" : "";
const updater = join(runfiles, repository, "tools/asset-update", `asset_updater${suffix}`);
const generated = join(runfiles, repository, "web/console", "generated_assets");
const result = spawnSync(updater, [workspace, generated], { encoding: "utf8", stdio: "inherit" });
if (result.error) throw result.error;
if (result.status !== 0) process.exitCode = result.status ?? 1;

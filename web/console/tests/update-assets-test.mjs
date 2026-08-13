import assert from "node:assert/strict";
import { chmod, mkdir, mkdtemp, readFile, rm, stat, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { updateAssets } from "../update-assets.mjs";

async function fixture() {
  const root = await mkdtemp(join(process.env.TEST_TMPDIR, "into-md-asset-update-"));
  const workspace = join(root, "workspace");
  const consoleDirectory = join(workspace, "web", "console");
  const generated = join(root, "generated");
  await mkdir(consoleDirectory, { recursive: true });
  await mkdir(join(generated, "assets"), { recursive: true });
  await writeFile(join(generated, "asset-manifest.json"), "generated manifest\n");
  await writeFile(join(generated, "assets", "app.js"), "generated app\n");
  return { root, workspace, consoleDirectory, generated };
}

async function externalSnapshot(path) {
  const info = await stat(path);
  return { contents: await readFile(path, "utf8"), inode: info.ino, mode: info.mode & 0o777 };
}

async function assertExternalUnchanged(path, expected) {
  assert.deepEqual(await externalSnapshot(path), expected);
}

test("asset updater rejects destination, backup, and path-component symlinks without touching external files", async () => {
  for (const target of ["destination", "backup", "component"]) {
    const { root, workspace, consoleDirectory, generated } = await fixture();
    const external = join(root, "external");
    const externalFile = join(external, "protected.txt");
    await mkdir(external);
    await writeFile(externalFile, "do not modify\n");
    await chmod(externalFile, 0o444);
    const before = await externalSnapshot(externalFile);
    if (target === "destination") {
      await symlink(external, join(consoleDirectory, "dist"));
    } else if (target === "backup") {
      await mkdir(join(consoleDirectory, "dist"));
      await symlink(external, join(consoleDirectory, ".dist-backup"));
    } else {
      await rm(join(workspace, "web"), { recursive: true });
      await symlink(external, join(workspace, "web"));
    }
    await assert.rejects(updateAssets(workspace, generated), /symbolic link|non-symlink directory/);
    await assertExternalUnchanged(externalFile, before);
    await rm(root, { recursive: true, force: true });
  }
});

test("asset updater performs a normal same-parent atomic replacement", async () => {
  const { root, workspace, consoleDirectory, generated } = await fixture();
  const destination = join(consoleDirectory, "dist");
  await mkdir(destination);
  await writeFile(join(destination, "old.txt"), "old\n");
  await updateAssets(workspace, generated);
  assert.equal(await readFile(join(destination, "asset-manifest.json"), "utf8"), "generated manifest\n");
  assert.equal(await readFile(join(destination, "assets", "app.js"), "utf8"), "generated app\n");
  await assert.rejects(stat(join(destination, "old.txt")), { code: "ENOENT" });
  await assert.rejects(stat(join(consoleDirectory, ".dist-backup")), { code: "ENOENT" });
  assert.equal((await stat(destination)).mode & 0o777, 0o755);
  assert.equal((await stat(join(destination, "assets", "app.js"))).mode & 0o777, 0o644);
  await rm(root, { recursive: true, force: true });
});

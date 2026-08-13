import { constants } from "node:fs";
import { cp, lstat, mkdtemp, open, readdir, rename, rm } from "node:fs/promises";
import { dirname, join, parse, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const DIRECTORY_FLAGS = constants.O_RDONLY
  | (constants.O_DIRECTORY ?? 0)
  | (constants.O_NOFOLLOW ?? 0);
const FILE_FLAGS = constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0);

async function metadata(path) {
  try {
    return await lstat(path);
  } catch (error) {
    if (error?.code === "ENOENT") return null;
    throw error;
  }
}

async function openVerified(path, expected, label) {
  if (expected.isSymbolicLink()) throw new Error(`${label} must not be a symbolic link`);
  const isDirectory = expected.isDirectory();
  if (!isDirectory && !expected.isFile()) throw new Error(`${label} must be a directory or regular file`);
  let handle;
  try {
    handle = await open(path, isDirectory ? DIRECTORY_FLAGS : FILE_FLAGS);
    const actual = await handle.stat();
    if (actual.dev !== expected.dev || actual.ino !== expected.ino || actual.isDirectory() !== isDirectory) {
      throw new Error(`${label} changed during no-follow validation`);
    }
    return handle;
  } catch (error) {
    await handle?.close();
    throw error;
  }
}

async function verifyDirectory(path, label) {
  const info = await metadata(path);
  if (!info || !info.isDirectory() || info.isSymbolicLink()) {
    throw new Error(`${label} must be an existing non-symlink directory`);
  }
  const handle = await openVerified(path, info, label);
  await handle.close();
}

async function verifyDirectoryChain(path, label) {
  const absolute = resolve(path);
  const root = parse(absolute).root;
  await verifyDirectory(root, `${label} root`);
  let current = root;
  const suffix = relative(root, absolute);
  for (const component of suffix.split(sep).filter(Boolean)) {
    current = join(current, component);
    await verifyDirectory(current, `${label} component`);
  }
}

async function verifySafeTree(path, label) {
  const info = await metadata(path);
  if (!info) return false;
  const handle = await openVerified(path, info, label);
  if (info.isDirectory()) {
    if ((info.mode & 0o200) === 0) {
      await handle.close();
      throw new Error(`${label} directory must be owner-writable for atomic replacement`);
    }
    const entries = await readdir(path);
    for (const entry of entries) await verifySafeTree(join(path, entry), `${label} entry`);
  }
  await handle.close();
  return true;
}

async function normalizeOwnedTree(path) {
  const info = await metadata(path);
  if (!info) throw new Error("owned update tree disappeared");
  const handle = await openVerified(path, info, "owned update tree");
  await handle.chmod(info.isDirectory() ? 0o755 : 0o644);
  if (info.isDirectory()) {
    const entries = await readdir(path);
    for (const entry of entries) await normalizeOwnedTree(join(path, entry));
  }
  await handle.close();
}

export async function updateAssets(workspace, generatedDirectory) {
  const workspaceRoot = resolve(workspace);
  const parent = resolve(workspaceRoot, "web/console");
  const destination = resolve(parent, "dist");
  const generated = resolve(generatedDirectory);
  const backup = resolve(parent, ".dist-backup");
  if (destination !== resolve(workspaceRoot, "web", "console", "dist")) {
    throw new Error("refusing to update an unexpected asset directory");
  }

  await verifyDirectoryChain(parent, "workspace");
  await verifySafeTree(destination, "destination");
  if (await metadata(backup)) {
    await verifySafeTree(backup, "backup");
    throw new Error("refusing to replace assets while a backup already exists");
  }

  const temporary = await mkdtemp(resolve(parent, ".dist-update-"));
  let previousMoved = false;
  try {
    await cp(generated, temporary, { recursive: true, force: true, dereference: true });
    await normalizeOwnedTree(temporary);
    await verifyDirectoryChain(parent, "workspace");
    const destinationExists = await verifySafeTree(destination, "destination");
    if (await metadata(backup)) throw new Error("asset backup appeared during update");
    if (destinationExists) {
      await rename(destination, backup);
      previousMoved = true;
    }
    await rename(temporary, destination);
    if (previousMoved) {
      await verifySafeTree(backup, "backup");
      await rm(backup, { recursive: true });
    }
  } catch (error) {
    await rm(temporary, { recursive: true, force: true });
    if (previousMoved && !(await metadata(destination))) {
      await verifySafeTree(backup, "backup");
      await rename(backup, destination);
    }
    throw error;
  }
}

const invokedPath = process.argv[1] ? resolve(process.argv[1]) : "";
if (invokedPath === resolve(fileURLToPath(import.meta.url))) {
  const workspace = process.env.BUILD_WORKSPACE_DIRECTORY;
  if (!workspace) throw new Error("asset update must run through bazel run");
  await updateAssets(workspace, resolve(dirname(fileURLToPath(import.meta.url)), "generated_assets"));
  process.stdout.write("updated checked-in console assets from Bazel-generated bytes\n");
}

"""Git-tracked input discovery and bounded, sequential blob reads."""

import pathlib
import subprocess
from contextlib import contextmanager

from .model import GateError

EXTENSIONS = {".rs", ".py", ".ts", ".tsx"}


def canonical_path(value: str) -> str:
    value = value.replace("\\", "/")
    parts = value.split("/")
    if not value or any(part in {"", ".", ".."} for part in parts) or ":" in value:
        raise GateError(f"invalid repository-relative path: {value!r}")
    return value


def exclusion(path: str) -> str | None:
    parts = pathlib.PurePosixPath(path).parts
    name = parts[-1]
    if parts[0] == "third_party":
        return "fixed third-party source"
    if "dist" in parts or "generated_assets" in parts or "generated_assets_repeat" in parts:
        return "generated assets"
    if any(part in {"tests", "fixtures", "benches", "testdata", "real-world-test-data"} for part in parts):
        return "tests / fixtures / benchmarks"
    if (name in {"tests.rs", "conftest.py"} or name.startswith("test_")
            or name.endswith(("_test.py", "_tests.rs", "_test_support.rs", "_fixture.rs", ".test.ts", ".spec.ts",
                              ".test.tsx", ".spec.tsx"))):
        return "test or fixture module"
    return None


def git(root: pathlib.Path, *args: str) -> bytes:
    result = subprocess.run(["git", "-C", str(root), *args], capture_output=True, check=False)
    if result.returncode:
        raise GateError(result.stderr.decode("utf-8", errors="replace").strip())
    return result.stdout


class Source:
    def __init__(self, root: pathlib.Path, ref: str | None = None):
        self.root = root.resolve()
        self.ref = git(root, "rev-parse", "--verify", f"{ref}^{{commit}}").decode().strip() if ref else None
        self.entries: dict[str, str] = {}
        if self.ref:
            for record in git(root, "ls-tree", "-rz", self.ref).split(b"\0"):
                if record:
                    metadata, raw_path = record.split(b"\t", 1)
                    mode, _, oid = metadata.decode().split()
                    self._add(raw_path.decode("utf-8"), mode, oid)
        else:
            for record in git(root, "ls-files", "--stage", "-z").split(b"\0"):
                if record:
                    metadata, raw_path = record.split(b"\t", 1)
                    mode, oid, stage = metadata.decode().split()
                    if stage != "0":
                        raise GateError("resolve index conflicts before checking structure")
                    self._add(raw_path.decode("utf-8"), mode, oid)

    def _add(self, path: str, mode: str, oid: str) -> None:
        path = canonical_path(path)
        if path in self.entries:
            raise GateError(f"duplicate tracked path: {path}")
        if pathlib.PurePosixPath(path).suffix in EXTENSIONS and mode not in {"100644", "100755"}:
            raise GateError(f"source is not a regular file: {path}")
        self.entries[path] = oid

    def read_local(self, path: str) -> bytes:
        target = self.root / canonical_path(path)
        if not target.resolve().is_relative_to(self.root) or target.is_symlink():
            raise GateError(f"source escapes repository or is a symlink: {path}")
        return target.read_bytes()

    def optional(self, path: str) -> bytes | None:
        if self.ref:
            return git(self.root, "show", f"{self.ref}:{path}") if path in self.entries else None
        return self.read_local(path) if (self.root / path).exists() else None

    @contextmanager
    def blobs(self):
        """Never materialize the complete base tree or all source bytes together."""
        if not self.ref:
            yield self.read_local
            return
        process = subprocess.Popen(["git", "-C", str(self.root), "cat-file", "--batch"],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        try:
            def read(path: str) -> bytes:
                process.stdin.write((self.entries[path] + "\n").encode("ascii"))
                process.stdin.flush()
                header = process.stdout.readline().split()
                if len(header) != 3 or header[1] != b"blob":
                    raise GateError(f"cannot read source blob: {path}")
                size = int(header[2])
                content = process.stdout.read(size)
                if len(content) != size or process.stdout.read(1) != b"\n":
                    raise GateError(f"truncated source blob: {path}")
                return content
            yield read
        finally:
            process.stdin.close()
            process.stdout.close()
            if process.wait() != 0:
                raise GateError("git cat-file failed")

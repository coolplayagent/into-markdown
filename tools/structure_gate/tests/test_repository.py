import concurrent.futures
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from tools.structure_gate.baseline import encode, freeze, load_baseline
from tools.structure_gate.cli import evaluate
from tools.structure_gate.model import BASELINE_PATH, GateError
from tools.structure_gate.scan import analyze, scan
from tools.structure_gate.source import Source, exclusion, git
from tools.structure_gate.storage import replace_baseline

ROOT = pathlib.Path(__file__).resolve().parents[3]


class RepositoryTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="structure-gate-test-")
        self.addCleanup(self.directory.cleanup)
        self.root = pathlib.Path(self.directory.name)
        git(self.root, "init", "-b", "main")
        git(self.root, "config", "user.name", "Structure gate test")
        git(self.root, "config", "user.email", "structure-test@example.invalid")
        git(self.root, "config", "core.autocrlf", "false")
        self.write("src/code.rs", b"fn existing() {}\n")
        self.write(BASELINE_PATH, encode(freeze({})))
        git(self.root, "add", ".")
        git(self.root, "commit", "-m", "fixture base")
        self.base = git(self.root, "rev-parse", "HEAD").decode().strip()

    def write(self, path, content):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(content)

    def check(self):
        return evaluate(self.root, self.base, "check")

    def test_base_passes_and_normal_tracked_addition_passes(self):
        self.assertEqual(self.check()["violations"], [])
        self.write("src/new.ts", b"export const answer = () => 42;\n")
        git(self.root, "add", "src/new.ts")
        self.assertEqual(self.check()["violations"], [])

    def test_large_inline_test_does_not_consume_production_budget(self):
        text = b"fn existing() {}\n#[cfg(test)]\nmod tests {\nfn fixture() {\n" + b"work();\n" * 2000 + b"}\n}\n"
        self.write("src/code.rs", text)
        self.assertEqual(self.check()["violations"], [])

    def test_manual_baseline_increase_does_not_grant_new_budget(self):
        source = "\n".join(f"const C{i}: i32 = {i};" for i in range(2000)).encode()
        self.write("src/code.rs", source)
        self.write(BASELINE_PATH, encode(freeze({"src/code.rs": analyze("src/code.rs", source)})))
        self.assertTrue(self.check()["violations"])
        before = (self.root / BASELINE_PATH).read_bytes()
        self.assertTrue(evaluate(self.root, self.base, "ratchet")["violations"])
        self.assertEqual((self.root / BASELINE_PATH).read_bytes(), before)

    def test_debt_reduction_requires_baseline_update(self):
        source = "\n".join(f"const C{i}: i32 = {i};" for i in range(2000)).encode()
        self.write("src/code.rs", source)
        self.write(BASELINE_PATH, encode(freeze({"src/code.rs": analyze("src/code.rs", source)})))
        git(self.root, "add", ".")
        git(self.root, "commit", "-m", "historical debt fixture")
        self.base = git(self.root, "rev-parse", "HEAD").decode().strip()
        self.write("src/code.rs", b"fn smaller() {}\n")
        self.assertTrue(self.check()["violations"])
        self.assertEqual(evaluate(self.root, self.base, "ratchet")["violations"], [])
        self.assertEqual(load_baseline((self.root / BASELINE_PATH).read_bytes())["files"], [])
        self.assertEqual(self.check()["violations"], [])

    def test_missing_and_damaged_baseline_fail(self):
        (self.root / BASELINE_PATH).unlink()
        self.assertTrue(self.check()["violations"])
        self.write(BASELINE_PATH, b"not json")
        with self.assertRaises(GateError):
            self.check()

    def test_missing_base_only_bootstraps_exact_pinned_commit(self):
        git(self.root, "rm", BASELINE_PATH)
        git(self.root, "commit", "-m", "bootstrap fixture")
        self.base = git(self.root, "rev-parse", "HEAD").decode().strip()
        with self.assertRaisesRegex(GateError, "pinned bootstrap"):
            evaluate(self.root, self.base, "ratchet")
        (self.root / BASELINE_PATH).parent.mkdir(parents=True, exist_ok=True)
        with patch("tools.structure_gate.cli.BOOTSTRAP_COMMIT", self.base):
            self.assertEqual(evaluate(self.root, self.base, "ratchet")["violations"], [])
            self.assertEqual(self.check()["violations"], [])

    def test_base_snapshot_does_not_read_candidate_source(self):
        self.write("src/code.rs", b"fn invalid(")
        metrics, _ = scan(Source(self.root, self.base))
        self.assertEqual(metrics["src/code.rs"].code_lines, 1)
        with self.assertRaises(GateError):
            self.check()

    def test_exclusions_are_reported(self):
        for path in ("third_party/vendor/lib.rs", "src/tests.rs", "src/item_tests.rs",
                     "src/item_test_support.rs", "tools/test_thing.py", "web/dist/generated.ts"):
            self.assertIsNotNone(exclusion(path))
            self.write(path, b"not parseable code")
        git(self.root, "add", ".")
        result = self.check()
        self.assertEqual(result["violations"], [])
        self.assertEqual(len(result["excluded"]), 6)

    def test_parallel_checks_are_read_only(self):
        before = git(self.root, "status", "--porcelain")
        command = [sys.executable, "-m", "tools.structure_gate", "check", "--root", str(self.root),
                   "--base-ref", self.base, "--format", "json"]
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(lambda _: subprocess.run(command, cwd=ROOT, capture_output=True), range(2)))
        self.assertEqual([result.returncode for result in results], [0, 0], results[0].stderr)
        self.assertEqual(git(self.root, "status", "--porcelain"), before)

    def test_writer_rejects_stale_baseline_and_existing_writer(self):
        path = self.root / BASELINE_PATH
        original = path.read_bytes()
        with self.assertRaisesRegex(GateError, "changed during analysis"):
            replace_baseline(path, b"stale", b"replacement")
        self.assertEqual(path.read_bytes(), original)
        self.assertFalse(path.with_suffix(".lock").exists())
        path.with_suffix(".lock").touch()
        with self.assertRaisesRegex(GateError, "writer is active"):
            replace_baseline(path, original, b"replacement")
        self.assertEqual(path.read_bytes(), original)

    def test_atomic_writer_removes_its_temporary_files(self):
        path = self.root / BASELINE_PATH
        replace_baseline(path, path.read_bytes(), encode(freeze({})))
        self.assertEqual(list(path.parent.glob("*.tmp")), [])
        self.assertFalse(path.with_suffix(".lock").exists())


if __name__ == "__main__":
    unittest.main()

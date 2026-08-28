import json
import pathlib
import re
import sys
import tempfile
import unittest

from tools.ci.run_compiled_rust_tests import compiled_harnesses, package_name
from tools.ci.run_timed import run


ROOT = pathlib.Path(__file__).resolve().parents[2]


class CompiledRustTestRunnerTests(unittest.TestCase):
    def test_collects_only_unique_test_executables_by_package(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            manifest = pathlib.Path(directory) / "cargo.jsonl"
            artifact = {
                "reason": "compiler-artifact",
                "package_id": "path+file:///repo/crate#example-package@0.0.3",
                "profile": {"test": True},
                "target": {"name": "example_package"},
                "executable": "/tmp/example-test",
            }
            manifest.write_text(
                "\n".join(
                    [
                        json.dumps({"reason": "build-script-executed"}),
                        json.dumps(artifact),
                        json.dumps(artifact),
                        json.dumps({**artifact, "profile": {"test": False}}),
                    ]
                ),
                encoding="utf-8",
            )

            packages = compiled_harnesses([manifest])

        self.assertEqual(list(packages), [artifact["package_id"]])
        self.assertEqual(
            packages[artifact["package_id"]],
            [("example_package", pathlib.Path("/tmp/example-test"))],
        )

    def test_package_name_comes_from_cargo_package_identity(self) -> None:
        self.assertEqual(
            package_name("path+file:///repo/crate#into-markdown-process-plugin@0.0.3"),
            "into-markdown-process-plugin",
        )


class PullRequestGateContractTests(unittest.TestCase):
    def test_gate_keeps_four_checks_and_the_complete_test_authorities(self) -> None:
        workflow = (ROOT / ".github/workflows/pr-fast-gate.yml").read_text(encoding="utf-8")
        native_action = (ROOT / ".github/actions/native-pr-gate/action.yml").read_text(
            encoding="utf-8"
        )

        self.assertEqual(workflow.count("    runs-on:"), 4)
        self.assertEqual(
            [int(value) for value in re.findall(r"timeout-minutes: (\d+)", workflow)],
            [5, 5, 5, 5],
        )
        self.assertEqual(workflow.count("uses: ./.github/actions/native-pr-gate"), 3)
        self.assertEqual(workflow.count("uses: actions/cache/restore@v4"), 1)
        self.assertEqual(workflow.count("uses: actions/cache/save@v4"), 1)
        self.assertEqual(workflow.count("name: Publish gate timing summary"), 1)
        self.assertEqual(native_action.count("uses: actions/cache/restore@v4"), 1)
        self.assertEqual(native_action.count("uses: actions/cache/save@v4"), 1)
        self.assertEqual(native_action.count("name: Publish gate timing summary"), 1)
        self.assertNotIn("uses: actions/cache@v4", workflow)
        self.assertNotIn("uses: actions/cache@v4", native_action)
        self.assertNotIn("-j2", workflow)
        self.assertNotIn("-j2", native_action)
        self.assertIn("push:\n    branches: [main]", workflow)
        self.assertIn("-p into-markdown-core", workflow)
        self.assertIn("-p into-markdown-layout-quality", workflow)
        self.assertIn("-p into-markdown-ocr", workflow)
        self.assertIn("-p into-markdown-process-plugin", workflow)
        self.assertIn("--test runtime", workflow)
        self.assertIn("tools/ci/run_compiled_rust_tests.py", workflow)
        self.assertEqual(
            native_action.count("cargo test --locked -p into-markdown-cli --bin into-md"),
            2,
        )


class TimedCommandTests(unittest.TestCase):
    def test_records_success_and_propagates_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "timings.tsv"
            self.assertEqual(run("success", output, [sys.executable, "-c", "pass"]), 0)
            self.assertEqual(run("failure", output, [sys.executable, "-c", "raise SystemExit(7)"]), 7)
            rows = output.read_text(encoding="utf-8").splitlines()

        self.assertEqual([row.split("\t", 1)[0] for row in rows], ["success", "failure"])
        self.assertTrue(all(float(row.split("\t", 1)[1]) >= 0 for row in rows))


if __name__ == "__main__":
    unittest.main()

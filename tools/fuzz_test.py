import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location("fuzz_tool", Path(__file__).with_name("fuzz.py"))
FUZZ = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FUZZ)


class FuzzToolTests(unittest.TestCase):
    def fixture_root(self):
        temporary = tempfile.TemporaryDirectory()
        root = Path(temporary.name)
        (root / "fuzz" / "regressions").mkdir(parents=True)
        (root / "fuzz" / "seeds").mkdir()
        (root / "fixtures").mkdir()
        (root / "fixtures" / "seed.bin").write_bytes(b"seed")
        (root / "fixtures" / "manifest.json").write_text(json.dumps({
            "schema_version": 1,
            "fixtures": [{
                "path": "seed.bin",
                "bytes": 4,
                "sha256": hashlib.sha256(b"seed").hexdigest(),
                "license": {"spdx": "Apache-2.0"},
            }],
        }), encoding="utf-8")
        (root / "fuzz" / "seeds.json").write_text(json.dumps({
            "schema_version": 1,
            "license": "Apache-2.0",
            "targets": {"zip": ["fixtures/seed.bin"]},
        }), encoding="utf-8")
        (root / "fuzz" / "regressions" / "manifest.json").write_text(
            '{"schema_version":1,"fixtures":[]}\n', encoding="utf-8"
        )
        return temporary, root

    def test_prepare_uses_content_addressed_owned_seed(self):
        temporary, root = self.fixture_root()
        self.addCleanup(temporary.cleanup)
        output = FUZZ.prepare(root, "zip")
        files = list(output.iterdir())
        self.assertEqual(len(files), 1)
        self.assertEqual(files[0].read_bytes(), b"seed")
        self.assertTrue(files[0].name.startswith(hashlib.sha256(b"seed").hexdigest()[:16]))

    def test_prepare_rejects_escape(self):
        temporary, root = self.fixture_root()
        self.addCleanup(temporary.cleanup)
        authority = json.loads((root / "fuzz" / "seeds.json").read_text(encoding="utf-8"))
        authority["targets"]["zip"] = ["../outside"]
        (root / "fuzz" / "seeds.json").write_text(json.dumps(authority), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "escapes repository"):
            FUZZ.prepare(root, "zip")

    def test_prepare_removes_stale_corpus_files(self):
        temporary, root = self.fixture_root()
        self.addCleanup(temporary.cleanup)
        output = root / "fuzz" / "corpus" / "zip"
        output.mkdir(parents=True)
        (output / "unreviewed.bin").write_bytes(b"not authoritative")
        FUZZ.prepare(root, "zip")
        self.assertFalse((output / "unreviewed.bin").exists())

    def test_promote_is_deduplicated_hash_bound_and_licensed(self):
        temporary, root = self.fixture_root()
        self.addCleanup(temporary.cleanup)
        artifact = root / "crash"
        artifact.write_bytes(b"small crash")
        first = FUZZ.promote(root, "zip", artifact)
        second = FUZZ.promote(root, "zip", artifact)
        self.assertEqual(first, second)
        manifest = json.loads(
            (root / "fuzz" / "regressions" / "manifest.json").read_text(encoding="utf-8")
        )
        self.assertEqual(len(manifest["fixtures"]), 1)
        record = manifest["fixtures"][0]
        self.assertEqual(record["sha256"], hashlib.sha256(b"small crash").hexdigest())
        self.assertEqual(record["license"], "Apache-2.0")

    def test_report_tracks_platform_sanitizer_and_artifacts(self):
        temporary, root = self.fixture_root()
        self.addCleanup(temporary.cleanup)
        artifacts = root / "fuzz" / "artifacts" / "zip"
        artifacts.mkdir(parents=True)
        (artifacts / "crash-a").write_bytes(b"a")
        output = root / "report.json"
        FUZZ.report(root, "zip", "address", 1, output)
        report = json.loads(output.read_text(encoding="utf-8"))
        self.assertEqual(report["target"], "zip")
        self.assertEqual(report["sanitizer"], "address")
        self.assertEqual(report["exit_status"], 1)
        self.assertEqual(report["artifacts"][0]["sha256"], hashlib.sha256(b"a").hexdigest())


if __name__ == "__main__":
    unittest.main()

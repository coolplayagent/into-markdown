"""Verify the actual generated model and its unchanged release identities."""

import hashlib
import json
import os
import pathlib
import unittest


ROOT = pathlib.Path(os.environ["TEST_SRCDIR"]) / os.environ["TEST_WORKSPACE"]
MODEL_SHA256 = "1a331345f04805badbb495c775a6ddffcdd1a732567d5ec8b3d5749e3c7a5e4b"
MODEL_BYTES = 39_593_761


def load(relative):
    return json.loads((ROOT / relative).read_text(encoding="utf-8"))


class SpeakerModelTests(unittest.TestCase):
    def test_generated_model_is_the_original_real_model(self):
        path = ROOT / "third_party/runtime-assets/models/3dspeaker_eres2net_base.onnx"
        self.assertEqual(path.stat().st_size, MODEL_BYTES)
        with path.open("rb") as stream:
            self.assertEqual(hashlib.file_digest(stream, "sha256").hexdigest(), MODEL_SHA256)

    def test_bazel_and_both_release_authorities_pin_identical_bytes(self):
        bazel = next(
            entry for entry in load("third_party/licenses/downloads.json")["model_files"]
            if entry["repository"] == "3dspeaker_eres2net_base_onnx_source"
        )
        platform = load("tools/platform-release/authority.json")["sharedDownloads"]["3dspeaker"]
        macos = next(
            entry for entry in load("tools/macos-release/authority.json")["downloads"]
            if entry["id"] == "3dspeaker"
        )
        for entry in (bazel, platform, macos):
            with self.subTest(entry=entry):
                self.assertEqual(entry["sha256"], MODEL_SHA256)
                self.assertEqual(entry["url"], bazel["url"])
        for entry in (platform, macos):
            self.assertEqual(entry["bytes"], MODEL_BYTES)
            self.assertNotIn("path", entry)


if __name__ == "__main__":
    unittest.main()

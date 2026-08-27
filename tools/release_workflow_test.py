from __future__ import annotations

import pathlib
import re
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]


class ReleaseWorkflowTest(unittest.TestCase):
    def read(self, name: str) -> str:
        return (ROOT / ".github" / "workflows" / name).read_text(encoding="utf-8")

    def assert_no_duplicate_mapping_keys(self, workflow: str) -> None:
        keys_by_indent: dict[int, set[str]] = {}
        block_scalar_indent: int | None = None
        key_pattern = re.compile(r"^(?P<indent> *)(?P<item>- )?(?P<key>[A-Za-z_][A-Za-z0-9_-]*):")
        for line_number, line in enumerate(workflow.splitlines(), 1):
            indentation = len(line) - len(line.lstrip(" "))
            if block_scalar_indent is not None:
                if not line.strip() or indentation > block_scalar_indent:
                    continue
                block_scalar_indent = None
            match = key_pattern.match(line)
            if match is None:
                continue
            effective_indent = indentation + (2 if match.group("item") else 0)
            if match.group("item"):
                keys_by_indent = {
                    level: keys for level, keys in keys_by_indent.items() if level < effective_indent
                }
            else:
                keys_by_indent = {
                    level: keys for level, keys in keys_by_indent.items() if level <= effective_indent
                }
            keys = keys_by_indent.setdefault(effective_indent, set())
            key = match.group("key")
            self.assertNotIn(key, keys, f"duplicate YAML key {key!r} at line {line_number}")
            keys.add(key)
            if line.rstrip().endswith(("|", ">", "|-", ">-", "|+", ">+")):
                block_scalar_indent = indentation

    def test_release_workflows_have_no_duplicate_mapping_keys(self) -> None:
        for path in sorted((ROOT / ".github" / "workflows").glob("*.yml")):
            with self.subTest(workflow=path.name):
                self.assert_no_duplicate_mapping_keys(path.read_text(encoding="utf-8"))

    def test_component_workflows_are_reusable_build_only_boundaries(self) -> None:
        platform = self.read("platform-modular-release.yml")
        macos = self.read("macos-arm64-release.yml")
        for workflow in (platform, macos):
            self.assertIn("workflow_call:", workflow)
            self.assertIn("release_version:", workflow)
            self.assertNotIn("gh release", workflow)
            self.assertNotIn("contents: write", workflow)
        self.assertIn("runner: windows-11-arm", platform)
        self.assertIn("target: aarch64-pc-windows-msvc", platform)
        self.assertIn("bazel_config: windows_arm64", platform)
        self.assertIn("platform_acceptance.py", macos)

    def test_unified_workflow_is_only_publication_authority(self) -> None:
        workflow = self.read("release.yml")
        self.assertIn("tags:\n      - 'v*.*.*'", workflow)
        self.assertIn('signing_mode = "signed" if event == "push"', workflow)
        self.assertIn("version.split('+', 1)[0]", workflow)
        self.assertIn("uses: ./.github/workflows/platform-modular-release.yml", workflow)
        self.assertIn("uses: ./.github/workflows/macos-arm64-release.yml", workflow)
        self.assertIn("tools/finalize_release.py", workflow)
        self.assertIn("gpg --batch --verify", workflow)
        self.assertIn("if: github.event_name == 'push'", workflow)
        self.assertIn('gh release edit "$RELEASE_TAG" --draft=false', workflow)


if __name__ == "__main__":
    unittest.main()

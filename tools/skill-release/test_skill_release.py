#!/usr/bin/env python3
"""Contract tests for the canonical and packaged Into Markdown skill."""

from __future__ import annotations

import pathlib
import tempfile
import unittest
import zipfile

from skill_release import (
    ALLOWED_FILES,
    FIXED_TIMESTAMP,
    SKILL_NAME,
    SKILL_SOURCE,
    SkillReleaseError,
    checksum_sidecar_matches,
    create_archive,
    materialize,
    validate,
    validate_materialized,
    verify_release,
)


class SkillReleaseTests(unittest.TestCase):
    def test_canonical_skill_has_the_reviewed_structure_and_routing(self) -> None:
        paths = validate()
        self.assertEqual(
            [path.relative_to(SKILL_SOURCE).as_posix() for path in paths],
            [path.as_posix() for path in ALLOWED_FILES],
        )

    def test_archive_is_deterministic_portable_and_self_verifying(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            root = pathlib.Path(name)
            first = create_archive(root / "first.zip")
            second = create_archive(root / "second.zip")
            self.assertEqual(first.read_bytes(), second.read_bytes())
            self.assertTrue(checksum_sidecar_matches(first))
            self.assertTrue(checksum_sidecar_matches(second))
            verify_release(first)
            with zipfile.ZipFile(first) as archive:
                self.assertTrue(all(info.date_time == FIXED_TIMESTAMP for info in archive.infolist()))
                self.assertTrue(all(info.filename.startswith(f"{SKILL_NAME}/") for info in archive.infolist()))
                names = [info.filename for info in archive.infolist()]
                self.assertEqual(names, [f"{SKILL_NAME}/", *sorted(names[1:])])
            first.with_name(first.name + ".sha256").write_text("invalid\n", encoding="ascii")
            with self.assertRaisesRegex(SkillReleaseError, "checksum sidecar"):
                verify_release(first)

    def test_materialized_core_copy_is_byte_identical(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            destination = pathlib.Path(name) / SKILL_NAME
            materialize(destination)
            validate_materialized(destination)
            for relative in ALLOWED_FILES:
                self.assertEqual(
                    (destination / relative).read_bytes(),
                    (SKILL_SOURCE / relative).read_bytes(),
                )

    def test_unexpected_files_and_symbolic_links_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            source = pathlib.Path(name) / SKILL_NAME
            materialize(source)
            (source / "README.md").write_text("unexpected", encoding="utf-8")
            with self.assertRaisesRegex(SkillReleaseError, "exact reviewed file set"):
                validate(source)
            (source / "README.md").unlink()
            (source / "references/linked.md").symlink_to(source / "SKILL.md")
            with self.assertRaisesRegex(SkillReleaseError, "symbolic link"):
                validate(source)


if __name__ == "__main__":
    unittest.main()

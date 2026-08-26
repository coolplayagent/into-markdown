import pathlib
import tempfile
import unittest

from archive import create, extract
from common import ReleaseError, sha256
from release import published_plugin_file


class ArchiveTest(unittest.TestCase):
    def test_published_plugin_name_is_target_unique(self) -> None:
        self.assertEqual(
            published_plugin_file("official.media.whisper.imp"),
            "official.media.whisper-aarch64-apple-darwin.imp",
        )

    def test_archive_is_reproducible_and_extracts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source = root / "source"
            source.mkdir()
            (source / "empty").write_bytes(b"")
            executable = source / "tool"
            executable.write_bytes(b"tool\n")
            executable.chmod(0o755)
            first = root / "first.tar.gz"
            second = root / "second.tar.gz"
            create(source, first, 1_767_225_600)
            create(source, second, 1_767_225_600)
            self.assertEqual(sha256(first), sha256(second))
            destination = root / "destination"
            extract(first, destination)
            self.assertEqual((destination / "empty").read_bytes(), b"")
            self.assertEqual((destination / "tool").read_bytes(), b"tool\n")

    def test_symbolic_link_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source = root / "source"
            source.mkdir()
            (source / "file").write_bytes(b"ok")
            (source / "link").symlink_to("file")
            with self.assertRaises(ReleaseError):
                create(source, root / "archive.tar.gz", 1_767_225_600)


if __name__ == "__main__":
    unittest.main()

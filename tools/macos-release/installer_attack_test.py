import hashlib
import os
import pathlib
import platform
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parent


class InstallerAttackTest(unittest.TestCase):
    def setUp(self) -> None:
        if platform.system() != "Darwin":
            self.skipTest("the release installer is macOS-only")

    def distribution(
        self, root: pathlib.Path, name: str = "distribution", manifest: str = "{}\n"
    ) -> tuple[pathlib.Path, str]:
        distribution = root / name
        (distribution / "bin").mkdir(parents=True)
        (distribution / "archive-manifest.json").write_text(manifest)
        checker = distribution / "bin/archive-check"
        checker.write_text(
            "#!/bin/sh\n"
            "test -f \"$1/archive-manifest.json\" || exit 1\n"
            f"test \"$(cat \"$1/archive-manifest.json\")\" = '{manifest.strip()}' || exit 1\n"
        )
        checker.chmod(0o755)
        installer = distribution / "install"
        installer.write_bytes((ROOT / "install").read_bytes())
        installer.chmod(0o755)
        identity = hashlib.sha256((distribution / "archive-manifest.json").read_bytes()).hexdigest()
        return distribution, identity

    def invoke(self, distribution: pathlib.Path, prefix: pathlib.Path, command: pathlib.Path):
        return subprocess.run(
            [distribution / "install", prefix, command],
            text=True,
            capture_output=True,
            env={"PATH": "/usr/bin:/bin"},
            check=False,
        )

    def test_preplaced_symlink_current_directory_and_corrupt_destination_fail_closed(self) -> None:
        # The installer intentionally rejects a writable ancestor. GitHub's
        # shared runner temp directory can have a collaborative mode, so put
        # the security fixture below the runner user's trusted home instead.
        with tempfile.TemporaryDirectory(dir=pathlib.Path.home()) as temporary:
            root = pathlib.Path(temporary).resolve()
            distribution, identity = self.distribution(root)
            command = root / "command"
            command.mkdir()

            victim = root / "victim"
            victim.mkdir()
            prefix_link = root / "prefix-link"
            prefix_link.symlink_to(victim, target_is_directory=True)
            result = self.invoke(distribution, prefix_link, command)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(list(victim.iterdir()), [])

            prefix = root / "prefix"
            (prefix / "current").mkdir(parents=True)
            result = self.invoke(distribution, prefix, command)
            self.assertNotEqual(result.returncode, 0)
            self.assertTrue((prefix / "current").is_dir())

            prefix = root / "corrupt-prefix"
            old = prefix / "versions" / "previous"
            old.mkdir(parents=True)
            (prefix / "current").symlink_to("versions/previous", target_is_directory=True)
            destination = prefix / "versions" / identity
            destination.mkdir(parents=True)
            (destination / "archive-manifest.json").write_text("ATTACKER\n")
            (destination / "bin").mkdir()
            checker = destination / "bin/archive-check"
            checker.write_text("#!/bin/sh\nexit 1\n")
            checker.chmod(0o755)
            result = self.invoke(distribution, prefix, command)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual((destination / "archive-manifest.json").read_text(), "ATTACKER\n")
            self.assertTrue((prefix / "current").is_symlink())
            self.assertEqual(os.readlink(prefix / "current"), "versions/previous")

    def test_verified_installed_version_can_upgrade_to_a_new_archive(self) -> None:
        with tempfile.TemporaryDirectory(dir=pathlib.Path.home()) as temporary:
            root = pathlib.Path(temporary).resolve()
            old, old_identity = self.distribution(root, "old", "{}\n")
            new, new_identity = self.distribution(root, "new", '{"new":true}\n')
            prefix = root / "prefix"
            command = root / "command"
            command.mkdir()

            result = self.invoke(old, prefix, command)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(os.readlink(prefix / "current"), f"versions/{old_identity}")
            result = self.invoke(new, prefix, command)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(os.readlink(prefix / "current"), f"versions/{new_identity}")
            self.assertEqual(os.readlink(command / "into-md"), f"{prefix}/current/bin/into-md")
            self.assertTrue((prefix / "versions" / old_identity).is_dir())
            self.assertTrue((prefix / "versions" / new_identity).is_dir())


if __name__ == "__main__":
    unittest.main()

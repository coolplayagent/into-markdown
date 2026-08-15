import hashlib
import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parent


class InstallerAttackTest(unittest.TestCase):
    def setUp(self) -> None:
        if os.uname().sysname != "Darwin":
            self.skipTest("the release installer is macOS-only")

    def distribution(self, root: pathlib.Path) -> tuple[pathlib.Path, str]:
        distribution = root / "distribution"
        (distribution / "bin").mkdir(parents=True)
        (distribution / "archive-manifest.json").write_text("{}\n")
        checker = distribution / "bin/archive-check"
        checker.write_text(
            "#!/bin/sh\n"
            "test -f \"$1/archive-manifest.json\" || exit 1\n"
            "test \"$(cat \"$1/archive-manifest.json\")\" = '{}' || exit 1\n"
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
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
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

            (prefix / "current").rmdir()
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


if __name__ == "__main__":
    unittest.main()

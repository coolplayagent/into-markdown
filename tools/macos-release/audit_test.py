import hashlib
import pathlib
import tempfile
import unittest

from audit import audit_embedded_paths, dependency_identities, strip_tool_header
from common import ReleaseError


class AuditTest(unittest.TestCase):
    def test_otool_header_removal_preserves_the_first_dependency(self) -> None:
        output = (
            "/private/tmp/into-md:\n"
            "\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0)\n"
            "\t/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation "
            "(compatibility version 150.0.0)\n"
        )
        dependencies = dependency_identities(strip_tool_header(output))
        self.assertEqual(
            dependencies,
            [
                "/usr/lib/libSystem.B.dylib",
                "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation",
            ],
        )

    def test_developer_path_mutations_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            candidate = pathlib.Path(temporary) / "binary"
            for value in (
                b"/Users/attacker/repo",
                b"/home/builder/cargo",
                b"/private/tmp/into-md-target/release",
                b"C:\\Users\\builder",
            ):
                candidate.write_bytes(b"Mach-O\0" + value + b"\0")
                with self.assertRaises(ReleaseError):
                    audit_embedded_paths(candidate)
            candidate.write_bytes(b"/usr/src/into-markdown\0")
            audit_embedded_paths(candidate)

    def test_signed_derivative_requires_exact_in_process_source_binding(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            candidate = pathlib.Path(temporary) / "libpdfium.dylib"
            candidate.write_bytes(b"Mach-O\0/Users/runner/work/pdfium-binaries/\0signed")
            with self.assertRaises(ReleaseError):
                audit_embedded_paths(candidate)
            digest = hashlib.sha256(candidate.read_bytes()).hexdigest()
            audit_embedded_paths(
                candidate,
                {
                    digest: "33c98063af28c0b7cbf8227f4422bf5c15942df2455cf7f0a5dce3dc601d52b0"
                },
            )


if __name__ == "__main__":
    unittest.main()

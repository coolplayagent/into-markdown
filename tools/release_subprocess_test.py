from __future__ import annotations

import contextlib
import io
import os
import pathlib
import sys
import unittest
from unittest import mock

import release_subprocess
from release_subprocess import ReleaseError, run


class ReleaseSubprocessTests(unittest.TestCase):
    def setUp(self) -> None:
        self.previous_stream_logs = os.environ.get("INTO_MD_RELEASE_STREAM_LOGS")
        os.environ["INTO_MD_RELEASE_STREAM_LOGS"] = "1"

    def tearDown(self) -> None:
        if self.previous_stream_logs is None:
            os.environ.pop("INTO_MD_RELEASE_STREAM_LOGS", None)
        else:
            os.environ["INTO_MD_RELEASE_STREAM_LOGS"] = self.previous_stream_logs

    def test_streaming_preserves_captured_stdout_without_echoing_tool_output(self) -> None:
        console = io.StringIO()
        with contextlib.redirect_stdout(console):
            output = run([sys.executable, "-c", "print('machine-readable-result')"])

        self.assertEqual(output.strip(), "machine-readable-result")
        self.assertNotIn("machine-readable-result", console.getvalue())
        self.assertIn("[release] start", console.getvalue())
        self.assertIn("[release] finish", console.getvalue())

    def test_build_tool_stdout_and_all_stderr_are_streamed(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        executable = pathlib.Path(sys.executable).name.casefold()
        with mock.patch.object(release_subprocess, "_BUILD_TOOLS", {executable}):
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                output = run(
                    [
                        sys.executable,
                        "-c",
                        "import sys; print('build-output'); print('warning', file=sys.stderr)",
                    ]
                )

        self.assertEqual(output.strip(), "build-output")
        self.assertIn("build-output", stdout.getvalue())
        self.assertIn("warning", stderr.getvalue())

    def test_streaming_drains_both_pipes_without_deadlock(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        script = (
            "import sys; "
            "[(print(f'out-{index}'), print(f'err-{index}', file=sys.stderr)) "
            "for index in range(512)]"
        )
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            output = run([sys.executable, "-c", script])

        self.assertIn("out-511", output)
        self.assertIn("err-511", stderr.getvalue())

    def test_command_failure_preserves_bounded_diagnostic_tail(self) -> None:
        script = (
            "import sys; "
            "[print(f'diagnostic-{index}', file=sys.stderr) for index in range(45)]; "
            "raise SystemExit(7)"
        )
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(ReleaseError) as raised:
                run([sys.executable, "-c", script])
        message = str(raised.exception)
        self.assertIn("exit 7", message)
        self.assertNotIn("diagnostic-4\n", message)
        self.assertIn("diagnostic-5", message)
        self.assertIn("diagnostic-44", message)


if __name__ == "__main__":
    unittest.main()

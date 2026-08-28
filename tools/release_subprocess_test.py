from __future__ import annotations

import contextlib
import io
import os
import sys
import threading
import unittest

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

    def test_streaming_preserves_and_echoes_captured_stdout(self) -> None:
        console = io.StringIO()
        with contextlib.redirect_stdout(console):
            output = run([sys.executable, "-c", "print('machine-readable-result')"])

        self.assertEqual(output.strip(), "machine-readable-result")
        self.assertIn("machine-readable-result", console.getvalue())
        self.assertIn("[release] start", console.getvalue())
        self.assertIn("[release] finish", console.getvalue())

    def test_stdout_and_stderr_are_forwarded_before_process_exit(self) -> None:
        class SignalingStream(io.StringIO):
            def __init__(self, marker: str) -> None:
                super().__init__()
                self.marker = marker
                self.seen = threading.Event()

            def write(self, value: str) -> int:
                written = super().write(value)
                if self.marker in value:
                    self.seen.set()
                return written

        stdout = SignalingStream("stdout-live")
        stderr = SignalingStream("stderr-live")
        result: list[str] = []
        script = (
            "import sys,time; "
            "print('stdout-live', flush=True); "
            "print('stderr-live', file=sys.stderr, flush=True); "
            "time.sleep(1)"
        )
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            worker = threading.Thread(
                target=lambda: result.append(run([sys.executable, "-u", "-c", script]))
            )
            worker.start()
            self.assertTrue(stdout.seen.wait(5), "stdout was not streamed")
            self.assertTrue(stderr.seen.wait(5), "stderr was not streamed")
            self.assertTrue(worker.is_alive(), "output was forwarded only after process exit")
            worker.join(5)

        self.assertFalse(worker.is_alive(), "release subprocess did not finish")
        self.assertEqual(result[0].strip(), "stdout-live")
        self.assertIn("stderr-live", stderr.getvalue())

    def test_all_stdout_and_stderr_are_streamed(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
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

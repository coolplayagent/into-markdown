"""Shared subprocess execution for native release tooling."""

from __future__ import annotations

import os
import pathlib
import subprocess
import sys
import threading
import time
from collections.abc import Iterable, Mapping


class ReleaseError(RuntimeError):
    """Stable packaging failure."""


def run(
    arguments: Iterable[str],
    *,
    cwd: pathlib.Path | None = None,
    env: Mapping[str, str] | None = None,
) -> str:
    """Run a release subprocess and optionally stream both output channels live."""
    command = [str(argument) for argument in arguments]
    stream = os.environ.get("INTO_MD_RELEASE_STREAM_LOGS") == "1"
    started = time.monotonic()
    if stream:
        executable = pathlib.Path(command[0]).name
        phase = command[1] if len(command) > 1 and not command[1].startswith("-") else "run"
        print(f"[release] start {executable} {phase}", flush=True)
        process = subprocess.Popen(
            command,
            cwd=cwd,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            errors="replace",
            bufsize=1,
        )
        stdout_lines: list[str] = []
        stderr_lines: list[str] = []

        def drain(pipe, lines: list[str], output) -> None:
            if pipe is None:
                return
            try:
                for line in pipe:
                    lines.append(line)
                    output.write(line)
                    output.flush()
            finally:
                pipe.close()

        stdout_thread = threading.Thread(
            target=drain,
            args=(process.stdout, stdout_lines, sys.stdout),
        )
        stderr_thread = threading.Thread(
            target=drain,
            args=(process.stderr, stderr_lines, sys.stderr),
        )
        stdout_thread.start()
        stderr_thread.start()
        returncode = process.wait()
        stdout_thread.join()
        stderr_thread.join()
        stdout = "".join(stdout_lines)
        stderr = "".join(stderr_lines)
        print(
            f"[release] finish {executable} {phase} in {time.monotonic() - started:.1f}s "
            f"(exit {returncode})",
            flush=True,
        )
    else:
        completed = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            errors="replace",
        )
        returncode = completed.returncode
        stdout = completed.stdout
        stderr = completed.stderr
    if returncode:
        # Cargo and native linkers commonly finish with a generic summary. Keep
        # enough of the diagnostic tail to expose the compiler or linker error
        # without flooding hosted-runner logs with the entire build.
        detail = stderr.strip().splitlines()[-40:] or ["no diagnostic"]
        rendered = "\n".join(detail)
        raise ReleaseError(
            f"command failed ({command[0]}, exit {returncode}):\n{rendered}"
        )
    return stdout

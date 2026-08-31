"""Explicit baseline updates with a short exclusive writer reservation."""

import os
import tempfile

from .model import GateError


def replace_baseline(path, expected, content):
    lock = path.with_suffix(".lock")
    try:
        descriptor = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    except FileExistsError as error:
        raise GateError(f"baseline writer is active ({lock}); retry after it finishes") from error
    temporary = None
    try:
        os.close(descriptor)
        actual = path.read_bytes() if path.exists() else None
        if actual != expected:
            raise GateError("baseline changed during analysis; rerun ratchet")
        with tempfile.NamedTemporaryFile(dir=path.parent, prefix="baseline-", suffix=".tmp", delete=False) as stream:
            temporary = stream.name
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        temporary = None
    finally:
        if temporary:
            os.unlink(temporary)
        lock.unlink()

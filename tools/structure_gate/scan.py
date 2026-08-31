"""Collect compact metrics while discarding each file's syntax tree."""

import hashlib
import pathlib

from .model import GateError, Metric
from .python_metrics import analyze_python
from .source import EXTENSIONS, exclusion
from .tree_metrics import analyze_tree


def analyze(path: str, data: bytes) -> Metric:
    try:
        text = data.decode("utf-8-sig")
    except UnicodeDecodeError as error:
        raise GateError(f"{path}: source is not UTF-8") from error
    metric = Metric(path, hashlib.sha256(data).hexdigest(), len(text.splitlines()), 0)
    extension = pathlib.PurePosixPath(path).suffix
    if extension == ".py":
        analyze_python(text, metric)
    else:
        analyze_tree(text.encode("utf-8"), extension, metric)
    return metric


def scan(source):
    metrics = {}
    excluded = []
    with source.blobs() as read:
        for path in sorted(source.entries):
            if pathlib.PurePosixPath(path).suffix not in EXTENSIONS:
                continue
            reason = exclusion(path)
            if reason:
                excluded.append({"path": path, "reason": reason})
                continue
            metrics[path] = analyze(path, read(path))
    return metrics, excluded

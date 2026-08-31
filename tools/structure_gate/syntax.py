"""Small shared helpers for source spans and review-only hints."""

import hashlib
import re

from .model import Allowance


def line_span(start: int, end: int, rows: set[int]) -> int:
    return sum(row in rows for row in range(start, end + 1))


def comment_allowances(comments, functions, rules, language):
    result = []
    pattern = (r"(?:noqa\b\s*:?|pylint:\s*disable(?:-next)?\s*=)\s*([^\n]*)"
               if language == "python" else r"eslint-disable(?:-next-line|-line)?\b\s*(.*)")
    for line, comment in comments:
        if language == "python" and "pylint: skip-file" in comment:
            result.extend(Allowance("<file>", rule, "file", line) for rule in sorted(rules) if rule.startswith("too-"))
            continue
        match = re.search(pattern, comment)
        if not match:
            continue
        before_reason, _, reason = match.group(1).rstrip(" */").partition(" -- ")
        words = set(re.findall(r"[\w-]+", before_reason))
        aliases = {"R0912": "too-many-branches", "R0913": "too-many-arguments",
                   "R0914": "too-many-locals", "R0915": "too-many-statements"}
        words = {aliases.get(word, word) for word in words}
        selected = rules if not words or "all" in words else rules.intersection(words)
        if "design" in words and "pylint:" in comment:
            selected = selected | {rule for rule in rules if rule.startswith("too-")}
        owner = min((fn for fn in functions if fn.line <= line <= fn.end
                     or fn.line == line + 1), key=lambda fn: fn.end - fn.line, default=None)
        wide = ("eslint-disable" in comment and not re.search(r"eslint-disable-(?:next-line|line)", comment)
                or "flake8: noqa" in comment or "ruff: noqa" in comment
                or "pylint:" in comment and "disable-next" not in comment
                and not (owner and owner.line <= line <= owner.end))
        for rule in sorted(selected):
            result.append(Allowance(owner.symbol if owner and not wide else "<file>", rule,
                                    "item" if owner and not wide else "file", line, reason.strip()))
    return result


def branch_hint(path, line, text, lines):
    if lines < 15:
        return None
    normalized = re.sub(r"\s+", " ", text).strip()
    return {"kind": "branch-fingerprint", "path": path, "line": line,
            "lines": lines, "digest": hashlib.sha256(normalized.encode()).hexdigest()}


def filename_hint(path, line, text):
    if (re.search(r"file_?name|basename|\.name\b", text, re.IGNORECASE)
            and re.search(r'''["'][^"'\n/\\]+\.[a-zA-Z0-9]{1,8}["']''', text)):
        return {"kind": "filename-condition", "path": path, "line": line,
                "message": "Review filename-specific condition; may be legitimate dispatch."}
    return None

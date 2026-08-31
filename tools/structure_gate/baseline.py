"""Frozen historical debt and monotonic comparison, independent of Git and CLI."""

import json
from collections import Counter

from .model import FILE_LIMIT, FUNCTION_LIMIT, GateError
from .source import canonical_path


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise GateError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def decode(data: bytes, label: str):
    try:
        return json.loads(data, object_pairs_hook=unique_object)
    except (ValueError, UnicodeDecodeError) as error:
        raise GateError(f"{label}: {error}") from error


def encode(value) -> bytes:
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


def freeze(metrics):
    files = []
    for path, metric in sorted(metrics.items()):
        functions = [{"symbol": fn.symbol, "lines": fn.lines} for fn in metric.functions if fn.lines > FUNCTION_LIMIT]
        allowances = [{"key": key, "count": count} for key, count in sorted(Counter(a.key for a in metric.allowances).items())]
        lines = metric.code_lines if metric.code_lines > FILE_LIMIT else None
        if lines is not None or functions or allowances:
            files.append({"path": path, "lines": lines, "functions": sorted(functions, key=lambda x: x["symbol"]),
                          "allowances": allowances})
    return {"version": 1, "limits": {"file": FILE_LIMIT, "function": FUNCTION_LIMIT}, "files": files}


def load_baseline(data):
    result = decode(data, "baseline")
    if (not isinstance(result, dict) or set(result) != {"version", "limits", "files"}
            or result["version"] != 1 or result["limits"] != {"file": FILE_LIMIT, "function": FUNCTION_LIMIT}
            or not isinstance(result["files"], list)):
        raise GateError("invalid baseline schema or thresholds")
    seen = set()
    for item in result["files"]:
        if not isinstance(item, dict) or set(item) != {"path", "lines", "functions", "allowances"}:
            raise GateError("invalid baseline file record")
        if not isinstance(item["path"], str):
            raise GateError("invalid baseline path")
        path = canonical_path(item["path"])
        if path in seen:
            raise GateError(f"duplicate baseline path: {path}")
        seen.add(path)
        item["path"] = path
        if item["lines"] is not None and (type(item["lines"]) is not int or item["lines"] <= FILE_LIMIT):
            raise GateError(f"invalid historical file limit: {path}")
        validate_entries(item["functions"], "symbol", "lines", FUNCTION_LIMIT)
        validate_entries(item["allowances"], "key", "count", 0)
    return result


def validate_entries(entries, identity, number, minimum):
    if not isinstance(entries, list):
        raise GateError("baseline records must be lists")
    seen = set()
    for entry in entries:
        if (not isinstance(entry, dict) or set(entry) != {identity, number}
                or not isinstance(entry[identity], str) or not entry[identity]
                or type(entry[number]) is not int or entry[number] <= minimum):
            raise GateError("invalid baseline metric record")
        if entry[identity] in seen:
            raise GateError(f"duplicate baseline identity: {entry[identity]}")
        seen.add(entry[identity])


def load_authority(data):
    records = decode(data, "exception authority") if data is not None else []
    if not isinstance(records, list):
        raise GateError("exception authority must be a list")
    result = {}
    for item in records:
        if (not isinstance(item, dict) or set(item) != {"path", "symbol", "rule", "reason", "issue"}
                or any(not isinstance(value, str) or not value.strip() for value in item.values())):
            raise GateError("exception needs path, symbol, rule, reason and issue")
        import re
        if not re.fullmatch(r"https://github\.com/coolplayagent/into-markdown/issues/[1-9][0-9]*", item["issue"]):
            raise GateError("exception must link a repository issue")
        key = (canonical_path(item["path"]), f"item|{item['symbol']}|{item['rule']}")
        if key in result:
            raise GateError(f"duplicate exception authority: {key}")
        result[key] = item
    return result


def pure_renames(before, after):
    deleted = {}
    for path in sorted(before.keys() - after.keys()):
        deleted.setdefault(before[path].digest, []).append(path)
    result = {}
    for path in sorted(after.keys() - before.keys()):
        choices = deleted.get(after[path].digest, [])
        if choices:
            result[path] = choices.pop(0)
    return result


def compare(before, after, baseline, authority):
    debt = {item["path"]: item for item in baseline["files"]}
    renames = pure_renames(before, after)
    violations = []
    for path, metric in sorted(after.items()):
        historical = debt.get(renames.get(path, path), {})
        limit = historical.get("lines") or FILE_LIMIT
        if metric.code_lines > limit:
            violations.append(f"{path}: file production lines {metric.code_lines} > {limit}")
        functions = {item["symbol"]: item["lines"] for item in historical.get("functions", [])}
        for fn in metric.functions:
            limit = functions.get(fn.symbol, FUNCTION_LIMIT)
            if fn.lines > limit:
                violations.append(f"{path}:{fn.line}: {fn.symbol} production lines {fn.lines} > {limit}")
        allowed = Counter({item["key"]: item["count"] for item in historical.get("allowances", [])})
        for allowance in metric.allowances:
            key = (path, allowance.key)
            record = authority.get(key)
            if record and (not allowance.reason or allowance.reason != record["reason"]):
                violations.append(f"{path}:{allowance.line}: exception reason must match inline explanation")
            if allowed[allowance.key] > 0:
                allowed[allowance.key] -= 1
                continue
            if allowance.scope != "item" or not record:
                violations.append(f"{path}:{allowance.line}: new structural allowance {allowance.key}")
    return violations

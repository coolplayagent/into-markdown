"""Deterministic comparison reports and advisory-only pattern grouping."""

from collections import defaultdict

from .baseline import pure_renames


def hints(metrics):
    branches = defaultdict(list)
    result = []
    for metric in metrics.values():
        for hint in metric.hints:
            if hint["kind"] == "branch-fingerprint":
                branches[hint["digest"]].append({key: value for key, value in hint.items() if key != "digest"})
            else:
                result.append(hint)
    for locations in branches.values():
        if len(locations) >= 3:
            result.append({"kind": "repeated-long-branch", "locations": locations,
                           "message": "Review repeated syntax; no semantic equivalence is assumed."})
    return result


def make_report(before, after, excluded, base_ref, violations):
    files = []
    renames = pure_renames(before, after)
    for path in sorted(before.keys() | after.keys()):
        old = before.get(renames.get(path, path))
        new = after.get(path)
        files.append({"path": path, "renamed_from": renames.get(path),
                      "base": old.json() if old else None, "candidate": new.json() if new else None,
                      "delta": {"code_lines": (new.code_lines if new else 0) - (old.code_lines if old else 0),
                                "max_function_lines": (new.maximum if new else 0) - (old.maximum if old else 0),
                                "allowances": len(new.allowances if new else []) - len(old.allowances if old else [])}})
    return {"base_ref": base_ref, "files": files, "excluded": excluded,
            "hints": hints(after), "violations": violations}


def print_text(report):
    print("Production lines (base -> candidate, delta); longest function; allowances; physical lines")
    for item in report["files"]:
        old, new = item["base"], item["candidate"]
        value = lambda record, key: record[key] if record else 0
        maximum = lambda record: max((fn["lines"] for fn in record["functions"]), default=0) if record else 0
        print(f"{item['path']}: {value(old, 'code_lines')} -> {value(new, 'code_lines')} "
              f"({item['delta']['code_lines']:+}); function {maximum(old)} -> {maximum(new)}; "
              f"allows {len(old['allowances']) if old else 0} -> {len(new['allowances']) if new else 0}; "
              f"physical {value(old, 'physical_lines')} -> {value(new, 'physical_lines')}")
    for item in report["excluded"]:
        print(f"EXCLUDED {item['path']}: {item['reason']}")
    for hint in report["hints"]:
        print(f"REVIEW {hint}")
    for violation in report["violations"]:
        print(f"FAIL {violation}")
    print(f"{len(report['files'])} files; {len(report['violations'])} violations")

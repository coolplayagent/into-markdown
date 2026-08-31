import copy
import unittest

from tools.structure_gate.baseline import compare, encode, freeze, load_authority, load_baseline
from tools.structure_gate.model import GateError
from tools.structure_gate.scan import analyze


def file_metric(lines, path="src/large.rs"):
    return analyze(path, "\n".join(f"const C{i}: i32 = {i};" for i in range(lines)).encode())


def function_metric(lines, path="src/function.rs", name="large"):
    body = "\n".join("    work();" for _ in range(lines - 2))
    return analyze(path, f"fn {name}() {{\n{body}\n}}".encode())


class BaselineTests(unittest.TestCase):
    def test_existing_debt_is_frozen_and_can_decrease(self):
        before = {"src/large.rs": file_metric(2000)}
        baseline = freeze(before)
        self.assertEqual(compare(before, before, baseline, {}), [])
        after = {"src/large.rs": file_metric(1900)}
        self.assertEqual(compare(before, after, baseline, {}), [])
        self.assertEqual(freeze(after)["files"][0]["lines"], 1900)
        self.assertTrue(compare(before, {"src/large.rs": file_metric(2001)}, baseline, {}))

    def test_new_files_and_functions_use_default_limits(self):
        for metric in (file_metric(2000), function_metric(101)):
            with self.subTest(path=metric.path):
                self.assertTrue(compare({}, {metric.path: metric}, freeze({}), {}))
        for metric in (file_metric(1000), function_metric(100)):
            with self.subTest(path=metric.path):
                self.assertEqual(compare({}, {metric.path: metric}, freeze({}), {}), [])

    def test_each_function_is_frozen_not_only_file_maximum(self):
        old = function_metric(200)
        before = {old.path: old}
        a = function_metric(190)
        a.functions.extend(function_metric(110, name="new_function").functions)
        violations = compare(before, {a.path: a}, freeze(before), {})
        self.assertEqual(len(violations), 1)
        self.assertIn("new_function", violations[0])

    def test_split_and_deletion_remove_historical_allowances(self):
        before = {"src/large.rs": file_metric(2000)}
        after = {path: file_metric(1000, path) for path in ("src/large.rs", "src/other.rs")}
        self.assertEqual(compare(before, after, freeze(before), {}), [])
        self.assertEqual(freeze(after)["files"], [])
        self.assertEqual(freeze({})["files"], [])

    def test_pure_rename_transfers_but_copy_does_not(self):
        before = {"src/large.rs": file_metric(2000)}
        after = {"src/moved.rs": file_metric(2000, "src/moved.rs")}
        self.assertEqual(compare(before, after, freeze(before), {}), [])
        self.assertTrue(compare(before, before | after, freeze(before), {}))
        after["src/moved.rs"] = file_metric(1999, "src/moved.rs")
        self.assertTrue(compare(before, after, freeze(before), {}))

    def test_deleting_one_allow_does_not_authorize_another(self):
        def metric(name):
            return analyze("src/x.rs", f"#[allow(clippy::too_many_lines)]\nfn {name}() {{}}".encode())
        before, after = {"src/x.rs": metric("old")}, {"src/x.rs": metric("new")}
        self.assertTrue(compare(before, after, freeze(before), {}))

    def test_new_local_allow_needs_exact_authority_and_inline_reason(self):
        metric = analyze("src/x.rs", b'#[allow(clippy::too_many_lines, reason = "linear state machine")]\nfn f() {}')
        record = {"path": metric.path, "symbol": "f", "rule": "too_many_lines", "reason": "linear state machine",
                  "issue": "https://github.com/coolplayagent/into-markdown/issues/277"}
        authority = load_authority(encode([record]))
        self.assertEqual(compare({}, {metric.path: metric}, freeze({}), authority), [])
        metric.allowances[0].reason = ""
        self.assertTrue(compare({}, {metric.path: metric}, freeze({}), authority))
        metric.allowances[0].scope = "file"
        self.assertTrue(compare({}, {metric.path: metric}, freeze({}), authority))

    def test_baseline_schema_paths_and_duplicates_fail_closed(self):
        good = freeze({"src/large.rs": file_metric(2000)})
        self.assertEqual(load_baseline(encode(good)), good)
        bad = copy.deepcopy(good)
        bad["files"].append(copy.deepcopy(bad["files"][0]))
        with self.assertRaises(GateError):
            load_baseline(encode(bad))
        for path in ("../outside.rs", "C:/outside.rs", "/outside.rs", "src/../../outside.rs"):
            bad = copy.deepcopy(good)
            bad["files"][0]["path"] = path
            with self.subTest(path=path), self.assertRaises(GateError):
                load_baseline(encode(bad))
        for data in (b"{", b'{"version": 1, "version": 2}', b"null"):
            with self.assertRaises(GateError):
                load_baseline(data)

    def test_windows_separators_normalize_and_detect_duplicate_identity(self):
        good = freeze({"src/large.rs": file_metric(2000)})
        windows = copy.deepcopy(good)
        windows["files"][0]["path"] = "src\\large.rs"
        self.assertEqual(load_baseline(encode(windows)), good)
        windows["files"].append(good["files"][0])
        with self.assertRaises(GateError):
            load_baseline(encode(windows))


if __name__ == "__main__":
    unittest.main()

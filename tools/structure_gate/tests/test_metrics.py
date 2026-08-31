import unittest

from tools.structure_gate.model import GateError
from tools.structure_gate.scan import analyze


class MetricTests(unittest.TestCase):
    def metric(self, text, extension="rs"):
        return analyze("src/example." + extension, text.encode())

    def test_rust_test_module_and_its_attributes_are_excluded(self):
        source = '''// prose
#[allow(clippy::too_many_lines)]
#[cfg(test)]
mod tests {
    fn giant() { let a = 1; }
}
fn production() {
    // prose
    let a = 1;
}
'''
        metric = self.metric(source)
        self.assertEqual(metric.physical_lines, 10)
        self.assertEqual(metric.code_lines, 3)
        self.assertEqual([f.symbol for f in metric.functions], ["production"])
        self.assertEqual(metric.allowances, [])

    def test_nested_cfg_test_logic_and_direct_test_attributes(self):
        for attribute in ("cfg(all(test, unix))", "cfg(any(test, all(test, unix)))",
                          "test", "tokio::test(flavor = \"multi_thread\")"):
            with self.subTest(attribute=attribute):
                result = self.metric(f"#[{attribute}]\nmod tests {{ fn helper() {{}} }}\nfn prod() {{}}")
                self.assertEqual(result.code_lines, 1)
                self.assertEqual([fn.symbol for fn in result.functions], ["prod"])
        for attribute in ("cfg(any(test, unix))", "cfg(not(test))", 'cfg(feature = "test")'):
            with self.subTest(attribute=attribute):
                self.assertEqual(len(self.metric(f"#[{attribute}]\nfn real() {{}}").functions), 1)

    def test_rust_literals_comments_and_raw_identifier_pattern(self):
        metric = self.metric('''fn parse() {
let text = r###"fn fake() {} #[allow(clippy::too_many_lines)]"###;
/* fn fake2() {} */
match x { raw @ (A | B) => {}, _ => {} }
}
''')
        self.assertEqual(metric.code_lines, 4)
        self.assertEqual(len(metric.functions), 1)
        self.assertEqual(metric.allowances, [])

    def test_macros_and_closures_have_stable_scopes(self):
        metric = self.metric('''macro_rules! branches { ($x:expr) => { match $x { _ => {} } }; }
impl Thing { fn parse() { let handler = |x| { x }; } }
''')
        self.assertEqual([fn.symbol for fn in metric.functions],
                         ["branches", "impl Thing::parse", "impl Thing::parse::handler"])

    def test_allowances_are_syntax_not_strings(self):
        metric = self.metric('''#[allow(clippy::too_many_lines, reason = "linear state machine")]
fn parse() {}
#[cfg_attr(feature = "tiny", allow(clippy::type_complexity))]
type Callback = fn();
''')
        self.assertEqual([(a.symbol, a.rule) for a in metric.allowances],
                         [("parse", "too_many_lines"), ("Callback", "type_complexity")])
        self.assertEqual(metric.allowances[0].reason, "linear state machine")

    def test_group_and_file_allowances(self):
        metric = self.metric("#![allow(warnings)]\nfn f() {}")
        self.assertEqual(len(metric.allowances), 4)
        self.assertTrue(all(item.scope == "file" for item in metric.allowances))
        metric = self.metric("#[allow(clippy::complexity)]\nfn f() {}")
        self.assertEqual({a.rule for a in metric.allowances}, {"too_many_arguments", "type_complexity"})

    def test_python_decorators_docstrings_and_nested_functions(self):
        metric = self.metric('''"""module docs"""
@decorate
def outer():
    """docs"""
    def inner():
        return 1
    return inner()
''', "py")
        self.assertEqual(metric.code_lines, 5)
        self.assertEqual([(f.symbol, f.lines) for f in metric.functions], [("outer", 5), ("outer::inner", 2)])
        metric = self.metric('def f(): "docs"; return 1\n', "py")
        self.assertEqual(metric.code_lines, 1)

    def test_python_actual_suppressions_only(self):
        metric = self.metric('value = "# noqa: C901"\n# noqa: C901 -- justified\ndef f():\n    return 1\n', "py")
        self.assertEqual([(a.symbol, a.rule, a.reason) for a in metric.allowances], [("f", "C901", "justified")])
        self.assertTrue(self.metric("def f():  # noqa\n    return 1\n", "py").allowances)

    def test_pylint_global_aliases_and_local_scope(self):
        for directive in ("disable=R0913", "disable=too-many-arguments", "disable=design", "skip-file"):
            with self.subTest(directive=directive):
                result = self.metric(f"# pylint: {directive}\ndef f():\n    return 1\n", "py")
                self.assertTrue(result.allowances)
                self.assertTrue(all(a.scope == "file" for a in result.allowances))
        result = self.metric("def f():\n    # pylint: disable=R0913 -- local\n    return 1\n", "py")
        self.assertEqual(result.allowances[0].scope, "item")

    def test_tsx_arrow_methods_and_comments(self):
        metric = self.metric('''// eslint-disable-next-line max-lines-per-function -- linear JSX
const App = () => (
  <div>{"function fake() {}"}</div>
);
class View { render() { return <App />; } }
''', "tsx")
        self.assertEqual([f.symbol for f in metric.functions], ["App", "View::render"])
        self.assertEqual(metric.allowances[0].symbol, "App")
        self.assertEqual(metric.allowances[0].reason, "linear JSX")
        self.assertTrue(self.metric("/* eslint-disable */\nconst f = () => 1;", "ts").allowances)

    def test_malformed_source_never_silently_passes(self):
        for extension, text in (("rs", "fn broken( {"), ("ts", "function broken( {"),
                                ("py", "def broken(:"), ("rs", "fn f() { match x { raw @ => {} } }")):
            with self.subTest(extension=extension):
                with self.assertRaises(GateError):
                    self.metric(text, extension)

    def test_bom_crlf_and_comments_count_consistently(self):
        a = self.metric("// note\nfn f() {\n    x();\n}\n")
        b = self.metric("\ufeff// note\r\nfn f() {\r\n    x();\r\n}\r\n")
        self.assertEqual(a.code_lines, b.code_lines)
        self.assertEqual(a.functions, b.functions)


if __name__ == "__main__":
    unittest.main()

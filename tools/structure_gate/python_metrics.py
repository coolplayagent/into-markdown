"""Python AST and tokenizer metrics, without importing inspected modules."""

import ast
import io
import tokenize

from .model import Function, GateError, PYTHON_LINTS, symbol_name
from .syntax import branch_hint, comment_allowances, filename_hint, line_span


def analyze_python(text: str, metric) -> None:
    try:
        tree = ast.parse(text, filename=metric.path)
        tokens = list(tokenize.generate_tokens(io.StringIO(text).readline))
    except (SyntaxError, tokenize.TokenError) as error:
        raise GateError(f"{metric.path}: Python parse failed: {error}") from error
    docstrings = []
    for node in ast.walk(tree):
        body = getattr(node, "body", None)
        if (isinstance(node, (ast.Module, ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))
                and isinstance(body, list) and body and isinstance(body[0], ast.Expr)
                and isinstance(body[0].value, ast.Constant) and isinstance(body[0].value.value, str)):
            docstrings.append(body[0].value)
    ignored = {tokenize.COMMENT, tokenize.NL, tokenize.NEWLINE, tokenize.INDENT,
               tokenize.DEDENT, tokenize.ENDMARKER, tokenize.ENCODING}
    rows = set()
    comments = []
    for token in tokens:
        if token.type == tokenize.COMMENT:
            comments.append((token.start[0], token.string))
        is_docstring = token.type == tokenize.STRING and any(
            node.lineno <= token.start[0] <= token.end[0] <= node.end_lineno for node in docstrings)
        if token.type not in ignored and not is_docstring:
            rows.update(range(token.start[0], token.end[0] + 1))
    source_lines = text.splitlines()
    rows = {row for row in rows if source_lines[row - 1].strip()}
    seen = {}

    def visit(node, scope):
        next_scope = scope
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            name = symbol_name([*scope, node.name], seen)
            next_scope = [*scope, node.name]
            if not isinstance(node, ast.ClassDef):
                start = min([node.lineno, *(item.lineno for item in node.decorator_list)])
                metric.functions.append(Function(name, start, node.end_lineno,
                                                 line_span(start, node.end_lineno, rows)))
        if isinstance(node, ast.Lambda):
            name = symbol_name([*scope, "<lambda>"], seen)
            metric.functions.append(Function(name, node.lineno, node.end_lineno,
                                             line_span(node.lineno, node.end_lineno, rows)))
        if isinstance(node, (ast.If, ast.Match)):
            snippet = ast.get_source_segment(text, node) or ""
            hint = filename_hint(metric.path, node.lineno, snippet.split(":", 1)[0])
            if hint:
                metric.hints.append(hint)
            hint = branch_hint(metric.path, node.lineno, ast.dump(node, include_attributes=False),
                               line_span(node.lineno, node.end_lineno, rows))
            if hint:
                metric.hints.append(hint)
        for child in ast.iter_child_nodes(node):
            visit(child, next_scope)

    visit(tree, [])
    metric.code_lines = len(rows)
    metric.allowances = comment_allowances(comments, metric.functions, PYTHON_LINTS, "python")

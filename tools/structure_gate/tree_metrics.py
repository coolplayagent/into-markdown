"""Syntax-aware Rust/TS/TSX metrics; one parse tree lives at a time."""

import re

from .model import Allowance, Function, RUST_LINTS, TS_LINTS, symbol_name
from .parsing import parse
from .rust_attributes import test_only
from .syntax import branch_hint, comment_allowances, filename_hint, line_span

COMMENTS = {"comment", "line_comment", "block_comment"}
FUNCTIONS = {"function_item", "function_declaration", "function_expression", "arrow_function",
             "method_definition", "generator_function_declaration", "generator_function",
             "closure_expression", "macro_definition"}
SCOPES = {"impl_item", "trait_item", "mod_item", "class_declaration", "class", "internal_module"}


def node_text(node, data):
    return data[node.start_byte:node.end_byte].decode("utf-8")


def target_after(attribute):
    target = attribute.next_named_sibling
    while target and target.type in COMMENTS | {"attribute_item"}:
        target = target.next_named_sibling
    return target


def declaration_name(node, data):
    name = node.child_by_field_name("name")
    if name:
        return node_text(name, data)
    if node.type == "impl_item":
        names = [node.child_by_field_name(field) for field in ("trait", "type")]
        return "impl " + " for ".join(re.sub(r"\s+", "", node_text(part, data)) for part in names if part)
    if node.parent and node.parent.type in {"variable_declarator", "let_declaration", "pair", "field_definition"}:
        name = node.parent.child_by_field_name("name") or node.parent.child_by_field_name("pattern")
        name = name or node.parent.child_by_field_name("key")
        if name:
            return node_text(name, data)
    return f"<{node.type}>"


def syntax_nodes(root, data):
    """Exclude only explicit test-only Rust items, never a name guessed to be a test."""
    nodes = []
    stack = [root]
    excluded = set()
    while stack:
        node = stack.pop()
        if node.id in excluded:
            continue
        if node.type == "attribute_item" and test_only(node_text(node, data)):
            target = target_after(node)
            if target:
                excluded.add(target.id)
                previous = node.prev_named_sibling
                while previous and previous.type in COMMENTS | {"attribute_item"}:
                    nodes = [item for item in nodes if not (previous.start_byte <= item.start_byte
                                                           and item.end_byte <= previous.end_byte)]
                    previous = previous.prev_named_sibling
                sibling = node.next_named_sibling
                while sibling and sibling.id != target.id:
                    excluded.add(sibling.id)
                    sibling = sibling.next_named_sibling
            continue
        nodes.append(node)
        if node.type not in COMMENTS:
            stack.extend(reversed(node.children))
    return nodes


def rust_allowances(nodes, data, identities):
    result = []
    for node in nodes:
        if node.type not in {"attribute_item", "inner_attribute_item"}:
            continue
        text = node_text(node, data)
        unquoted = re.sub(r'"(?:\\.|[^"\\])*"', '""', text)
        groups = re.findall(r"\b(?:allow|expect)\s*\(([^()]*)\)", unquoted)
        rules = set()
        for group in groups:
            if re.search(r"(?:^|,)\s*warnings\s*(?:,|$)", group):
                rules.update(RUST_LINTS)
            selected = set(re.findall(r"clippy\s*::\s*(\w+)", group))
            rules.update(selected & RUST_LINTS)
            if "pedantic" in selected:
                rules.add("too_many_lines")
            if selected & {"all", "complexity"}:
                rules.update({"too_many_arguments", "type_complexity"})
            if selected & {"all", "perf"}:
                rules.add("large_enum_variant")
        if not rules:
            continue
        target = target_after(node) if node.type == "attribute_item" else None
        scope = "item" if target and target.type in FUNCTIONS | {"struct_item", "enum_item", "type_item"} else "file"
        owner = identities.get(target.id, "<file>") if target else "<file>"
        reason_match = re.search(r'reason\s*=\s*"((?:\\.|[^"\\])*)"', text)
        previous = node.prev_named_sibling
        following = node.next_named_sibling
        comment = following if following and following.type in COMMENTS and following.start_point.row == node.end_point.row else previous
        reason = reason_match.group(1) if reason_match else ""
        if not reason and comment and comment.type in COMMENTS and abs(comment.end_point.row - node.start_point.row) <= 1:
            reason = node_text(comment, data).lstrip("/ *!").strip()
        for rule in sorted(rules):
            result.append(Allowance(owner, rule, scope, node.start_point.row + 1, reason))
    return result


def analyze_tree(data, extension, metric):
    tree = parse(data, extension, metric.path)
    nodes = syntax_nodes(tree.root_node, data)
    rows = set()
    comments = []
    source_lines = data.splitlines()
    for node in nodes:
        if node.type in COMMENTS:
            comments.append((node.start_point.row + 1, node_text(node, data)))
        elif node.child_count == 0:
            end = node.end_point.row + (1 if node.end_point.column else 0)
            rows.update(range(node.start_point.row + 1, end + 1))
    rows = {row for row in rows if source_lines[row - 1].strip()}
    metric.code_lines = len(rows)
    identities = {}
    seen = {}
    for node in nodes:
        if node.type not in FUNCTIONS | SCOPES | {"struct_item", "enum_item", "type_item"}:
            continue
        scope = []
        parent = node.parent
        while parent:
            if parent.type in FUNCTIONS | SCOPES:
                scope.append(declaration_name(parent, data))
            parent = parent.parent
        name = symbol_name([*reversed(scope), declaration_name(node, data)], seen)
        identities[node.id] = name
        if node.type in FUNCTIONS:
            start, end = node.start_point.row + 1, node.end_point.row + 1
            metric.functions.append(Function(name, start, end, line_span(start, end, rows)))
    if extension == ".rs":
        metric.allowances = rust_allowances(nodes, data, identities)
    else:
        metric.allowances = comment_allowances(comments, metric.functions, TS_LINTS, "typescript")
    for node in nodes:
        if node.type in {"if_expression", "if_statement", "match_arm", "switch_case"}:
            snippet = node_text(node, data)
            condition = node.child_by_field_name("condition") or node.child_by_field_name("value")
            hint = filename_hint(metric.path, node.start_point.row + 1, node_text(condition, data) if condition else "")
            if hint:
                metric.hints.append(hint)
            hint = branch_hint(metric.path, node.start_point.row + 1, snippet,
                               line_span(node.start_point.row + 1, node.end_point.row + 1, rows))
            if hint:
                metric.hints.append(hint)

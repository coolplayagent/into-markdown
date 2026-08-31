"""Recognize test-only cfg expressions without guessing other build features."""

import re


def test_only(attribute):
    if re.fullmatch(r"#\[\s*(?:\w+::)?test(?:\([^\]]*\))?\s*\]", attribute):
        return True
    match = re.fullmatch(r"#\[\s*cfg\s*\((.*)\)\s*\]", attribute, re.DOTALL)
    if not match:
        return False
    tokens = re.findall(r'\w+|"(?:\\.|[^"\\])*"|[(),=]', match.group(1))
    cursor = 0

    def expression():
        nonlocal cursor
        name = tokens[cursor]
        cursor += 1
        if cursor < len(tokens) and tokens[cursor] == "=":
            cursor += 2
            return None
        if cursor < len(tokens) and tokens[cursor] == "(":
            cursor += 1
            values = []
            while tokens[cursor] != ")":
                values.append(expression())
                if tokens[cursor] == ",":
                    cursor += 1
            cursor += 1
            if name == "all":
                return False if False in values else None if None in values else True
            if name == "any":
                return True if True in values else None if None in values else False
            if name == "not" and len(values) == 1:
                return None if values[0] is None else not values[0]
            return None
        return False if name == "test" else None

    try:
        result = expression()
        return cursor == len(tokens) and result is False
    except IndexError:
        return False  # Not a recognized cfg form; keep it in production metrics.

"""Pinned parser entry point and its documented contextual-keyword correction."""

import re
from functools import lru_cache

from .model import GateError


@lru_cache(maxsize=3)
def parser(extension):
    from tree_sitter import Language, Parser
    import tree_sitter_rust
    import tree_sitter_typescript
    languages = {".rs": tree_sitter_rust.language, ".ts": tree_sitter_typescript.language_typescript,
                 ".tsx": tree_sitter_typescript.language_tsx}
    return Parser(Language(languages[extension]()))


def errors(root):
    pending = [root]
    while pending:
        node = pending.pop()
        if node.type == "ERROR" or node.is_missing:
            yield node
        pending.extend(reversed(node.children))


def parse(data, extension, path):
    tree = parser(extension).parse(data)
    if extension == ".rs" and tree.root_node.has_error:
        # tree-sitter-rust 0.24.2 confuses the contextual `raw` identifier with
        # the raw-reference keyword in `raw @ (...)` patterns. Correct only
        # that error node, preserving bytes/lines; then require a clean parse.
        replacement = bytearray(data)
        changed = False
        for node in errors(tree.root_node):
            if (node.parent.type == "match_block"
                    and re.fullmatch(rb"raw\s*@", data[node.start_byte:node.end_byte])
                    and node.next_named_sibling and node.next_named_sibling.type == "match_arm"):
                replacement[node.start_byte:node.start_byte + 3] = b"r_w"
                changed = True
        if changed:
            tree = parser(extension).parse(bytes(replacement))
    if tree.root_node.has_error:
        node = next(errors(tree.root_node), tree.root_node)
        raise GateError(f"{path}:{node.start_point.row + 1}: {extension} parse failed ({node.type})")
    return tree

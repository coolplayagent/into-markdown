#!/usr/bin/env python3
"""Validate public Markdown against the built CLI and format catalog."""

from __future__ import annotations

import argparse, json, os, pathlib, re, shlex, subprocess, sys, tempfile, urllib.parse

MARKER = re.compile(r"<!--\s*(cli|format)-example:\s*([^>]+?)\s*-->\s*\n[^\n]*?`([^`\n]+)`", re.M)
LINK = re.compile(r"(?<!!)\[[^\]]*\]\(([^)]+)\)")
FENCE = re.compile(r"```[^\n]*\n(.*?)```", re.S)
STALE = ("当前工程仍是转换后端脚手架", "与未来 HTTP 服务共享", "转换和状态查询不会隐式调用这两个命令")
TOKENS = ("official.ocr.ppocrv6", "official.media.whisper", "Office 97", "into-markdown-skill.zip", "macOS ARM64", "Linux x86_64", "Linux ARM64", "Windows x86_64", "Windows ARM64")


class CheckError(RuntimeError): pass


def runfile(*logical_paths):
    manifest = os.environ.get("RUNFILES_MANIFEST_FILE")
    if not manifest: return None
    manifest_path = pathlib.Path(manifest)
    if not manifest_path.is_file() or manifest_path.stat().st_size > 64 * 1024 * 1024: return None
    wanted = set(logical_paths)
    with manifest_path.open(encoding="utf-8") as source:
        for line in source:
            logical, separator, physical = line.rstrip("\r\n").partition(" ")
            if separator and logical in wanted:
                candidate = pathlib.Path(physical)
                if candidate.is_file(): return candidate.resolve()
    return None


def invoke(binary, args, cwd, env, stdin=None):
    result = subprocess.run([str(binary), *args], cwd=cwd, env=env, input=stdin, capture_output=True, text=True, encoding="utf-8")
    if result.returncode:
        raise CheckError(f"into-md {' '.join(args)} failed ({result.returncode})\n{result.stderr}")
    return result.stdout


def children(text):
    lines = text.splitlines()
    if "Commands:" not in lines: return []
    found = []
    for line in lines[lines.index("Commands:") + 1:]:
        if not line.strip(): break
        match = re.match(r"  ([a-z0-9][a-z0-9-]*)\s", line)
        if match and match.group(1) != "help": found.append(match.group(1))
    return found


def command_tree(binary, root, env):
    found, pending = set(), [()]
    while pending:
        parent = pending.pop()
        for child in children(invoke(binary, [*parent, "--help"], root, env)):
            path = (*parent, child); label = " ".join(path)
            if label not in found: found.add(label); pending.append(path)
    return found


def examples(path):
    groups = {"cli": {}, "format": {}}
    for kind, raw, command in MARKER.findall(path.read_text(encoding="utf-8")):
        label = " ".join(raw.split())
        if label in groups[kind]: raise CheckError(f"duplicate {kind} example {label} in {path.name}")
        groups[kind][label] = command.strip()
    return groups["cli"], groups["format"]


def syntax(binary, command, cwd, env):
    tokens = shlex.split(command)
    if not tokens or tokens[0] != "into-md": raise CheckError(f"invalid example: {command}")
    if any(x in {"|", ">", "<", "&&", ";"} for x in tokens): raise CheckError(f"nonportable example: {command}")
    args = tokens[1:]
    if "--help" not in args and "-h" not in args: args.append("--help")
    invoke(binary, args, cwd, env)
    return tokens


def check_examples(binary, root, env):
    zh, zf = examples(root / "docs/cli-examples.md"); en, ef = examples(root / "docs/cli-examples.en.md")
    if zh != en or zf != ef: raise CheckError("Chinese and English executable examples differ")
    expected = {"convert", *command_tree(binary, root, env)}
    if set(zh) != expected: raise CheckError(f"CLI coverage drifted; missing={sorted(expected-set(zh))}, extra={sorted(set(zh)-expected)}")
    top = {x.split()[0] for x in expected if x != "convert"}
    for label, command in sorted(zh.items()):
        tokens = syntax(binary, command, root, env)
        if label == "convert":
            if len(tokens) < 2 or tokens[1] in top: raise CheckError("conversion example has no input")
        elif tokens[1:1+len(label.split())] != label.split(): raise CheckError(f"wrong command example: {label}")
    catalog = json.loads(invoke(binary, ["formats", "--json", "--no-config"], root, env))
    available = {x["format"]: x for x in catalog if x["status"] == "available"}
    if set(zf) != set(available): raise CheckError(f"format coverage drifted; missing={sorted(set(available)-set(zf))}, extra={sorted(set(zf)-set(available))}")
    with tempfile.TemporaryDirectory(prefix="into-md-doc-formats-") as name:
        temp = pathlib.Path(name)
        for format_id, descriptor in sorted(available.items()):
            tokens = syntax(binary, zf[format_id], root, env)
            if "--format" not in tokens or tokens[tokens.index("--format")+1] != format_id: raise CheckError(f"wrong format example: {format_id}")
            source = temp / f"source.{descriptor['extensions'][0]}"; source.write_bytes(b"")
            invoke(binary, [str(source), "--format", format_id, "-o", str(temp/f"{format_id}.md"), "--conflict", "error", "--dry-run", "--no-config"], temp, env)


def check_real(binary, env):
    with tempfile.TemporaryDirectory(prefix="into-md-doc-smoke-") as name:
        temp = pathlib.Path(name); source = temp/"source.txt"; output = temp/"source.md"
        source.write_text("Alpha documentation example\n", encoding="utf-8")
        invoke(binary, [str(source), "-o", str(output), "--conflict", "error", "--no-config"], temp, env)
        if not output.read_text(encoding="utf-8").strip(): raise CheckError("empty file conversion")
        stdin_output = temp/"stdin.md"
        invoke(binary, ["-", "--format", "txt", "--asset-mode", "embed", "-o", str(stdin_output), "--conflict", "error", "--no-config"], temp, env, "stdin example\n")
        if not stdin_output.read_text(encoding="utf-8").strip(): raise CheckError("empty stdin conversion")
        for args in (["version", "--json", "--no-config"], ["capabilities", "list", "--json", "--no-config"], ["doctor", "--json", "--no-config"]): json.loads(invoke(binary, args, temp, env))


def check_markdown(root):
    paths = [root/"README.md", root/"README.en.md", root/"CONTRIBUTING.md", root/"CONTRIBUTING.en.md", *sorted((root/"docs").rglob("*.md"))]
    if any(not x.is_file() for x in paths): raise CheckError("required bilingual documentation is missing")
    for source in paths:
        text = source.read_text(encoding="utf-8")
        for phrase in STALE:
            if phrase in text: raise CheckError(f"stale phrase in {source.relative_to(root)}: {phrase}")
        for raw in LINK.findall(text):
            target = raw.split(maxsplit=1)[0].strip("<>")
            if not target or target.startswith(("#", "http://", "https://", "mailto:")): continue
            relative = urllib.parse.unquote(target.split("#", 1)[0])
            if relative and not (source.parent/relative).resolve().exists(): raise CheckError(f"broken link: {source.relative_to(root)} -> {relative}")
    zh = (root/"README.md").read_text(encoding="utf-8"); en = (root/"README.en.md").read_text(encoding="utf-8")
    for token in TOKENS:
        if token not in zh or token not in en: raise CheckError(f"README contract drifted: {token}")
    if FENCE.findall(zh) != FENCE.findall(en): raise CheckError("README command blocks differ")
    for left, right in (("docs/user-guide.md","docs/user-guide.en.md"),("docs/cli-examples.md","docs/cli-examples.en.md"),("docs/plugin-development.md","docs/plugin-development.en.md"),("CONTRIBUTING.md","CONTRIBUTING.en.md")):
        if left not in zh or right not in en: raise CheckError(f"README missing bilingual links: {left}, {right}")


def main():
    parser = argparse.ArgumentParser(); parser.add_argument("--into-md", required=True, type=pathlib.Path); parser.add_argument("--repository", type=pathlib.Path); args = parser.parse_args()
    if args.repository: root = args.repository.resolve()
    elif os.environ.get("TEST_SRCDIR"):
        readme = runfile("_main/README.md", "into_markdown/README.md")
        root = readme.parent if readme else (pathlib.Path(os.environ["TEST_SRCDIR"])/os.environ["TEST_WORKSPACE"]).resolve()
    else: root = pathlib.Path(subprocess.run(["git","rev-parse","--show-toplevel"], check=True, capture_output=True, text=True).stdout.strip()).resolve()
    binary = next((x.resolve() for x in (args.into_md, root/args.into_md, pathlib.Path.cwd()/args.into_md) if x.is_file()), None)
    if binary is None:
        name = "into-md.exe" if os.name == "nt" else "into-md"
        binary = runfile(f"_main/apps/cli/{name}", f"into_markdown/apps/cli/{name}")
    if binary is None: raise CheckError(f"into-md unavailable: {args.into_md}")
    with tempfile.TemporaryDirectory(prefix="into-md-doc-user-") as user:
        env = dict(os.environ); env["INTO_MARKDOWN_USER_DATA_HOME"] = user; env.pop("INTO_MARKDOWN_CONFIG", None)
        check_markdown(root); check_examples(binary, root, env); check_real(binary, env)
    print("documentation contract passed")


if __name__ == "__main__":
    try: main()
    except (CheckError, json.JSONDecodeError, ValueError) as error:
        print(f"docs-check: {error}", file=sys.stderr); raise SystemExit(1)

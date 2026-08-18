import { Fragment, type ReactNode } from "react";

const MAX_TREE_DEPTH = 12;
const MAX_TREE_NODES = 1_000;
const MAX_MARKDOWN_BLOCKS = 2_000;

/** Markdown preview that never creates HTML, links, images, or resource-bearing DOM nodes. */
export function SafeMarkdownPreview({ source }: { source: string }) {
  const allLines = source.replaceAll("\r\n", "\n").split("\n");
  const lines = allLines.slice(0, MAX_MARKDOWN_BLOCKS);
  const output: ReactNode[] = [];
  let code: string[] | null = null;
  for (const [index, line] of lines.entries()) {
    if (line.startsWith("```")) {
      if (code) { output.push(<pre key={`code-${index}`}><code>{code.join("\n")}</code></pre>); code = null; }
      else code = [];
      continue;
    }
    if (code) { code.push(line); continue; }
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      const level = heading[1]!.length;
      output.push(<div className={`preview-heading h${level}`} role="heading" aria-level={level} key={index}>{heading[2]}</div>);
    } else if (/^\s*[-*+]\s+/.test(line)) {
      output.push(<div className="preview-list-item" key={index}>• {line.replace(/^\s*[-*+]\s+/, "")}</div>);
    } else if (line.trim()) {
      output.push(<p key={index}>{line}</p>);
    }
  }
  if (code) output.push(<pre key="code-final"><code>{code.join("\n")}</code></pre>);
  if (lines.length < allLines.length) output.push(<p className="tree-limit" role="status" key="limit">… preview block limit reached</p>);
  return <div className="markdown-preview">{output}</div>;
}

export function JsonTree({ value }: { value: unknown }) {
  let nodes = 0;
  const render = (item: unknown, depth: number, key: string): ReactNode => {
    nodes += 1;
    if (nodes > MAX_TREE_NODES) return <span className="tree-limit">… node limit reached</span>;
    if (depth > MAX_TREE_DEPTH) return <span className="tree-limit">… depth limit reached</span>;
    if (item === null || typeof item !== "object") return <span className="tree-value">{JSON.stringify(item)}</span>;
    const entries = Array.isArray(item) ? item.map((entry, index) => [String(index), entry] as const) : Object.entries(item as Record<string, unknown>);
    const visible = entries.slice(0, 200);
    return <details open={depth < 2}><summary>{Array.isArray(item) ? `Array(${entries.length})` : `Object(${entries.length})`}</summary><ul>{visible.map(([name, child]) => <li key={`${key}-${name}`}><span className="tree-key">{name}</span>: {render(child, depth + 1, `${key}-${name}`)}</li>)}{visible.length < entries.length && <li className="tree-limit">… {entries.length - visible.length} more entries</li>}</ul></details>;
  };
  return <div className="json-tree">{render(value, 0, "root")}</div>;
}

export function JsonPreview({ source, truncated }: { source: string; truncated: boolean }) {
  if (truncated) return <pre className="raw-preview"><code>{source}</code></pre>;
  try { return <JsonTree value={JSON.parse(source)} />; }
  catch { return <Fragment><p role="alert">Invalid JSON preview.</p><pre className="raw-preview"><code>{source}</code></pre></Fragment>; }
}

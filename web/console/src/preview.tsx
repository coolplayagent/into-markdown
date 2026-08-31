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
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index]!;
    if (line.startsWith("```")) {
      if (code) { output.push(<pre key={`code-${index}`}><code>{code.join("\n")}</code></pre>); code = null; }
      else code = [];
      continue;
    }
    if (code) { code.push(line); continue; }
    if (isSourceAnchor(line)) continue;
    const delimiter = lines[index + 1];
    if (line.includes("|") && delimiter && isTableDelimiter(delimiter)) {
      const headers = tableCells(line);
      const rows: string[][] = [];
      index += 2;
      while (index < lines.length && lines[index]!.includes("|") && lines[index]!.trim()) {
        rows.push(tableCells(lines[index]!));
        index += 1;
      }
      index -= 1;
      output.push(<div className="preview-table-scroll" key={`table-${index}`}><table><thead><tr>{headers.map((cell, cellIndex) => <th key={cellIndex} scope="col">{renderInline(cell)}</th>)}</tr></thead><tbody>{rows.map((row, rowIndex) => <tr key={rowIndex}>{headers.map((_, cellIndex) => <td key={cellIndex}>{renderInline(row[cellIndex] ?? "")}</td>)}</tr>)}</tbody></table></div>);
      continue;
    }
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    const listDepth = Math.min(12, Math.floor((line.match(/^[ \t]*/)?.[0] ?? "").replaceAll("\t", "    ").length / 4));
    if (heading) {
      const level = heading[1]!.length;
      output.push(<div className={`preview-heading h${level}`} role="heading" aria-level={level} key={index}>{renderInline(heading[2]!)}</div>);
    } else if (/^\s*[-*+]\s+/.test(line)) {
      output.push(<div className={`preview-list-item list-depth-${listDepth}`} key={index}><span className="preview-list-marker" aria-hidden="true">•</span><span>{renderInline(line.replace(/^\s*[-*+]\s+/, ""))}</span></div>);
    } else if (/^\s*\d{1,4}[.)]\s+/.test(line)) {
      const ordered = /^\s*(\d{1,4})[.)]\s+(.*)$/.exec(line)!;
      output.push(<div className={`preview-list-item ordered list-depth-${listDepth}`} key={index}><span className="preview-list-marker" aria-hidden="true">{ordered[1]}.</span><span>{renderInline(ordered[2]!)}</span></div>);
    } else if (line.trim()) {
      output.push(<p key={index}>{renderInline(line)}</p>);
    }
  }
  if (code) output.push(<pre key="code-final"><code>{code.join("\n")}</code></pre>);
  if (lines.length < allLines.length) output.push(<p className="tree-limit" role="status" key="limit">… preview block limit reached</p>);
  return <div className="markdown-preview">{output}</div>;
}

function isSourceAnchor(line: string): boolean {
  return /^<a id="[A-Za-z0-9._:-]{1,128}"><\/a>$/.test(line.trim());
}

function renderInline(source: string): ReactNode {
  source = stripOcrBoundaryMarkers(source);
  const nodes: ReactNode[] = [];
  const pattern = /(<strong>[^<>\n]{1,512}<\/strong>|<em>[^<>\n]{0,512}<\/em>|\*\*[^*\n]{1,512}\*\*|`[^`\n]{1,512}`)/g;
  let cursor = 0;
  for (const match of source.matchAll(pattern)) {
    const offset = match.index ?? 0;
    if (offset > cursor) nodes.push(unescapeInline(source.slice(cursor, offset)));
    const value = match[0];
    if (value.startsWith("<strong>")) nodes.push(<strong key={`${offset}-strong`}>{unescapeInline(value.slice(8, -9))}</strong>);
    else if (value.startsWith("<em>")) nodes.push(<em key={`${offset}-em`}>{unescapeInline(value.slice(4, -5))}</em>);
    else if (value.startsWith("**")) nodes.push(<strong key={`${offset}-bold`}>{unescapeInline(value.slice(2, -2))}</strong>);
    else nodes.push(<code key={`${offset}-code`}>{unescapeInline(value.slice(1, -1))}</code>);
    cursor = offset + value.length;
  }
  if (cursor < source.length) nodes.push(unescapeInline(source.slice(cursor)));
  return nodes.length === 1 ? nodes[0] : nodes;
}

function stripOcrBoundaryMarkers(source: string): string {
  return source
    .replace(/<em>\\?\[<\/em><em>(?:Image OCR|End OCR)<\/em><em>\\?\]<\/em>/g, "")
    .replace(/\\?\*?\\?\[Image OCR\\?\]\s*/g, "")
    .replace(/\s*\\?\[End OCR\\?\]\\?\*?/g, "")
    .trim();
}

function unescapeInline(source: string): string {
  return source.replace(/\\([\\`*{}[\]()#+.!_|>~-])/g, "$1");
}

function tableCells(line: string): string[] {
  const trimmed = line.trim().replace(/^\|/, "").replace(/\|$/, "");
  return trimmed.split(/(?<!\\)\|/).map((cell) => cell.trim().replaceAll("\\|", "|"));
}

function isTableDelimiter(line: string): boolean {
  const cells = tableCells(line);
  return cells.length > 0 && cells.every((cell) => /^:?-{3,}:?$/.test(cell));
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

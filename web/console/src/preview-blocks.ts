/** Bounded block boundaries shared by paragraph and list continuation collection. */
export function tableCells(line: string): string[] {
  const trimmed = line.trim().replace(/^\|/, "").replace(/\|$/, "");
  return trimmed.split(/(?<!\\)\|/).map((cell) => cell.trim().replaceAll("\\|", "|"));
}

export function isTableDelimiter(line: string): boolean {
  const cells = tableCells(line);
  return cells.length > 0 && cells.every((cell) => /^:?-{3,}:?$/.test(cell));
}

function startsBlock(lines: string[], index: number): boolean {
  const line = lines[index]!;
  return !line.trim() || /^\s*(?:#{1,6}\s|[-*+]\s|\d{1,9}[.)]\s|```|~~~|>|<!-- -->)/.test(line)
    || /^<a id="[A-Za-z0-9._:-]{1,128}"><\/a>$/.test(line.trim())
    || (line.includes("|") && !!lines[index + 1] && isTableDelimiter(lines[index + 1]!));
}

export function collectParagraph(lines: string[], start: number, first: string, list = false): { text: string; end: number } {
  const parts = [first];
  let end = start;
  while (end + 1 < lines.length && !startsBlock(lines, end + 1)) {
    const line = lines[end + 1]!;
    // The renderer indents list continuations; unindented content stays separate.
    if (list && !/^[ \t]+/.test(line)) break;
    parts.push(line.trimStart());
    end += 1;
  }
  return { text: parts.join("\n"), end };
}

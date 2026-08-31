import { createElement, type ReactNode } from "react";

const tags: Record<string, string> = { "**": "strong", "*": "em", "~~": "del" };
const punctuation = /[\p{P}\p{S}]/u;
const whitespace = /\s/u;

function flanks(source: string, start: number, size: number): [boolean, boolean] {
  const previousUnit = source.charCodeAt(start - 1);
  const before = start === 0 ? undefined : source.slice(start - (previousUnit >= 0xdc00 && previousUnit <= 0xdfff ? 2 : 1), start);
  const nextPoint = source.codePointAt(start + size);
  const after = nextPoint === undefined ? undefined : String.fromCodePoint(nextPoint);
  const beforeSpace = before === undefined || whitespace.test(before);
  const afterSpace = after === undefined || whitespace.test(after);
  const beforePunctuation = before !== undefined && punctuation.test(before);
  const afterPunctuation = after !== undefined && punctuation.test(after);
  return [!afterSpace && (!afterPunctuation || beforeSpace || beforePunctuation),
    !beforeSpace && (!beforePunctuation || afterSpace || afterPunctuation)];
}

type Budget = { left: number; work: number };

function closing(source: string, marker: string, start: number, native: boolean, budget: Budget): number {
  const nested: number[] = [];
  const htmlOpen = marker.startsWith("</") ? marker.replace("</", "<") : null;
  let htmlDepth = 0;
  for (let index = start; index < source.length && budget.work > 0; index += 1) {
    budget.work -= 1;
    if (!marker.startsWith("`") && source[index] === "\\") { index += 1; continue; }
    if (native && source[index] === "`") {
      const ticks = /^`+/.exec(source.slice(index))![0];
      const end = closing(source, ticks, index + ticks.length, false, budget);
      if (end >= 0) index = end + ticks.length - 1;
      continue;
    }
    if (marker.startsWith("*") && source[index] === "*") {
      const size = /^\*+/.exec(source.slice(index))![0].length;
      const [opens, closes] = flanks(source, index, size);
      let remaining = size;
      if (closes) {
        while (nested.length && remaining >= nested[nested.length - 1]!) remaining -= nested.pop()!;
        if (!nested.length && remaining >= marker.length) return index + size - remaining;
      }
      if (opens && remaining) nested.push(remaining);
      index += size - 1;
      continue;
    }
    if (htmlOpen && source.startsWith(htmlOpen, index)) { htmlDepth += 1; index += htmlOpen.length - 1; continue; }
    if (!source.startsWith(marker, index)) continue;
    if (htmlOpen && htmlDepth > 0) { htmlDepth -= 1; index += marker.length - 1; continue; }
    if (marker.startsWith("`") && (source[index - 1] === "`" || source[index + marker.length] === "`")) continue;
    if (!native || flanks(source, index, marker.length)[1]) return index;
  }
  return -1;
}

function decodeText(source: string): string {
  return source.replace(/&(?:amp|lt|gt|quot|apos|#39|#(?:[0-9]+|x[0-9a-f]+));/gi, (entity) => {
    const named: Record<string, string> = { "&amp;": "&", "&lt;": "<", "&gt;": ">", "&quot;": '"', "&apos;": "'", "&#39;": "'" };
    if (named[entity.toLowerCase()]) return named[entity.toLowerCase()]!;
    const number = entity.slice(2, -1);
    const value = Number.parseInt(number.replace(/^x/i, ""), /^x/i.test(number) ? 16 : 10);
    return value > 0 && value <= 0x10ffff && !(value >= 0xd800 && value <= 0xdfff) ? String.fromCodePoint(value) : "\ufffd";
  });
}

/** Render a bounded inline subset exclusively through inert React elements. */
export function renderInline(source: string): ReactNode {
  return parse(source, 0, { left: 1_000, work: Math.min(source.length * 16, 2_000_000) });
}

function parse(source: string, depth: number, budget: Budget): ReactNode {
  if (depth >= 12 || budget.left <= 0) return decodeText(source);
  const nodes: ReactNode[] = [];
  let plain = "";
  const flush = () => { if (plain) { nodes.push(decodeText(plain)); plain = ""; } };
  for (let index = 0; index < source.length;) {
    if (budget.left <= 0 || budget.work <= 0) { plain += source.slice(index); break; }
    if (source[index] === "\\" && /[!"#$%&'()*+,\-./:;<=>?@[\\\]^_`{|}~]/.test(source[index + 1] ?? "")) {
      plain += source[index + 1]; index += 2; continue;
    }
    if (source.startsWith("<br>", index)) {
      flush(); nodes.push(createElement("br", { key: index })); budget.left -= 1; index += 4; continue;
    }
    budget.work -= 1;
    const rest = source.slice(index);
    const html = /^<(strong|em|del|u|sub|sup|code)>/.exec(rest);
    const code = /^`+/.exec(rest);
    const native = /^(\*\*\*|\*\*|~~|\*)/.exec(rest);
    const marker = html?.[0] ?? code?.[0] ?? native?.[0];
    if (!marker || (native && !html && !code && !flanks(source, index, marker.length)[0])) {
      plain += source[index]; index += 1; continue;
    }
    const endMarker = html ? `</${html[1]}>` : marker;
    const end = closing(source, endMarker, index + marker.length, !!native && !html && !code, budget);
    if (end < 0) { plain += marker; index += marker.length; continue; }
    flush(); budget.left -= 1;
    let body = source.slice(index + marker.length, end);
    if (code) {
      if (body.startsWith(" ") && body.endsWith(" ") && body.trim()) body = body.slice(1, -1);
      nodes.push(createElement("code", { key: index }, body));
    } else if (html?.[1] === "code") {
      nodes.push(createElement("code", { key: index }, decodeText(body)));
    } else {
      const content = parse(body, depth + 1, budget);
      nodes.push(marker === "***"
        ? createElement("strong", { key: index }, createElement("em", null, content))
        : createElement(html?.[1] ?? tags[marker]!, { key: index }, content));
    }
    index = end + endMarker.length;
  }
  flush();
  return nodes;
}

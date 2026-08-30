# DOCX converter module boundary for issue #270

Issue #270 consumes the structured summary and empty-result policy delivered by #273 and #268.
The integration order is **#273 → #268 → #270**. This change remains converter-internal and does
not alter Core, Engine, result DTOs, or CLI reporting.

## Required module layout

`crates/converters/src/docx.rs` remains the small converter registration and orchestration surface.
Implementation moves under `crates/converters/src/docx/` with these responsibilities:

| Module | Owns | Must not own |
| --- | --- | --- |
| `package.rs` | raw ZIP admission, canonical part names, duplicate/encryption/compression checks, bounded member reads, macro/active-part exclusion | XML semantics or result classification |
| `content_types.rs` | authoritative defaults/overrides and media-type lookup | filename-driven format decisions |
| `relationships.rs` | QName-checked relationship parsing, owner-relative target resolution, internal/external policy, cycle identity | downloading or converter dispatch |
| `xml.rs` | namespace/QName resolution, parent-QName frames, DTD/entity rejection, event/depth accounting | Word block semantics |
| `styles_numbering.rs` | styles, heading inheritance, abstract numbering, overrides and labels | paragraph emission |
| `fields.rs` | complex/simple fields, instruction/result state, safe hyperlink target policy | relationship lookup or tables |
| `tables.rs` | nested tables, grid spans, horizontal/vertical merge occupancy, row/cell limits | media extraction |
| `media.rs` | image relationship binding, type/signature/dimension validation, deduplication, placeholders | altChunk parsing |
| `alt_chunk.rs` | internal HTML/XHTML/MHT/RTF dispatch, independent nested budgets, output remapping/merge, placeholders | external access or package admission |
| `word.rs` | document-order event dispatch across body, headers, footers, notes, comments and content controls | ZIP or MIME parsing |
| `tests.rs`, `fixture_tests.rs` | package, QName, order, tables, fields, media, altChunk, strict, resource and corpus contracts | production helpers |

Every semantic dispatch key is `(element QName, semantic parent QName, profile)`. Local names,
prefix spelling, filenames, and corpus strings are never authority. Unknown extensions affect only
their subtree unless they violate a package or resource invariant.

## State and data flow

1. Package admission produces immutable part metadata and bounded read handles.
2. Content types and relationships are parsed before any semantic part is converted.
3. The Word event reader emits ordered operations into a document builder. Paragraphs, tables,
   notes, headers/footers, links, images, and altChunks share one monotonic source-order sequence.
4. Nested payloads reuse the request's existing options and execution context. HTML, MIME and RTF
   parsers therefore consume the same input, memory, node, inline, asset and deadline authorities;
   a child cannot create a fresh root budget or temporary-space lease.
5. Nested node and asset IDs are remapped into the DOCX document scope. Equal payloads may share
   stored bytes, but repeated source occurrences and repeated visible text remain in order.
6. The converter returns content plus scoped diagnostics and `SourceContentEvidence`. The shared
   #268 terminal policy remains the only authority for empty/degraded/failed outcomes.

## Migration constraints

- No DOCX production file may grow beyond 900 lines; orchestration stays below 350 lines.
- New functions accept focused context structs instead of adding argument-count suppressions.
- No `too_many_lines` or `too_many_arguments` suppression is added; the pre-existing suppression
  count remains unchanged while the former 5,000-line source is divided by responsibility.
- Package/security and relationship tests move first, then XML/QName, styles/numbering, fields,
  tables, media, and altChunk. Each move must preserve the full converter test suite before the
  next responsibility moves.
- The public converter ID, priority, supported format, and output ordering remain stable.

## Shared terminal-policy integration

#273 and #268 are present in the branch base. #270 maps converter evidence to their shared
terminal contract:

- recovered visible content: ordinary converter output, with warnings only for actual omission;
- unsupported or external wrapper: non-empty local placeholder plus
  `office.relationshipOmitted` / `word.unsupportedWrapperOmitted`;
- verified truly empty source: shared empty-source evidence;
- unverified empty output: stable `emptyContent` failure rather than successful degradation.

This sequencing prevents a DOCX-only result convention from diverging from the product-wide
contract and keeps classification evidence independent from conversion outcomes.

# Fixture corpus authority

`manifest.json` is the machine-audited authority for the checked-in corpus under `small/`.
Every declared file records its size, SHA-256, media type, scenario, license, provenance, and
expected conversion contract. The license audit rejects unknown schema fields, duplicate or
non-ASCII/escaping paths, symlinks, size/hash drift, and files
that are present on only one side of the manifest/filesystem relationship. Fixture paths use a
lowercase ASCII safe set, so their identity is portable without relying on platform Unicode
case-folding behavior.

RTF samples are repository-authored ASCII source files produced directly by `generate.py`; they do
not contain copied Office documents. Their English/Chinese text, corruption, exact depth boundary,
and inert object/local-file field scenarios are licensed with the rest of the corpus under
Apache-2.0 and bound to byte and semantic hashes in the manifest. `.gitattributes` classifies RTF
as binary so checkout newline conversion cannot change those authority bytes on Windows.

`semantic-layout-quality-authority.json` is the cross-format layout gate. It binds real converter
fixtures to canonical Document IR and GFM SHA-256 values, declares the only allowed coordinate
tolerance (`0.01` source units) and its negative boundary test, records normal/complex/misordered/
corrupt/resource-boundary coverage for every core family, and hash-binds the separate PDF, OCR and
legacy Office native authorities. Run the complete explicit gate with
`bazel test //crates/converters:semantic_layout_quality_gate`; ordinary wildcard builds remain
offline and do not resolve native runtimes.

The corpus covers every format marked `available` by the product format registry. The converter
test reads the same manifest, compares that registry dynamically, and runs each non-OCR fixture
through its real converter and Markdown renderer. A limit fixture records an exact public
`ConversionOptions` field, the failing value, the adjacent passing value, the expected error limit,
and the passing Markdown hash.

The workbook slice contains separately checked-in XLSX, XLSM, and XLSB normal, corrupt, and
exact-adjacent limit packages. `generate.py` writes the BIFF12 record streams directly from
repository-owned values, so the XLSB corpus is reproducible without Excel, LibreOffice, or an
opaque upstream workbook. Across the slice, the normal cases bind 1900/1904 dates, scalar and
formula-cache semantics, inert macro parts, and repeated image anchors; corrupt and limit cases
bind duplicate physical-sheet authority, truncated OPC/BIFF12 structure, and row-budget ±1.

PDF is the one native-runtime format. Most text, mixed, scanned, encrypted, damaged,
over-page-limit, link, and four-rotation samples remain deterministic in-memory Rust fixtures for
the explicit pinned-PDFium smoke. Twelve repository-generated Apache-2.0 layout PDFs are also bound
to this manifest and `pdf-layout-quality-authority.json`: they exercise multiple columns, rotated
text, headings, lists, and tables through the production converter. Ordinary Cargo/Bazel tests only
audit those bytes and remain offline; the explicit PDF layout quality target maps the pinned PDFium
runtime and checks exact semantic goldens plus precision and recall thresholds.

Outlook MSG fixtures are produced entirely by `fixtures/generate.py --msg-only`. The deterministic
writer creates CFB directory, FAT, miniFAT and MAPI property streams from repository-authored names,
addresses, bodies and attachment bytes; it does not use Outlook, copy mail, or download a template.
The corpus covers plain, HTML, bounded LZFu/RTF conversion, canonical audited CID binding, embedded attachment provenance,
truncation, a cyclic FAT and the exact input-byte boundary. Every generated MSG remains Apache-2.0
repository content and carries the same generator/hash/license authority as the other small fixtures.

## Storage and network boundary

Small fixtures are Apache-2.0 repository-generated files checked into `small/`; ordinary Cargo and
Bazel tests are offline. Large generator/runtime inputs are in `large_artifacts`, the third-party
inventory, and the dedicated `fixtures/downloads.json`. The `//fixtures:download_fixture` target is
tagged `manual`, is not a dependency of the normal graph, and is excluded from release payloads.
It rejects redirects, enforces the declared host and streamed byte ceiling, and verifies exact size
and SHA-256 before atomically installing `<output>/<repository>/<downloaded_file_path>`. For example:

```sh
bazel run //fixtures:download_fixture -- \
  --manifest "$PWD/fixtures/downloads.json" \
  --artifact noto-sans-cjk-sc-regular-generator-font \
  --output-directory /verified/fixture-inputs
```

## OCR goldens

OCR PNGs use repository-authored, post-model-release NFC phrases rather than an external data set.
Simplified Chinese, Traditional Chinese, English, and mixed-script groups each contain three
independent lines. `manifest.json` specifies the font, grayscale canvas, coordinates, colors,
font size, Pillow/FreeType versions, PNG compression, line ending, character counts, CER
normalization, punctuation policy, and per-group thresholds. Copyright in the phrases and generated
PNGs belongs to the into-markdown contributors under Apache-2.0.

The generator input is Noto Sans CJK SC Regular from the fixed upstream commit in the manifest.
That font is OFL-1.1; the repository records the exact font-file SHA-256 and the upstream license.
The checked-in PNG is authoritative. Byte reproducibility is claimed only for the recorded
environment: Python 3.13, Pillow 11.3.0, FreeType 2.13.3, locale-independent grayscale rendering,
no DPI metadata, `ImageFont.Layout.BASIC`, and PNG compression level 9. Run the following in that
environment after obtaining the font through the controlled manual downloader:

```sh
python3 fixtures/generate.py --font /verified/NotoSansCJKsc-Regular.otf --verify
```

The command validates the font hash, generates into a temporary directory, and compares every byte
with the checked-in corpus. It does not modify the repository in verification mode. Regeneration is
the same command without `--verify`; review the resulting manifest and all binary changes.

The PresentationML subset is pure OPC/XML and has no font or third-party generator dependency. Its
checked-in matrix includes two distinct layouts, multilingual text, master text styles, speaker
notes, a broken slide relationship, the adjacent input-size boundary, and real PPTX/PPTM/PPSX/
PPSM/POTX main content types. Macro-enabled fixtures contain repository-authored inert VBA bytes;
the expected conversion proves those parts are isolated through OPC metadata and never opened. It
can be regenerated independently while preserving the authoritative OCR records:

```sh
python3 fixtures/generate.py --presentation-only
```

The PP-OCRv6 tiny recognizer archive is only a fixed, manual dependency authority for the OCR
quality consumer. This corpus neither embeds the model nor implements recognition or CER execution.

# Fixture corpus authority

`manifest.json` is the machine-audited authority for the checked-in corpus under `small/`.
Every declared file records its size, SHA-256, media type, scenario, license, provenance, and
expected conversion contract. The license audit rejects unknown schema fields, duplicate or
non-ASCII/escaping paths, symlinks, size/hash drift, and files
that are present on only one side of the manifest/filesystem relationship. Fixture paths use a
lowercase ASCII safe set, so their identity is portable without relying on platform Unicode
case-folding behavior.

The corpus covers every format marked `available` by the product format registry. The converter
test reads the same manifest, compares that registry dynamically, and runs each non-OCR fixture
through its real converter and Markdown renderer. A limit fixture records an exact public
`ConversionOptions` field, the failing value, the adjacent passing value, the expected error limit,
and the passing Markdown hash.

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

The PP-OCRv6 tiny recognizer archive is only a fixed, manual dependency authority for the OCR
quality consumer. This corpus neither embeds the model nor implements recognition or CER execution.

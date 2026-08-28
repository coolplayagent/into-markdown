# CLI workflows

Use the host shell's normal argument quoting. Keep paths explicit, especially when they contain spaces. The examples use POSIX syntax; translate only shell syntax on Windows, not `into-md` option names.

## Preflight and artifact choice

Confirm the platform-selected bundled executable before touching outputs. In the examples, `BUNDLED_INTO_MD` means its absolute path; never resolve it through `PATH`:

```sh
"$BUNDLED_INTO_MD" version --json
```

Use these output forms:

- Markdown: ordinary human-readable result, with `-o <OUTPUT.md>`.
- Bundle: portable ZIP containing Markdown, IR, diagnostics, provenance, and assets, with `--emit bundle` and a `.mdpkg.zip` output.
- Result JSON: machine-readable Markdown, IR, assets, diagnostics, and provenance, with `--emit result-json`.
- IR JSON: structured document only, with `--emit ir-json`.

## Local files and standard input

Convert one file without overwriting an existing destination:

```sh
"$BUNDLED_INTO_MD" "/absolute/input.docx" \
  -o "/absolute/output.md" \
  --conflict error \
  --log-format json
```

When stdout is the requested result, keep resources self-contained:

```sh
"$BUNDLED_INTO_MD" "/absolute/input.docx" --asset-mode embed --log-format json
```

For stdin, supply an authoritative format when detection cannot derive it from a filename:

```sh
printf '%s\n' 'source text' | \
  "$BUNDLED_INTO_MD" - --format text --asset-mode embed \
  -o "/absolute/stdin.md" --conflict error --log-format json
```

Do not combine stdin with other inputs.

## Batch and directories

Multiple inputs or a directory require `--output-dir`. Preflight without writing, then run with a report:

```sh
"$BUNDLED_INTO_MD" "/absolute/documents" \
  --recursive \
  --output-dir "/absolute/markdown" \
  --conflict error \
  --dry-run \
  --log-format json

"$BUNDLED_INTO_MD" "/absolute/documents" \
  --recursive \
  --output-dir "/absolute/markdown" \
  --conflict error \
  --report "/absolute/conversion-report.json" \
  --log-format json
```

Afterward, parse the report's `schemaVersion` and every item status. A partial-failure process status or one failed item means the batch did not fully succeed.

## Portable Bundle and structured output

Create a self-contained Bundle:

```sh
"$BUNDLED_INTO_MD" "/absolute/report.pdf" \
  --emit bundle \
  -o "/absolute/report.mdpkg.zip" \
  --conflict error \
  --log-format json
```

For downstream automation, request result JSON and verify `schemaVersion`, non-empty `markdown`, diagnostics, and provenance rather than scraping terminal text:

```sh
"$BUNDLED_INTO_MD" "/absolute/report.pdf" \
  --emit result-json \
  -o "/absolute/report.result.json" \
  --conflict error \
  --log-format json
```

## OCR

OCR defaults to `auto`. Use `always` only when the user requests OCR or the source is known to be scanned:

```sh
"$BUNDLED_INTO_MD" "/absolute/scan.pdf" \
  --ocr always \
  -o "/absolute/scan.md" \
  --conflict error \
  --log-format json
```

Language hints may be repeated with `--ocr-language <BCP47>`. OCR is built into Core. If it is unavailable, inspect `"$BUNDLED_INTO_MD" capabilities show ocr --json` and `"$BUNDLED_INTO_MD" doctor --json`; report the capability state without trying to install or repair internal runtime payloads.

## Transcription and diarization

Transcribe a complete local recording after capture or import:

```sh
"$BUNDLED_INTO_MD" "/absolute/meeting.webm" \
  --ai audio-transcription=only \
  -o "/absolute/meeting.md" \
  --conflict error \
  --log-format json
```

Add diarization only when speaker separation is requested:

```sh
"$BUNDLED_INTO_MD" "/absolute/meeting.webm" \
  --ai audio-transcription=only \
  --diarize \
  --expected-speakers 2 \
  -o "/absolute/meeting.md" \
  --conflict error \
  --log-format json
```

Omit `--expected-speakers` when the count is unknown. Use `--asr-language` only with a reliable language hint. Do not describe this as realtime transcription, and never validate media with renamed extensions, silence, random bytes, or mock transcripts.

If transcription or diarization is unavailable, inspect the bundled executable with `capabilities show transcription --json`, `capabilities show diarization --json`, and `doctor --json`. Keep this skill to read-only capability checks; `setup` and plugin-management commands belong to a separately requested product-management workflow.

## Remote sources and providers

Remote input must be part of the user's request. Authorize only its host:

```sh
"$BUNDLED_INTO_MD" "https://documents.example/report.pdf" \
  --allow-network \
  --allow-host documents.example \
  -o "/absolute/report.md" \
  --conflict error \
  --log-format json
```

Do not authorize redirects or additional hosts speculatively. Loopback and private-network destinations additionally require explicit user intent before adding `--allow-private-network`. Provider use follows the same per-invocation authorization rule and must rely on already configured routing.

## Validation and recovery

Treat exit status as authoritative. Then verify the requested output:

- Markdown must exist and be non-empty; inspect diagnostics before declaring a visibly sparse result correct.
- Bundle must be a readable ZIP with `manifest.json`, `document.md`, `document.ir.json`, `diagnostics.json`, and `provenance.json`.
- Result or IR JSON must parse and carry the supported `schemaVersion`.
- Batch reports must parse and account for every planned input.

Use the JSON error `code` to choose the next read-only check:

- `componentUnavailable`: inspect the corresponding capability and `doctor --json`.
- `networkDenied`: confirm that remote access was requested and that the exact host was authorized; do not broaden permission silently.
- `malformed` or `invalidMedia`: report the input failure without retrying through another converter.
- output conflict: preserve the existing destination and ask for a new path or explicit replacement authority.

Stop after one diagnostic retry unless new evidence changes the cause. Return the artifact paths, conversion status, and any remaining diagnostics in the user's language.

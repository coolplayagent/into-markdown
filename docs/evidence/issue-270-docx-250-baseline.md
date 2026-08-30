# Issue #270 DOCX corpus baseline

This evidence was captured from `main` commit
`9e797178e7072a84146da6b0a3bfb22d8d62fde5` on Windows 11 x86-64. It is preparation for issue
#270 and does not change converter or result semantics. The dependency order remains
**#272 → #268 → #270**.

## Corpus authority and pre-conversion classification

The immutable authority is `issue-270-docx-250-manifest.json`. It selects the first 250 regular
DOCX paths in case-folded relative-path order from the real-world corpus. Every entry is bound to
its byte length and SHA-256. The 250 paths contain 240 unique payloads; all ten duplicate payload
groups are listed instead of silently deduplicated.

Classification is completed before the probe starts and is verified again before every run. A
probe exit code cannot alter these categories:

| Category | Files | Meaning |
| --- | ---: | --- |
| raw | 250 | all fixed manifest paths |
| `validPolicyAllowed` | 217 | authoritative Word main part, safe package/XML structure, within default structural limits |
| `policyRejected` | 8 | six compound/encrypted containers, one DTD/entity declaration, one compression-ratio rejection |
| `defaultStructuralHardLimit` | 1 | XML nesting exceeds the default depth limit |
| `invalidPackage` | 24 | independently detected truncated/invalid ZIP or invalid OPC/Word main structure |

`invalidPackage` is never inferred from conversion failure. In particular, encrypted compound
containers are classified as policy rejections rather than being relabeled as damaged files.

## Release probe baseline

The per-file report is `issue-270-docx-250-baseline-windows.json`. It was produced with the release
build of `docx_corpus_probe` and records time, process peak RSS, Markdown size/hash, diagnostics,
and memory/temporary lease and filesystem snapshots for every file.

| Metric | Result |
| --- | ---: |
| successful conversions | 217 / 250 |
| non-empty Markdown | 200 / 250 |
| successful but empty Markdown | 17 |
| failures | 33 (26 malformed, 6 encrypted, 1 resource limit) |
| successful-file mean / median | 17.535 ms / 13.728 ms |
| successful-file p95 / maximum | 25.665 ms / 339.892 ms |
| maximum observed peak RSS | 49,729,536 bytes (47.43 MiB) |
| non-zero memory lease after completion | 0 / 250 |
| non-zero temporary lease after completion | 0 / 250 |
| temporary file-count or byte delta | 0 / 250 |

Each file is run in three fresh processes; the report keeps all three times, uses their median for
latency and their maximum for RSS, and rejects nondeterministic exit codes, errors, Markdown hashes,
or resource snapshots. The future baseline/candidate gate compares only files successful in both
runs and requires the candidate mean to remain below 150% of the baseline mean. No candidate
comparison is asserted in this preparation change.

## Empty-output evidence

All 17 empty successes remain listed as successful observations; none is reclassified. Five carry
`word.unsupportedWrapperOmitted`, while twelve have no diagnostic.

Two directly establish the altChunk defect without filename or content special-casing:

| Source | Independent package signal | Current observation |
| --- | --- | --- |
| Apache Tika `testAltChunkHTML.docx` | valid/policy-allowed; one internal altChunk; 570 payload bytes; zero `w:t` characters | success, `word.unsupportedWrapperOmitted`, 0 Markdown bytes |
| Apache Tika `testAltChunkMHT.docx` | valid/policy-allowed; one internal altChunk; 1,529 payload bytes; zero `w:t` characters | success, `word.unsupportedWrapperOmitted`, 0 Markdown bytes |

There is also one conversion failure among the 217 independently allowed packages:
`stress014.docx`, reported as malformed. This is a compatibility observation, not evidence that
the package is corrupt. `MultipleBodyBug.docx` is independently classified as an invalid Word main
structure because it contains more than one body; its converter failure does not determine that
classification.

## Reproduction

```powershell
cargo build --release -p into-markdown-converters --example docx_corpus_probe
python tools/docx-corpus-evidence.py `
  --corpus-root C:\path\to\real-world-test-data `
  --manifest docs/evidence/issue-270-docx-250-manifest.json `
  --baseline-probe target/release/examples/docx_corpus_probe.exe `
  --report docs/evidence/issue-270-docx-250-baseline-windows.json `
  --iterations 3
```

The tool first verifies all manifest hashes and classifications, then starts the first conversion.
Changing the corpus or classification rules fails verification instead of silently rewriting the
authority.

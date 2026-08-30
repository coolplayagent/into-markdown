# Issue #270 DOCX corpus baseline

This evidence compares the #270 candidate with the DOCX converter at `main` commit
`8d1d7d75` on Windows 11 x86-64. The branch consumes, but does not duplicate, the structured
summary and empty-result policy from #273 and #268. The dependency order is
**#273 → #268 → #270**.

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

## Paired Release probe

The per-file report is `issue-270-docx-250-baseline-windows.json`. It was produced with the release
build of `docx_corpus_probe` and records time, process peak RSS, Markdown size/hash, diagnostics,
and memory/temporary lease and filesystem snapshots for every file.

| Metric | `main` baseline | #270 candidate |
| --- | ---: | ---: |
| successful conversions | 217 / 250 | 217 / 250 |
| non-empty Markdown | 200 / 250 | 206 / 250 |
| verified empty source | not recorded | 11 |
| unverified successful empty output | 17 | 0 |
| failures | 33 | 33 |
| successful-file mean / median | 99.908 / 70.323 ms | 101.997 / 69.435 ms |
| successful-file p95 / maximum | 240.505 / 1,216.980 ms | 251.077 / 1,424.064 ms |
| maximum observed peak RSS | 49,700,864 bytes | 46,407,680 bytes |
| non-zero memory/temp lease after completion | 0 / 250 | 0 / 250 |
| temporary file-count or byte delta | 0 / 250 | 0 / 250 |

Each file is run in three fresh processes. Baseline and candidate runs are interleaved per file,
with odd/even rounds swapping which binary runs first. The report keeps all raw times, uses their
median for latency and maximum for RSS, and rejects nondeterministic exits, payloads or resource
snapshots. All 217 successful files are common to both sides. Their mean changed from 99.908 ms to
101.997 ms, a 2.091% fallback, satisfying the `<50%` gate.

## Empty-output evidence

The candidate produces six additional non-empty results. The remaining eleven empty outputs all
carry converter-owned `SourceContentEvidence::Empty`; no successful empty output has unknown
evidence. Unsupported objects, external relationships and glossary-only sources retain visible
placeholders, so the shared #268 policy never receives an unverified degraded empty result.

Two directly establish the altChunk defect without filename or content special-casing:

| Source | Independent package signal | Candidate observation |
| --- | --- | --- |
| Apache Tika `testAltChunkHTML.docx` | valid/policy-allowed; one internal altChunk; 570 payload bytes; zero `w:t` characters | success, `word.altChunkConverted`, 181 Markdown bytes |
| Apache Tika `testAltChunkMHT.docx` | valid/policy-allowed; one internal altChunk; 1,529 payload bytes; zero `w:t` characters | success, `word.altChunkConverted`, 211 Markdown bytes |

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
  --baseline-probe target/issue270/baseline.exe `
  --candidate-probe target/issue270/candidate.exe `
  --report docs/evidence/issue-270-docx-250-baseline-windows.json `
  --iterations 3
```

The tool first verifies all manifest hashes and classifications, then starts the first conversion.
Changing the corpus or classification rules fails verification instead of silently rewriting the
authority.

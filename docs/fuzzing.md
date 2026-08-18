# Continuous fuzzing

The repository owns seven defensive `cargo-fuzz` targets under `fuzz/`: `zip`,
`xml`, `rtf`, `pdf`, `office`, `media`, and `plugin_protocol`. They accept only
in-memory bytes, use the production Engine or the exact CLI plugin-configuration
boundary, keep networking disabled, and discard ordinary stable conversion
errors. A panic, sanitizer finding, per-input timeout, or process memory breach
is a failure.

The common parser harness limits each input to 1 MiB, expanded content to 8 MiB,
request memory to 64 MiB, temporary storage to 16 MiB, archive depth to four,
archive entries to 256, and pages to 64. LibFuzzer additionally enforces a
10-second per-input timeout and a 2 GiB process RSS ceiling. These outer limits
detect deadlock and allocator regressions even if an inner parser check is
missing. The target process has no authorization to contact an external system.

## Seed and regression authority

`fuzz/seeds.json` maps targets to the repository-generated Apache-2.0 fixture
corpus established by Issue #55. `python tools/fuzz.py prepare TARGET` verifies
that every referenced fixture is declared with the Apache-2.0 SPDX identifier in
`fixtures/manifest.json`, then content-addresses the checked-out bytes while
copying them into the ignored working corpus. The repository license gate owns
the manifest's independent size and SHA-256 drift checks.
Plugin seeds are repository-authored TOML files covered by the same seed
authority. No downloaded or user document becomes a seed.

Minimized failures are content-addressed below `fuzz/regressions/TARGET/` and
bound by `fuzz/regressions/manifest.json`. Preparing a corpus always verifies and
includes these regression fixtures. The scheduled workflow minimizes the first
failure, records its size, SHA-256, license and provenance, and opens a reviewable
pull request automatically. Raw crashes and the sanitizer report remain CI
artifacts for 30 days. Promotion can also be reproduced locally:

```shell
python tools/fuzz.py minimize zip fuzz/artifacts/zip/crash-...
```

## Gates

The pull-request workflow runs all seven Linux AddressSanitizer targets for 60
seconds each. The scheduled workflow is deliberately separate: one labeled,
self-hosted Linux runner per target runs AddressSanitizer for 86,400 seconds.
Its 25-hour job timeout leaves bounded setup and archival time without silently
shortening the campaign. A weekly differential job also runs short macOS ARM64
AddressSanitizer and Linux non-sanitized coverage samples.

`fuzz/platforms.json` is the machine-readable platform/sanitizer policy. Windows
MSVC cargo-fuzz execution is tracked as unsupported; promoted byte fixtures are
portable and remain part of the corpus on every supported fuzz runner. Every CI
attempt emits a JSON report containing target, sanitizer, platform, commit,
status, and content hashes of failures, so a platform-only result is visible
rather than averaged away.

The PDF target reaches the production PDF boundary. Full native parsing requires
the separately audited pinned PDFium runtime on the self-hosted fuzz runner; a
missing runtime is a stable component error, never a substitute parser. Audio
and video likewise retain their process-isolated, explicitly audited FFmpeg
boundary; raster media parsing runs in-process in every profile. Reports must
state runner provisioning, so native and non-native coverage are not conflated.

## Local short run

Install nightly Rust and the pinned runner, then run:

```shell
cargo install cargo-fuzz --locked --version 0.13.1
python tools/fuzz.py prepare xml
cargo fuzz run --sanitizer address xml fuzz/corpus/xml -- \
  -runs=1000 -max_len=1048576 -timeout=10 -rss_limit_mb=2048 \
  -artifact_prefix=fuzz/artifacts/xml/ -dict=fuzz/dictionaries/xml.dict
```

Do not run the harness against services, URLs, mounted third-party corpora, or
unreviewed native runtimes. It is a repository-owned parser quality gate, not a
scanner.

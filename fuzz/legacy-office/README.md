# Legacy Office fuzzing

The three targets enter the production Core converter with the ordinary request memory credit.
Run a bounded smoke locally with:

```sh
cargo +nightly-2026-08-20 fuzz run --fuzz-dir fuzz/legacy-office legacy_doc fuzz/legacy-office/corpus/legacy_doc tools/macos-release/fixtures -- -max_total_time=30
cargo +nightly-2026-08-20 fuzz run --fuzz-dir fuzz/legacy-office legacy_ppt fuzz/legacy-office/corpus/legacy_ppt tools/macos-release/fixtures -- -max_total_time=30
cargo +nightly-2026-08-20 fuzz run --fuzz-dir fuzz/legacy-office legacy_xls fuzz/legacy-office/corpus/legacy_xls tools/macos-release/fixtures -- -max_total_time=30
```

The checked-in four-byte seeds exercise the shortest malformed paths. CI additionally supplies the
repository-owned `tools/macos-release/fixtures/` corpus to every target, so mutations reach the
authenticated CFB and format-specific record graphs without duplicating those binary fixtures.

# Local test gates

Date: 2026-08-23
Branch head at focused rerun: `dd3c6f1`

## Passing gates

- `bazel test //web/console:unit_test //web/console:typecheck //web/console:determinism_test //web/console:dist_integration_test //web/console:update_assets_test --test_output=errors`: 9/9 Web tests passed, with the type, deterministic build, embedded-dist and asset-update checks successful.
- `cargo test -p into-markdown-cli capability_source_off_replaces_the_exact_scope_with_a_valid_disabled_route`: 1/1 passed.
- `cargo test --locked -p into-markdown-cli --bin into-md ui::tests -- --test-threads=1`: 17/17 passed.
- `cargo test --locked -p into-markdown-cli web_tasks::tests -- --test-threads=1`: 63/63 passed.
- `bazel test //apps/cli:web_security_test //web/console:web_security_test --test_output=errors`: all four resolved targets passed, including the production CLI boundary and embedded console tests.
- `bazel test //tools/installed-smoke:installed_smoke_test --test_output=errors`: focused rerun passed 21/21. In the full parallel run, two process-grandchild timing tests had failed once; the isolated rerun shows this was concurrency-sensitive rather than a persistent product failure.
- `cargo fmt --all -- --check` and `git diff --check`: passed before the `2cf47fd` commit.
- `cargo test -p into-markdown-cli --bin into-md -- --test-threads=1`: 234/234 passed after commit `7cb3307` added batch-output lease serialization.
- `cargo test -p into-markdown-cli app::tests::parallel_batch_serializes_atomic_output_leases -- --exact --test-threads=1`: 1/1 passed. The test converts 24 real text inputs with eight workers into one output directory and asserts that all outputs exist without `transactionBusy`.
- `cargo test --locked -p into-markdown-asr`: 26/26 library tests and the available 1/1 quality gate passed; the pinned-model quality test remained explicitly ignored by its test contract. The passing unit set includes incomplete trailing UTF-8, invalid provider bytes, and token bytes split inside UTF-8 recovery.
- Console typecheck passed with Node 24.19.0 (the repository requests 24.13.0). The focused console shards passed: workbench 17/17, preview 4/4, history cleanup 1/1, history actions 1/1, and accessibility 4/4. Commit `1a18458` keeps the renamed workbench assertions in the intended shard.
- After the final code self-review in `dd3c6f1`, the complete Web gate was rerun: 9/9 targets passed, the TypeScript build passed, and `//apps/cli:cli_test` passed with the regenerated embedded assets. The administration bundle now runs 15/15 tests, including deletion/rerun pagination clamping, legacy-context synchronization, nested Tab/Shift+Tab focus containment, numeric preference validation, and focus restoration.

Core-only plugin lifecycle evidence is stored separately under `docs/qa/evidence/runtime/`.

## Real local conversion evidence

- Local OCR, current debug CLI, isolated three-plugin home, default parallel jobs: 3/3 real PNG fixtures succeeded in 6.92 seconds. The earlier run had succeeded for two images and failed the mixed image with `transactionBusy`; after `7cb3307`, English, mixed Chinese/English, and Simplified Chinese outputs all completed in the same batch. Report: `/private/tmp/into-md-ocr-rerun.FnriZk/report.json`.
- Legacy Office, current debug CLI, isolated three-plugin home: real `.doc`, `.xls`, and `.ppt` fixtures completed 3/3. The outputs preserved document text, spreadsheet row/formula order, and two-slide order with speaker notes. The batch took 249.64 seconds, so functional acceptance passed but the high cold batch latency is recorded and is not represented as a performance pass. Report: `/private/tmp/into-md-office-rerun.G05O9t/report.json`.
- Local speech, current optimized CLI plus a locally signed package containing the current optimized media provider: 10/10 real speech inputs completed in one single-worker invocation in 103.63 seconds. The matrix covered WAV, CBR and VBR MP3, M4A/AAC, OGG/Opus, FLAC, WebM, an AAC/M4A file deliberately named `.mp3`, and durations through 185.008 seconds. All ten Markdown outputs were non-empty, and all 142 parsed timestamp ranges were monotonic and non-overlapping. A debug-host control run took 132.29 seconds and produced ten byte-identical artifacts. The earlier package had failed 4/10 long recordings with an old Whisper UTF-8 error; the current provider did not reproduce it. This is current-source package evidence, not the final post-merge installed artifact. Details: `docs/qa/evidence/runtime/audio-current-release-matrix.md`; report: `docs/qa/evidence/runtime/audio-current-release-report.json`.
- Ten-second-class local speech timing, current optimized CLI and the same isolated current-provider package: a 113,971-byte, 9.059-second real M4A completed in 9.24 seconds and 8.10 seconds in two independent processes. The same input under the debug CLI took 34.28 and 33.36 seconds; sampling attributed that debug-only gap to the unoptimized signed-runtime SHA-256 copy before provider launch. All four timed outputs were byte-identical. This meets the current optimized-build target but remains pending on the final installed binary.
- The OCR and Office runs used `--no-config` plus the isolated plugin configuration explicitly. The speech run used a fresh isolated user-data root whose only configured plugin was the current-source media package. Both approaches avoid unrelated project/global Provider configuration while retaining installed local plugin authority.

## Full repository run

`bazel test //... --test_output=errors` built all targets and reported 43/47 passing. The four failing targets were separated as follows:

1. `//apps/cli:exit_contract_test`: 13/15 passed; the provider double-authorization and invalid-notebook tests expected exit 5 but received exit 2. The exact same two assertions were reproduced from an isolated detached `origin/main` (`120853c`) worktree. This is a confirmed pre-existing baseline failure, not introduced by PR 2.
2. `//tools/installed-smoke:installed_smoke_test`: two timing-sensitive process tests failed in the parallel run, then the focused target passed 21/21.
3. `//tools/license-check:license_check` and `//tools/license-check:license_check_unit_test`: the repository reports stale Cargo normal-runtime authorities and nondeterministic/stale npm SPDX authority. PR 2 does not modify `Cargo.lock`, `apps/official-provider/Cargo.toml`, `crates/api/Cargo.toml`, `crates/ocr/Cargo.toml`, or `tools/license-check`; these are recorded as pre-existing repository audit failures and are not suppressed.

These distinctions do not waive the product acceptance matrix: real remote routes, remaining Edge controls, final installed-artifact checks and exact final screenshots remain explicitly pending or blocked in `product-control-matrix.md`.

## Pull request infrastructure

Every inspected PR #187 job on both the earlier and current heads failed within 1–6 seconds with an empty `steps` array: Bazel platform contract, Local Web security boundary, and plugin-manager. Two push-only workflow-file runs also failed without creating any jobs. The runs contain no checkout, build, test or log output, so they are recorded as runner/account infrastructure failures before code execution. Local gates above remain the code evidence; the failed badges are not represented as passing CI.

# Contributing to Into Markdown

[中文](CONTRIBUTING.md)

Thank you for improving Into Markdown. This repository owns the local-first conversion Core,
two complete capability plugins, the Agent Skill, installers, and four-platform release
authority. A change must preserve the common IR, offline default, explicit authority, and
reproducible release boundary rather than making only one entry point appear to work.

## Before you start

- Search existing issues first. Keep one focused PR per issue and link it in the PR body.
- Do not disclose exploit details in a public issue. Use the private channel described on the
  repository Security page.
- Do not commit customer documents, credentials, private keys, download caches, generated models,
  capability runtimes, or machine-local test output.
- A new dependency, source, model, or runtime must update its lockfile, source authority, license
  inventory, SBOM, and release inventory. A README statement cannot replace machine validation.

## Product and interface constraints

- Conversion produces validated Document IR before the common renderer generates Markdown. CLI,
  Web, plugins, and providers cannot maintain separate semantic result models.
- Local input is offline by default. Remote sources and providers access the network only with
  explicit authority for the current invocation; private destinations require a separate grant.
- Office 97–2003 parsing is built into Core. OCR and speech ship as complete capability plugins
  containing their runtimes, models, licenses, and SBOMs, and are installed, updated, and verified
  as plugin units.
- Changes to public commands, DTOs, error codes, the format catalog, plugin or capability IDs, and
  release paths need compatibility analysis, tests, and synchronized Chinese and English docs.
- Security failures return stable, parseable errors. They must not panic, silently downgrade, or
  present incomplete output as success.

## Build and test

Bazel is authoritative for release builds; Cargo provides fast feedback and focused crate tests.
Run the closest gate first, then the affected higher-level contracts:

```sh
bazel build //...
bazel test //...
cargo fmt --all -- --check
cargo check --workspace --locked
```

Format or conversion changes use structurally real documents, images, or media and assert IR,
Markdown, resources, diagnostics, and provenance. Renamed extensions, random bytes, silent audio,
or compile-only checks are not substitutes. Native-runtime and capability-plugin claims require
black-box testing from a fresh release install. See [`docs/testing.md`](docs/testing.md) and
[`docs/installed-smoke.md`](docs/installed-smoke.md) for the detailed gates.

Run the executable documentation contract for documentation changes:

```sh
bazel test //tools/docs-check:docs_check_test
```

It discovers public commands and the current format catalog from the real CLI, checks Chinese and
English example coverage, command syntax, and local links, and performs real TXT and stdin
conversion plus a dry-run for every available format. When adding or changing a public command or
format, update [`docs/cli-examples.md`](docs/cli-examples.md) and its English counterpart.

## CI change constraints

- CI permits only the four existing jobs in `.github/workflows/pr-fast-gate.yml`:
  Linux x86_64 (shared tests and Web), Linux ARM64 Core, Windows x86_64 Core, and macOS ARM64 Core.
- Preserve their names, runners, five-minute timeouts, and `pull_request` trigger. Unit tests may
  be added to these jobs: shared tests belong in Linux x86_64 and platform-specific tests in the
  corresponding job. Run expensive suites, full builds, and real-runtime matrices locally on
  demand, preserving the fast gate's time budget.
- Additional workflows, jobs/tasks, matrix platforms or combinations, and manual, scheduled, or
  other automatic CI triggers are forbidden. Scripts and reusable workflows must not dispatch
  additional CI. Keep the allowlist validator enabled and intact when adding tests.
- The workflow directory contains only `pr-fast-gate.yml` and the manual release workflow
  `platform-modular-release.yml`. Run the release workflow only when the user explicitly requests
  a release or installed-artifact acceptance.
- The existing `pr_fast_gate.py` invocation in each job runs `ci_workflow_policy.py`, rejecting
  extra workflows, jobs, matrices, changed runners/names, and automatic release triggers. The
  validator uses a fixed YAML block layout; preserve the workflow control structure when editing
  unit tests. Changes to the allowlist or release triggers require explicit user approval. Before
  delivery, run `python3 -m unittest tools.platform-release.test_pr_fast_gate` and review workflows
  and the scripts they invoke together.

## Plugin and release changes

Process and WASI plugins follow the `process-v1` and `wasi-v1` isolation contracts. See
[`docs/plugin-development.en.md`](docs/plugin-development.en.md) for development, signing, and
lifecycle gates. Release scripts materialize every platform artifact from one canonical source,
pin inventories, digests, permissions, and signatures, and verify the unpacked result. Do not copy
an implementation into platform-specific scripts or mutate user agent-skill and configuration
directories.

## PR completion criteria

Describe the user-visible result, security and compatibility impact, tests executed, and exact
gates that could not run locally. Before submission, review the README, authoritative docs,
CLI/Web behavior, install and uninstall paths, four-platform releases, and supply-chain evidence
as one product. Remove temporary files and debug switches, and ensure `git diff --check` passes.

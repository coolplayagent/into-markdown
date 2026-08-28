---
name: into-markdown
description: Convert documents, images, audio or video, standard input, directories, and explicitly authorized remote sources into Markdown or structured artifacts with the bundled into-md CLI. Use when the user asks to run Into Markdown or convert source files; do not use for editing existing Markdown, generic summarization, Web UI administration, plugin management, or provider configuration.
---

# Into Markdown

Use the executable bundled in this skill to produce the requested artifact. Selecting and starting the bundled Core is offline. Do not search `PATH`, install, download, upgrade, reconfigure, or contact the network during that process. Network access is allowed only when the user explicitly requests a remote-source conversion, under the narrow authorization rules below.

1. Resolve the absolute directory containing this `SKILL.md`, inspect the host OS and CPU architecture, and select exactly one executable:
   - Windows x86_64: `assets/windows-x86_64/into-md.exe`
   - Linux x86_64: `assets/linux-x86_64/into-md`
   - Linux ARM64: `assets/linux-arm64/into-md`
   - Any other platform: stop with `The bundled Into Markdown skill supports Windows x86_64, Linux x86_64, and Linux ARM64; this host is unsupported.`
2. Invoke that absolute path directly and run `version --json`. If the asset is missing or cannot execute, report that packaging or host error instead of searching for another copy or substituting another converter.
3. Choose Markdown for an ordinary conversion, Bundle when the result must retain resources and provenance as one portable file, or structured JSON only when the user or downstream workflow needs it.
4. Run conversion as `<BUNDLED_INTO_MD> [OPTIONS] <INPUT...>` with an explicit output destination. Respect discovered user and project configuration unless the user requests an isolated diagnostic run.
5. Protect existing work with `--conflict error`. For a directory or multiple inputs, run `--dry-run` first, then perform the conversion with `--report <REPORT.json>`.
6. Check the process exit status, expected artifact existence and non-empty content. For batch work, parse every report item and disclose partial failures. Do not infer success from an output row or file alone.
7. On failure, prefer the stable event from `--log-format json`. Use the bundled executable's `doctor --json` or relevant read-only capability query only when it helps distinguish unavailable capability, invalid input, configuration, or network policy.

Read [references/cli-workflows.md](references/cli-workflows.md) before running OCR, transcription, diarization, batch, Bundle, stdin, remote-source, or failure-recovery workflows.

## Boundaries

- Never add `--allow-network`, `--allow-private-network`, or a remote provider merely to make a conversion succeed. Network use requires matching user intent for that invocation; restrict authorized traffic with `--allow-host` whenever possible.
- OCR and Office 97–2003 parsing are built into Core. Speech transcription and diarization may come from the optional media plugin; report an unavailable capability and its stable diagnostic without trying to install or manage it. Do not expose or manage internal runtime payloads or models.
- Do not open `into-md ui` for agent-run conversion. Do not install, enable, disable, update, or remove plugins, and do not change provider or product configuration under this skill.
- Preserve source files. Never overwrite an existing destination unless the user explicitly requests replacement and the consequences are clear.
- Reply in the user's language and link or identify every generated artifact.

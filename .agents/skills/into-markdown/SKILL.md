---
name: into-markdown
description: Convert documents, images, audio or video, standard input, directories, and explicitly authorized remote sources into Markdown or structured artifacts with the installed into-md CLI. Use when the user asks to run Into Markdown or convert source files; do not use for editing existing Markdown, generic summarization, Web UI administration, plugin management, or provider configuration.
---

# Into Markdown

Use the installed `into-md` command to produce the requested artifact. Do not build, install, upgrade, or reconfigure the product unless the user separately asks for that work.

1. Locate `into-md` with the host shell and run `into-md version --json`. If it is unavailable or unsupported on this host, report that directly instead of substituting another converter.
2. Choose Markdown for an ordinary conversion, Bundle when the result must retain resources and provenance as one portable file, or structured JSON only when the user or downstream workflow needs it.
3. Run conversion as `into-md [OPTIONS] <INPUT...>` with an explicit output destination. Respect discovered user and project configuration unless the user requests an isolated diagnostic run.
4. Protect existing work with `--conflict error`. For a directory or multiple inputs, run `--dry-run` first, then perform the conversion with `--report <REPORT.json>`.
5. Check the process exit status, expected artifact existence and non-empty content. For batch work, parse every report item and disclose partial failures. Do not infer success from an output row or file alone.
6. On failure, prefer the stable event from `--log-format json`. Use `into-md doctor --json` or the relevant read-only capability query only when it helps distinguish unavailable capability, invalid input, configuration, or network policy.

Read [references/cli-workflows.md](references/cli-workflows.md) before running OCR, transcription, diarization, batch, Bundle, stdin, remote-source, or failure-recovery workflows.

## Boundaries

- Never add `--allow-network`, `--allow-private-network`, or a remote provider merely to make a conversion succeed. Network use requires matching user intent for that invocation; restrict authorized traffic with `--allow-host` whenever possible.
- Treat OCR, speech, and diarization as complete capability plugins. Office 97–2003 parsing is built into Core. If a selected plugin is unavailable, report the complete capability state and its stable diagnostic.
- Do not open `into-md ui` for agent-run conversion. Do not install, enable, disable, update, or remove plugins, and do not change provider or product configuration under this skill.
- Preserve source files. Never overwrite an existing destination unless the user explicitly requests replacement and the consequences are clear.
- Reply in the user's language and link or identify every generated artifact.

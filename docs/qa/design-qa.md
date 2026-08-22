# Product design QA

Date: 2026-08-23
Browser: Microsoft Edge
Implementation branch: `codex/admin-visual-pagination`

## Decision baseline

The generated preferences images are design explorations, not specifications. The implementation takes only the confirmed qualities from `exec-2525c281-c247-4d68-b677-8868226de291.png` and its related iterations: stable left navigation, task-grouped rows, aligned labels and controls, and a stable bottom save area. It deliberately does not copy the six-entry administration information architecture shown in those images.

The product now exposes three administration tasks only:

1. Capabilities and sources
2. Preferences
3. Diagnostics

Format support, AI services, and local extensions are contextual views within Capabilities and sources. Old routes normalize into that context instead of preserving hidden parallel navigation.

The speech workspace implements record or import, then submit complete audio for transcription. It does not represent transcription as live, does not promise meeting summaries, keeps history auxiliary to the current recording, and provides save, discard, remove, progress/cancel, result, and regenerate actions in stable locations.

## Evidence policy

- The 62 clipboard images archived in `docs/qa/evidence/negative/` are negative or function-only evidence. None is a target visual.
- Earlier successful transcript/result captures prove only that a function ran. They do not prove recognition quality, layout, accessibility, pagination, or interaction quality.
- `docs/qa/visual-regression.md` maps every archived negative capture to the issue family, code surface, Edge regression evidence, and operated controls.
- Final screenshots are captured from the built-in UI served by the real CLI and operated in Microsoft Edge. DOM-only assertions are supporting evidence, not substitutes for screenshots or control operation.

## Responsive review

The core pages are checked at these Edge viewport sizes:

- 1920 × 1080: wide desktop; content remains bounded and does not create an empty half-screen.
- 1365 × 768: standard desktop acceptance viewport.
- 1024 × 768: compact desktop/tablet landscape; dialogs stay within the viewport and tables scroll only where necessary.
- 375 × 812: narrow mobile; navigation, cards, save controls, dialogs, and history pagination remain usable without horizontal page overflow.

Contact sheets live beside the source captures under `docs/qa/evidence/edge/`. Screenshots named `*-pre-final.png` are intentionally retained as negative before-evidence and are not final-state approval.

## Interaction and accessibility review

- Local validation, task failure, provider failure, plugin upload failure, and save confirmation are rendered beside the initiating control or object.
- Result and administration dialogs move focus inside on open, close only the topmost layer on Escape, and restore focus to the trigger on close.
- Permanent deletion uses danger styling, names the affected object, and requires confirmation.
- Terminal task state is expressed in user language; internal values such as `failed`, `succeeded`, `conflict`, and provider error codes are not appended to normal task rows.
- Initial Preferences rendering uses a five-section skeleton and stable save area while data is fetched; a delayed response cannot overwrite a newer section request.
- Pagination controls report the current range/page, expose disabled boundary states, and remain reachable on narrow viewports.

## Design exploration comparison

The reference preferences image and the final Preferences page are compared for spacing, row rhythm, alignment, navigation stability, and save-area behavior only. The comparison must not be interpreted as approval of reference labels, fields, or six-entry navigation. The final same-size comparison is generated after the installed-binary Edge pass and stored under `docs/qa/evidence/edge/`.

## Current comparison artifacts

- Source visual truth: `/Users/yx/.codex/generated_images/01a024f8-d0c5-7293-8ffd-46315bbbc705/exec-2525c281-c247-4d68-b677-8868226de291.png`.
- Source pixels: 1487 × 1058. It is an exploration, and only the four qualities named in the decision baseline are comparison targets.
- Intermediate implementation: `docs/qa/evidence/edge/preferences-1365x768.png`.
- Implementation pixels and CSS viewport: 1365 × 768 at device scale 1.
- State: Simplified Chinese, light theme, Preferences with Documents and recognition and Output expanded.
- Combined evidence: `docs/qa/evidence/edge/preferences-exploration-intermediate-comparison.png` (2484 × 816). The source was proportionally scaled to 768 pixels high and placed beside the unscaled implementation. This is an interim composition check, not a same-viewport fidelity pass.

## Findings

- [P1] Final same-viewport comparison is missing. The exploration and intermediate Edge capture have different aspect ratios and heights, so spacing, typography, row density, and bottom-action placement cannot be judged precisely yet. Capture the installed implementation at 1487 × 1058 and rebuild the combined comparison.
- [P1] Final screenshots are not from the installed artifact. The retained exact-size matrices predate final installation, while the `final-*` files are cropped by the locked desktop and are rejected as evidence.
- [P2] The interim comparison shows the intended task grouping, aligned two-column rows, three-entry navigation, and stable bottom action slot. The implementation deliberately differs from the exploration's six-entry navigation and does not treat that difference as drift.

## Required fidelity surfaces

- Fonts and typography: interim hierarchy is readable, but final font size, line height, wrapping, and optical weight remain blocked on an equal-viewport capture.
- Spacing and layout rhythm: task rows and controls align consistently; final sidebar proportions, section gaps, sticky-action overlap, and viewport bottom behavior remain blocked.
- Colors and visual tokens: the existing product accent, surface, border, and semantic-state tokens are preserved; final light/dark Edge comparison remains pending.
- Image quality and asset fidelity: this settings surface has no custom raster asset or logo substitution. Existing Lucide icons and the product brand mark are reused.
- Copy and content: the implementation uses three administration tasks and user-facing settings language. The six-entry exploration IA and its unsupported field set are intentionally excluded.

## Comparison history

1. Intermediate comparison: confirmed the permitted structural qualities and rejected pixel-level judgment because the source and implementation viewports do not match.
2. Post-fix comparison: pending the unlocked Microsoft Edge installed-artifact capture. No P0/P1/P2 visual finding is closed solely from the intermediate screenshot.

## Final result

final result: blocked

Blockers: macOS is locked, so Microsoft Edge cannot produce or inspect the final same-viewport screenshots; PR 2 is still draft and the installed binary has not yet been rebuilt from both merged PRs.

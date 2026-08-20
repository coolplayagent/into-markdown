# Product Design QA: unified workbench

## Source and implementation

- Reference visual: `/Users/yx/.codex/generated_images/019ff4e6-d0a0-7ef2-aef9-09b2b00fd528/exec-943c23ab-c980-4b9f-9cef-a2f5a4f74705.png`
- Workbench capture: `/private/tmp/into-md-design-qa-workbench-single-page-1280.png`
- Result-dialog capture: `/private/tmp/into-md-result-redesign-final.png`
- Adaptive result-dialog capture: `/private/tmp/into-md-result-adaptive-final.png`
- Unavailable-audio state capture: `/private/tmp/into-md-audio-disabled-final.png`
- Result before/after comparison: `/private/tmp/into-md-result-redesign-comparison.png`
- Same-frame comparison: `/private/tmp/into-md-design-qa-workbench-comparison.png`
- Truth statement: the implementation keeps option 1's restrained teal palette, compact controls, fine borders, icon language, and two-column workbench while consolidating history, setup, and results into one application surface.

## Information architecture

- The header contains brand, service state, language, and theme only. Standalone status and history destinations are removed.
- The left panel contains file selection, the active batch, and the complete recent-task list in one bounded scroll region.
- The right panel keeps capabilities, conversion settings, and the primary action continuously visible.
- Selecting a completed task opens a full result dialog without changing the route. Reading preview, Markdown source, batch switching, download, resources, diagnostics, retry, and deletion stay inside that dialog.
- Audio transcription is enabled by default. Runtime readiness is independent from that preference; an unavailable runtime exposes an in-place dependency-preparation dialog rather than another page.
- Cleanup remains an explicit user action. Conversion, preview, restart, and navigation never clear history automatically.

## Visual comparison

- The implemented workbench uses the same visual hierarchy as option 1: strong title, quiet canvas, white functional panels, deep-teal selected states, compact status treatment, and a persistent primary action.
- The reference's inline result and history blocks are replaced by a scrollable recent-task region and an integrated reading dialog. The redesigned dialog removes the nested paper card, heavy frame, and single crowded toolbar while preserving the one-page workflow.
- The final visual pass removed the cramped audio status layout and corrected the settings-card overflow visible at compact desktop widths.
- Live in-app-browser QA at 1280 x 720 is stricter than the target desktop widths and has zero document-level vertical scrolling. Responsive rules and automated tests continue to cover the 1440 x 900 and 1493 x 1048 desktop layouts.

## Interaction and behavior checks

- Persisted tasks restore original filenames and formats; all five available recent items render rather than an arbitrary three-item cap.
- The recent list has `overflow-y: auto` and an actual scroll range while the page itself remains fixed.
- Audio transcription starts checked. “准备依赖” opens a same-page dialog with the exact model command and the fixed LGPL FFmpeg requirement.
- Opening `columns-then-table.pdf` preserves `/` and opens one accessible result dialog. Batch switching and the rendered Markdown table are visible without a route change.
- Reading preview, Markdown source, detail drawer, and close behavior were exercised in the live packaged binary.
- The real rent-contract artifact no longer exposes raw `<em>` tags, Markdown escapes, or OCR boundary labels in reading mode. Ordered items use aligned, styled markers; source mode remains byte-faithful to the downloaded Markdown.
- At 1280 x 720 the document page itself has no vertical scroll (`720 / 720`); the result document is the dialog's single vertical reading region (`3275 / 532`).
- The result dialog is portalled directly to `document.body`, so route motion never becomes its containing block. At the live Mac browser's 1280 x 720 CSS viewport it measures `top 18 / bottom 702`, preserving equal visible margins and eliminating the clipped lower edge.
- The backdrop uses a 68% product-background wash with `14px` blur, reduced saturation, and slight brightness control; background headings and controls no longer read as competing bold content.
- When the verified audio runtime is missing, the transcription switch is off and disabled. The dependency action remains visible, and uploads cannot request ASR until the runtime reports ready.
- The service status is not a navigation target; `/history` and `/status` no longer have standalone UI surfaces.
- Frontend typecheck, unit suites, distribution integration, asset determinism/update checks, Rust UI tests, formatting, and diff checks passed.

## Accessibility and motion

- Icon-only controls retain accessible names; dialogs expose modal roles and labelled headings; status is not communicated by color alone.
- Native-looking selects and checkboxes are replaced by product-styled controls with visible focus states.
- Buttons, segmented controls, menus, drawers, dialogs, switches, progress, and route content use restrained transitions.
- `prefers-reduced-motion` collapses all animation and transition durations.

## Open findings

- P0: none.
- P1: none.
- P2: none.

## Final result

passed

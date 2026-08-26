# Microsoft Edge product control matrix

Date: 2026-08-23
Browser: Microsoft Edge with the supported Codex browser connection; native macOS pickers are used for local file selection.
Build under test: the optimized `codex/admin-visual-pagination` CLI assembled in a release-style `install/bin` + `install/lib` layout for current Web and Core rows; older rows explicitly marked debug remain supporting evidence only. The current speech cold/warm rows use the release CLI with the signed global media plugin.

`PASS` means the visible control was operated in Microsoft Edge and its resulting UI or persisted state was inspected. Unit or API checks appear only as supporting evidence. `BLOCKED` and `PENDING` are deliberately not counted as acceptance.

## Global shell

| Surface | Control / states operated | Result | Evidence |
|---|---|---:|---|
| Header | 文稿转换、语音转写、系统管理 navigation | PASS | Each link opened its named workspace; the selected administration route exposed only the three-task sub-navigation. |
| Header | Language 简体中文 → English → 简体中文 | PASS | Visible labels changed in place and restored without losing the current route. |
| Header | Theme 跟随系统、浅色、深色 | PASS | Each option changed the rendered theme and the final system setting remained selected. |
| Header | Skip to main content | PASS | From the Edge address bar, keyboard traversal reached the visible skip link; activating it moved focus to the main workspace without changing the route. |

## Document workbench

| Surface | Control / states operated | Result | Evidence |
|---|---|---:|---|
| Source | Native Edge file picker with real `wide.json` | PASS | File appeared in the current selection, conversion completed, result preview opened. |
| Source | Directory picker | PASS | The native Edge directory picker opened at the real PDF fixture directory, displayed the browser's explicit 12-file upload confirmation, and added all 12 PDFs with their relative `pdf/…` paths. |
| Source | Drag and drop | PENDING | Must be executed through the normal Edge surface, not a synthetic DOM event. |
| Selection | Remove selected file | PASS | Removing `columns-then-table.pdf` changed the visible selection and Start button counts from 12 to 11 without shifting the workspace shell. |
| Conversion | OCR 自动、始终识别扫描内容、关闭 | PASS | All three segmented controls changed pressed state. |
| Conversion | Assets 保存到同名文件夹、不保存附件 | PASS | Both supported controls changed pressed state and their local help text updated. The removed inline-embedding choice is no longer offered in the ordinary workbench. |
| Conversion | Start conversion | PASS | Real JSON conversion reached completed result. |
| Remote OCR | Select `qa-local`, one-upload consent check/uncheck | PASS | Consent appeared only beside Start conversion; checked state set per-upload network and service authorization, then cleared both. |
| Remote OCR | Select 关闭 and switch | PASS | Edge displayed 来源已更新; project TOML persisted `mode = "off"` without `primary`; `/api/admin` returned `currentSource = "off"`. |
| Current task | Open completed task | PASS | Result dialog opened with task title and status. |
| Result | 阅读预览 / Markdown 源码 | PASS | Both visible modes rendered the converted JSON content. |
| Result | 详情与资源 open/close | PASS | Drawer opened and closed without dismissing the underlying result. |
| Result | Close / Escape / focus restore | PASS | Close restored focus to the originating `wide.json` task row; Escape closes only the top layer. |
| Result | More menu, pin/unpin | PASS | Pin state changed in the menu and was restored. |
| Result | Retry | PASS | Retry created new completed tasks; reloading history exposed them. |
| Result | Download Markdown | BLOCKED | Click reached Edge download/save handling, but the locked desktop prevented completion; a `.crdownload` is not acceptance. |
| Result | Permanent delete | BLOCKED | Edge confirmation was reached but not accepted; deletion needs explicit action-time approval and must use a disposable task. |
| History | Search hit / no hit | PASS | `wide` returned matching tasks; `no-such-task` rendered the local no-results state. |
| History | Status all / completed / failed | PASS | Completed showed matching rows; failed showed the local no-results state; all restored the set. |
| History | Pagination next / previous / boundaries | PASS | Seven real workbench tasks produced page 1/2 and 2/2; both directions and disabled boundaries were inspected. |
| History | Cleanup now | PENDING | Confirmation and exact retained/deleted set still require an approved disposable-data run. |

## Speech workspace

| Surface | Control / states operated | Result | Evidence |
|---|---|---:|---|
| Record | Start, pause, resume, stop | PASS | Edge captured real spoken audio; live waveform/duration changed during recording and stopped into a complete draft. |
| Recording draft | Save recording | PASS | Edge produced `meeting-2026-08-22T17-32-38.200Z.webm`, 556,833 bytes. |
| Recording draft | Play | PASS | Recorded audio played from the draft control. |
| Recording draft | Discard/remove | PENDING | Must be operated on a disposable recording without losing the retained evidence file. |
| Transcription | Submit completed recording | PASS | 34-second real speech completed through the local speech plugin and opened a timed transcript result. |
| Import | Native picker, real 10-second WebM | PASS | Import completed and produced a local transcript; see `audio-webm-10s-result-1365x768.png`. |
| Import | Real 9.059-second M4A and cancellation | PASS | Native Edge picker accepted `medium-real-aac.m4a`; cancellation stayed in the fixed action slot, first showed disabled `正在取消`, then converged to `已取消` without moving the three-column shell. |
| Import | Real 31-second WebM cold and warm | PASS | Native Edge picker accepted `long-real.webm` (501,441 bytes). The release service completed a cold task in 9,747 ms and the immediately repeated task in 6,788 ms; both opened a 13-segment timed transcript result with speaker labels. |
| Import | Real 185.000-second M4A derived from the real 185.008-second WebM fixture | PASS | The macOS Edge native picker accepted `into-md-qa-very-long-real.m4a` with the production media accept filter intact. Pure transcription completed in 15,128 ms of durable task time, produced a 5,475-byte Markdown artifact with 76 monotonic timestamp ranges through 178.8 seconds, and opened the real result preview. Evidence: `/private/tmp/into-md-product-audit-current/01-185s-transcript-result.jpeg`. |
| Transcript | Preview/source/details/close | PASS | Result views and focus-safe close were operated. |
| Transcript | Rename speaker | PASS | Speaker was renamed to 主持人 and the regenerated artifact persisted the name. |
| Transcript | Cancel running task | PASS | A real M4A task was cancelled in Edge; progress/cancel/terminal feedback reused one stable action slot and the task finished as `已取消`. |
| Transcript | Regenerate | PASS | Result retry/regenerate control created another task. |
| Remote route | Per-upload service consent and remote transcription | BLOCKED | UI consent was exercised, but no `DASHSCOPE_API_KEY` is present, so the real remote result is not accepted. |

## Capabilities and sources

| Surface | Control / states operated | Result | Evidence |
|---|---|---:|---|
| Capability rows | Source select and switch | PASS | OCR switched to `qa-local` and then to the valid disabled route; speech/local source state remained readable. |
| Capability rows | Install local OCR | PENDING | Will be exercised from Core-only in the installed-artifact Web pass. |
| Capability rows | Verify local speech | PASS | Edge ran the row-local check to completion; the same fixed-width button changed from progress/cancel to `验证通过` and returned without moving the capability row. |
| AI services | Open / Escape / close / focus restore | PASS | Context dialog opened over Capabilities and Escape closed the top layer. |
| AI services | Add all fields and save `qa-local` | PASS | Vendor-neutral service persisted base URL, model/capabilities, environment-variable name, allowed host and private-network choice. No secret value was entered. |
| AI services | Edit / cancel | PASS | Existing service fields opened and cancel left persisted configuration unchanged. |
| AI services | Test connection | PASS | Failure stayed on the `qa-local` card with user-facing missing-secret/network text and no internal error code. |
| AI services | Set as capability source | PASS | OCR source change was reflected in the capability row and workbench consent. |
| AI services | Delete | BLOCKED | Requires explicit action-time approval; remote real-route cleanup remains mandatory after that test. |
| Local extensions | Open / Escape / close / focus restore | PASS | Context dialog layering and focus restoration were inspected. |
| Local extensions | Local `.imp` picker | PASS | The native Edge picker selected the real signed 599 MB media package. The dialog explicitly stated that local installation does not need network access and retained one fixed action/status area. |
| Local extensions | HTTPS source fields/cancel | PASS | HTTPS mode and its inputs were operated without installing an untrusted package. |
| Local extensions | Verify / disable / enable | PASS | Edge verification completed locally; disable and enable persisted across refresh and the capability route changed to `已停用` and back to ready. Feedback stayed on the affected extension card. |
| Local extensions | Repeat local install / uninstall | PENDING | The first 599 MB upload exposed a duplicate-submit and missing-progress defect. Both install entry points now synchronously lock and reserve one stable staging status slot, with double-click regression tests; the final Edge repeat and confirmed uninstall remain required. |

## Preferences

| Surface | Control / states operated | Result | Evidence |
|---|---|---:|---|
| Navigation | All five preference sections | PASS | Section content changed inside the stable three-entry admin shell. |
| Conversion | OCR languages | PASS | The former free-form field is now a curated eight-choice selector covering automatic detection and every supported Simplified Chinese, Traditional Chinese and English subset. The combined `zh-Hans,en` choice is regression-tested to persist as two language entries rather than one comma-containing string. |
| Conversion | OCR confidence range | PASS | Changed to 51%, saved, reloaded and persisted. |
| Remaining fields | Selects, numeric inputs, switches | PASS | Edge operated every visible selector, OCR confidence slider, source-information switch, speech language/script selectors, timeout and concurrency steppers, and allowed-host input across all five sections. Temporary changes reached a 13-field dirty state, were discarded by leaving the route, and reopening Preferences restored the persisted 70% value and disabled Save state. |
| Fixed action area | Save / saved / dirty states | PASS | The bottom action row stayed outside the internally scrolling preference content; dirty count and success remained local without floating over settings. |
| Loading | Immediate skeleton and delayed fetch | PASS | Edge route transitions were stable; delayed/stale-response behavior is additionally covered by the Web tests. |

## Diagnostics

| Surface | Control / states operated | Result | Evidence |
|---|---|---:|---|
| Diagnostics | Remediation actions | PASS | Plugin and AI-service issues link directly to their owning screen. Core PDF runtime failures no longer send users to unrelated Preferences or expose `PDFIUM_LIBRARY`; they identify an incomplete Core installation and use the page-level rerun after repair. |
| Diagnostics | 检查本机 / 检查本机与 AI 服务 | PASS | Edge ran both explicit actions. Each disabled in place while running and returned in the same header slot; no ambiguous standalone network checkbox remained. |
| Diagnostics | Failure guidance | PASS | The few-line failure cause and remediation were visible inline on each affected card; passing and skipped groups alone use disclosure controls. A release-style package with the pinned Core PDFium library reported `runtime.pdfium` healthy in CLI diagnostics; Edge then showed PDF among the ten healthy checks and did not offer a PDF plugin or Preferences detour. The same package converted `fixtures/small/pdf/titled-table.pdf` into a non-empty table-preserving Markdown artifact. |

Supporting regression evidence does not replace any pending Edge row: 27/27 administration tests cover local feedback, stable save and verification slots, curated OCR language combinations, source-context changes, nested dialog focus containment, page clamping after a diagnostic rerun removes the former last page, and the Core-only PDF repair contract without internal environment-variable instructions.

## Responsive and installed-artifact acceptance

| Requirement | Result | Evidence / next condition |
|---|---:|---|
| 1920×1080, 1365×768, 1024×768, 375×812 Edge screenshots | PENDING | Fresh final 1365×768 captures cover workbench, speech, capabilities, Preferences and diagnostics with the normal Edge renderer and zero console messages. Fresh 1024×768 captures cover workbench, speech, capabilities and Preferences; that pass found and fixed a real management-row overflow by stacking the capability heading above its controls before any action could be clipped. The 1920×1080 and 375×812 installed-binary captures remain required after merge. |
| Initial preferences exploration vs final same-size comparison | PENDING | Compare only stable sidebar, task grouping, alignment and bottom save area; never copy the six-entry IA. |
| Installed `~/.local/bin/into-md` Web and CLI | PENDING | Only after PR 2 merges; verify binary hash/provenance, restart, then repeat key flows. |
| Real remote OCR and transcription | BLOCKED | `DASHSCOPE_API_KEY` absent in the current process. |
| Genuine two-person conversational speech and diarization | PASS | Microsoft Edge imported the real 386,156-byte, 60.008-second moderator/panelist WebM with diarization and an expected count of two. Durable Web state records task `06f6463320b82ce10f691432453e0ada` as succeeded in 14,501 ms and publishes a 1,948-byte transcript containing both `Speaker 1` and `Speaker 2` across 15 monotonic timestamp ranges through 59.990 seconds, with empty diagnostics. The source is a natural CC BY 4.0 Wikimedia panel exchange, not silence, TTS, or concatenated single-speaker clips. |

## Current debug CLI real-file matrix

| Surface | Result | Evidence |
|---|---:|---|
| Three real OCR PNGs in one default-parallel batch | PASS | 3/3 succeeded in 6.92 s after `7cb3307`; the previous `transactionBusy` failure did not recur; English, mixed Chinese/English, and Simplified Chinese text were inspected. |
| Real `.doc`, `.xls`, `.ppt` in one batch | PASS | 3/3 succeeded; document text, spreadsheet row/formula order, and two-slide order with notes were inspected. Cold elapsed time was 249.64 s and is retained as a latency concern, not a performance pass. |
| Ten real local speech inputs with the current optimized CLI and media provider | PASS | 10/10 succeeded in 103.63 s with a locally signed isolated test package: WAV, CBR/VBR MP3, M4A/AAC, OGG/Opus, FLAC, WebM, magic/extension mismatch, and durations through 185.008 s. All 142 timestamp ranges were monotonic; outputs were byte-identical to the debug-host control. This does not replace the final installed-artifact pass. |
| Ten-second-class local speech cold process | PASS | Current optimized CLI converted a 113,971-byte, 9.059-second real M4A in 9.24 s and 8.10 s in two independent runs; output was byte-identical to the debug runs. The final installed binary still needs the same check. |
| Genuine two-person 60-second conversation | PASS | A CC BY 4.0 German panel segment contains a natural moderator handoff and answer, not silence, TTS, or concatenated single-speaker clips. Release CLI diarization with an expected count of two completed in 11.65 s, emitted both anonymous speaker labels over monotonic ranges and reported no diagnostics or warnings. |
| Release Web speech latency | PASS | The same real 31-second WebM completed in Edge in 9,747 ms cold and 6,788 ms immediately repeated. A real 185-second M4A completed pure transcription in 15,128 ms of durable task time, far below source duration; its 76 timestamp ranges were monotonic. The installed-binary repetition remains pending. |
| Manager-verified runtime dispatch | PASS | The plugin manager now dispatches its private authenticated snapshot directly instead of copying the complete runtime a second time. The real process-plugin E2E passes with a zero temporary-byte dispatch budget, which would reject the former duplicate runtime copy. |
| Current debug CLI unit suite | PASS | 234/234 CLI unit tests passed; the focused 24-input/eight-worker output-lease regression passed. |

# Microsoft Edge product control matrix

Date: 2026-08-23
Browser: Microsoft Edge with the supported Codex browser connection; native macOS pickers are used for local file selection.
Build under test: `target/debug/into-md` from `codex/admin-visual-pagination` unless the row says installed artifact.

`PASS` means the visible control was operated in Microsoft Edge and its resulting UI or persisted state was inspected. Unit or API checks appear only as supporting evidence. `BLOCKED` and `PENDING` are deliberately not counted as acceptance.

## Global shell

| Surface | Control / states operated | Result | Evidence |
|---|---|---:|---|
| Header | 文稿转换、语音转写、系统管理 navigation | PASS | Each link opened its named workspace; the selected administration route exposed only the three-task sub-navigation. |
| Header | Language 简体中文 → English → 简体中文 | PASS | Visible labels changed in place and restored without losing the current route. |
| Header | Theme 跟随系统、浅色、深色 | PASS | Each option changed the rendered theme and the final system setting remained selected. |
| Header | Skip to main content | PENDING | Keyboard focus/activation trace still required in the installed-artifact pass. |

## Document workbench

| Surface | Control / states operated | Result | Evidence |
|---|---|---:|---|
| Source | Native Edge file picker with real `wide.json` | PASS | File appeared in the current selection, conversion completed, result preview opened. |
| Source | Directory picker | BLOCKED | Requires unlocked macOS native picker. |
| Source | Drag and drop | PENDING | Must be executed through the normal Edge surface, not a synthetic DOM event. |
| Selection | Remove selected file | PENDING | Scheduled with the remaining native-file pass. |
| Conversion | OCR 自动、始终识别扫描内容、关闭 | PASS | All three segmented controls changed pressed state. |
| Conversion | Assets 保存到同名文件夹、直接写入 Markdown、不保存附件 | PASS | All three segmented controls changed pressed state and their local help text updated. |
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
| Import | Real 35-second M4A | BLOCKED | Requires unlocked macOS native picker. |
| Import | Real 150-second MP3 | BLOCKED | Requires unlocked macOS native picker. |
| Transcript | Preview/source/details/close | PASS | Result views and focus-safe close were operated. |
| Transcript | Rename speaker | PASS | Speaker was renamed to 主持人 and the regenerated artifact persisted the name. |
| Transcript | Cancel running task | PENDING | Needs a fresh long-running task in the installed-artifact pass. |
| Transcript | Regenerate | PASS | Result retry/regenerate control created another task. |
| Remote route | Per-upload service consent and remote transcription | BLOCKED | UI consent was exercised, but no `DASHSCOPE_API_KEY` is present, so the real remote result is not accepted. |

## Capabilities and sources

| Surface | Control / states operated | Result | Evidence |
|---|---|---:|---|
| Capability rows | Source select and switch | PASS | OCR switched to `qa-local` and then to the valid disabled route; speech/local source state remained readable. |
| Capability rows | Install local OCR | PENDING | Will be exercised from Core-only in the installed-artifact Web pass. |
| Capability rows | Verify local speech | PENDING | CLI lifecycle is covered, but the visible Edge verify button still needs the final pass. |
| AI services | Open / Escape / close / focus restore | PASS | Context dialog opened over Capabilities and Escape closed the top layer. |
| AI services | Add all fields and save `qa-local` | PASS | Vendor-neutral service persisted base URL, model/capabilities, environment-variable name, allowed host and private-network choice. No secret value was entered. |
| AI services | Edit / cancel | PASS | Existing service fields opened and cancel left persisted configuration unchanged. |
| AI services | Test connection | PASS | Failure stayed on the `qa-local` card with user-facing missing-secret/network text and no internal error code. |
| AI services | Set as capability source | PASS | OCR source change was reflected in the capability row and workbench consent. |
| AI services | Delete | BLOCKED | Requires explicit action-time approval; remote real-route cleanup remains mandatory after that test. |
| Local extensions | Open / Escape / close / focus restore | PASS | Context dialog layering and focus restoration were inspected. |
| Local extensions | Local `.imp` picker | BLOCKED | Requires unlocked macOS native picker for the Web path. |
| Local extensions | HTTPS source fields/cancel | PASS | HTTPS mode and its inputs were operated without installing an untrusted package. |
| Local extensions | Verify / disable / enable / uninstall | PENDING | CLI lifecycle matrix passed; every equivalent visible Edge action remains for the installed-artifact pass. |

## Preferences

| Surface | Control / states operated | Result | Evidence |
|---|---|---:|---|
| Navigation | All five preference sections | PASS | Section content changed inside the stable three-entry admin shell. |
| Conversion | OCR languages | PASS | Changed to `zh,en`, saved, reloaded and persisted. |
| Conversion | OCR confidence range | PASS | Changed to 51%, saved, reloaded and persisted. |
| Remaining fields | Selects, numeric inputs, switches | PENDING | Unit coverage exists; every visible Edge field needs a consolidated trace in the installed pass. |
| Sticky action area | Save / saved / dirty states | PASS | Save remained stable while switching sections; success stayed local to the action area. |
| Loading | Immediate skeleton and delayed fetch | PASS | Edge route transitions were stable; delayed/stale-response behavior is additionally covered by the Web tests. |

## Diagnostics

| Surface | Control / states operated | Result | Evidence |
|---|---|---:|---|
| Diagnostics | Remediation link | PASS | Direct link navigated to the relevant Preferences section. |
| Diagnostics | Network opt-in toggle | PENDING | Needs visible before/after execution trace. |
| Diagnostics | Run / retry | PENDING | Needs visible execution with both local and permitted-network states. |
| Diagnostics | Details expand/collapse | PENDING | Needs visible operation trace. |

## Responsive and installed-artifact acceptance

| Requirement | Result | Evidence / next condition |
|---|---:|---|
| 1920×1080, 1365×768, 1024×768, 375×812 Edge screenshots | PENDING | Earlier full matrices are retained as intermediate evidence. `final-*` captures were cropped by the locked desktop and are not acceptance; they must be replaced after unlock from the final installed binary. |
| Initial preferences exploration vs final same-size comparison | PENDING | Compare only stable sidebar, task grouping, alignment and bottom save area; never copy the six-entry IA. |
| Installed `~/.local/bin/into-md` Web and CLI | PENDING | Only after PR 2 merges; verify binary hash/provenance, restart, then repeat key flows. |
| Real remote OCR and transcription | BLOCKED | `DASHSCOPE_API_KEY` absent in the current process. |
| Genuine two-person conversational speech and diarization | BLOCKED | Existing two-source fixture and single-speaker Edge recordings are not a genuine natural conversation. A real two-person sample is still required. |

## Current debug CLI real-file matrix

| Surface | Result | Evidence |
|---|---:|---|
| Three real OCR PNGs in one default-parallel batch | PASS | 3/3 succeeded in 6.92 s after `7cb3307`; the previous `transactionBusy` failure did not recur; English, mixed Chinese/English, and Simplified Chinese text were inspected. |
| Real `.doc`, `.xls`, `.ppt` in one batch | PASS | 3/3 succeeded; document text, spreadsheet row/formula order, and two-slide order with notes were inspected. Cold elapsed time was 249.64 s and is retained as a latency concern, not a performance pass. |
| Ten real local speech inputs with the current optimized media provider | PASS | 10/10 succeeded in 132.29 s with a locally signed isolated test package: WAV, CBR/VBR MP3, M4A/AAC, OGG/Opus, FLAC, WebM, magic/extension mismatch, and durations through 185.008 s. All 142 timestamp ranges were monotonic. This does not replace the final installed-artifact pass. |
| Ten-second-class local speech cold process | PASS | Current optimized CLI converted a 113,971-byte, 9.059-second real M4A in 9.24 s and 8.10 s in two independent runs; output was byte-identical to the debug runs. The final installed binary still needs the same check. |
| Current debug CLI unit suite | PASS | 234/234 CLI unit tests passed; the focused 24-input/eight-worker output-lease regression passed. |

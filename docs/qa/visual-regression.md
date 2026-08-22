# Visual regression traceability

All captures in `docs/qa/evidence/negative/` are negative or function-only evidence. They are archived by their original basename; no capture in that directory is an approved design. Code commit `c36536e` contains the main web product restructuring; follow-up `2cf47fd` removes implementation-limit/network-policy controls from the normal workbench and makes remote AI consent local to one conversion. Runtime performance and capability-policy fixes are in `78ba923`.

The preferences exploration `exec-2525c281-c247-4d68-b677-8868226de291.png` is also not an approved specification. Its stable navigation, task-grouped rows, aligned labels/controls, and fixed action area are the only reusable qualities. The six-entry administration IA, field inventory, and folding hierarchy are excluded. The interim combined comparison is `evidence/edge/preferences-exploration-intermediate-comparison.png`; the final same-viewport installed-artifact comparison remains blocked while macOS is locked.

The old captures did not retain a reliable CSS viewport or device-pixel-ratio record, so they are not treated as pixel-diff baselines. Regression screenshots use the recorded Edge acceptance viewports (1920 × 1080, 1365 × 768, 1024 × 768, and 375 × 812), with 1365 × 768 as the primary functional comparison. This preserves real viewport behavior without inventing missing capture metadata.

## Earlier issue captures

| Archived capture | Problem that must not return | Fix / Edge evidence | Operated controls |
|---|---|---|---|
| [b437ddec](evidence/negative/codex-clipboard-b437ddec-d754-4aa8-83ce-c3cda60d889d.png) | Speech failure dominates the page or exposes internal state. | `c36536e`; `speech-1365x768.png`, recorded/imported transcript captures. | Generate, cancel, result, retry. |
| [2da97feb](evidence/negative/codex-clipboard-2da97feb-4cc2-43a3-b7c7-04b99321ee1a.png) | Speech failure is detached from the task. | Same as above; failure/result stay in the transcript slot. | Generate, task row, result close. |
| [4075d9ab](evidence/negative/codex-clipboard-4075d9ab-3f91-4aed-bc30-6e3372064bbf.png) | Meeting wording promises unsupported behavior. | `c36536e`; `recording-draft-real-speech-1365x768.png`. | Record, pause/resume, finish, save/discard. |
| [4d6aff11](evidence/negative/codex-clipboard-4d6aff11-4cc1-465f-b693-801c1bd4ea69.png) | Provider failure is global or technical. | `c36536e`; AI-service test trace in `product-control-matrix.md`. | Test connection. |
| [8106ef83](evidence/negative/codex-clipboard-8106ef83-930c-4d46-b30a-9a03be600471.png) | Provider fields and actions are misaligned. | `c36536e`; `ai-services-dialog-1365x768.png`. | Open, edit, cancel, Escape. |
| [8f4a0547](evidence/negative/codex-clipboard-8f4a0547-1735-41ef-955e-2fc0bfbf2c9f.png) | Existing service cannot be edited. | `c36536e`; contextual AI services dialog. | Edit, save/cancel. |
| [f2916048](evidence/negative/codex-clipboard-f2916048-794b-46f5-8b8c-c859c0012490.png) | Plugin install pushes or corrupts the page layout. | `c36536e`; local-extension dialog trace in `product-control-matrix.md`. | Install dialog, local/HTTPS source. |
| [98abf782](evidence/negative/codex-clipboard-98abf782-a437-4504-8860-b2b01add19ca.png) | Plugin detail exposes ungrouped internals. | `c36536e`; contextual details. | Details summary, verify. |
| [04bda3dd](evidence/negative/codex-clipboard-04bda3dd-650c-42fe-9058-147689c8a4c1.png) | Plugin destructive action is visually ambiguous. | `c36536e`; danger action and confirmation. | Disable/enable, uninstall/confirm. |
| [5c1ebe23](evidence/negative/codex-clipboard-5c1ebe23-a8c8-4e93-9171-54ef2afc4ec5.png) | Plugin dialog overflows a compact viewport. | `c36536e`; 1024 and 375 viewport matrix. | Open, scroll, Escape. |
| [d6988c05](evidence/negative/codex-clipboard-d6988c05-a378-45de-a0fc-e77dc373305a.png) | Preferences expose raw implementation keys. | `c36536e`; `preferences-1365x768.png`. | Every preference control, save. |
| [b66018e6](evidence/negative/codex-clipboard-b66018e6-15ab-45fb-ae11-bf5695e5f946.png) | Preferences have unstable loading/layout. | `c36536e`; immediate five-section skeleton tests. | Route switch, delayed fetch, save. |
| [30cda973](evidence/negative/codex-clipboard-30cda973-1a16-435a-8fd0-9cadbc5623ff.png) | Diagnostics list duplicate symptoms without a remediation task. | `c36536e`; `diagnostics-1365x768.png`. | Run, network toggle, remediation link. |
| [f9811589](evidence/negative/codex-clipboard-f9811589-ee9d-4e9f-8a5b-cde12cbb39a0.png) | Diagnostics use a large empty or global error state. | `c36536e`; grouped compact cards. | Run/retry, details. |
| [3581cd50](evidence/negative/codex-clipboard-3581cd50-49d8-467b-9c5f-563daed34791.png) | Capability cards waste space. | `c36536e`; `capabilities-1365x768.png`. | Source select, switch, install/verify. |
| [d4f79e19](evidence/negative/codex-clipboard-d4f79e19-68f4-4967-9cf4-1a09fd127e80.png) | Format support is a parallel top-level task. | `c36536e`; contextual format table. | Search, previous/next page. |
| [504ae243](evidence/negative/codex-clipboard-504ae243-f46c-47b8-bd04-08ec15931e43.png) | Large blank area follows short capability content. | `c36536e`; four-viewport capability matrix. | Capability rows. |
| [a74e72ae](evidence/negative/codex-clipboard-a74e72ae-00c7-4161-b930-d0ebf68d4b5a.png) | Pagination is absent or hidden. | `c36536e`; visible table/history paging. | Next, previous, search reset. |
| [34f4c4f0](evidence/negative/codex-clipboard-34f4c4f0-f2b4-46ba-a128-f45713686385.png) | Format status and source are hard to scan. | `c36536e`; aligned contextual table. | Search and page boundaries. |
| [66025096](evidence/negative/codex-clipboard-66025096-1ec7-44b4-a4cf-2e330a1884c9.png) | History is capped and hidden behind another large window. | `c36536e`; `workbench-1365x768.png`. | Full pagination. |
| [073259a2](evidence/negative/codex-clipboard-073259a2-6859-463f-8dac-2309969cd0b0.png) | Search does not reset or reconcile pages. | `c36536e`; history unit and Edge checks. | Search, status filter. |
| [6c942a4d](evidence/negative/codex-clipboard-6c942a4d-60c4-471d-9530-6b968efc4459.png) | File/task cards mix internal status with user feedback. | `c36536e`; task rows use localized status/stage. | Open task, retry/remove. |
| [7c4810d4](evidence/negative/codex-clipboard-7c4810d4-ddb8-49a1-a0ee-7410ec894dbc.png) | Current work and history have unclear ownership. | `c36536e`; stable current-task and history regions. | Current row, history row. |
| [64572359](evidence/negative/codex-clipboard-64572359-7a2c-42ba-b7c5-23a0c541e2ab.png) | OCR choices are internal policy names. | `c36536e`; task-language segmented controls. | Auto, always scan, off. |
| [865342cb](evidence/negative/codex-clipboard-865342cb-f44d-418e-985a-746846224799.png) | Asset-output choices do not describe the result. | `c36536e`; result-oriented labels and hints. | Folder, embed, omit. |
| [b856a516](evidence/negative/codex-clipboard-b856a516-0434-49d1-aacb-b5943ee6a442.png) | Icons imply the wrong expansion direction/action. | `c36536e`, `2cf47fd`; the unrelated advanced drawer is removed and result details use a matching drawer affordance. | Result details open/close. |
| [469f81e0](evidence/negative/codex-clipboard-469f81e0-0dad-416a-8f2d-d389cdd11e89.png) | Disabled controls do not explain why. | `c36536e`; local task feedback and selection state. | Select/remove file, start. |

## Supplementary per-image audit

| Archived capture | Recorded defect or evidence-only meaning | Fix / Edge evidence | Operated controls |
|---|---|---|---|
| [0a942cc2](evidence/negative/codex-clipboard-0a942cc2-405c-437b-b6a3-77a025de56c2.png) | Empty two-column workbench and implementation terms. | `c36536e`, `2cf47fd`; `workbench-1365x768.png`. | OCR/asset task controls; implementation-limit fields are absent. |
| [274a5e57](evidence/negative/codex-clipboard-274a5e57-933d-4363-a322-a34659ac1730.png) | Asset choices lack object/outcome meaning. | `c36536e`; result-oriented labels/hints. | All asset choices. |
| [30edeaad](evidence/negative/codex-clipboard-30edeaad-eaf1-4f05-864a-bb3ad3c6de52.png) | Empty queue/settings and English failure. | `c36536e`; current row local feedback. | Failed row, retry/details. |
| [b94b23a8](evidence/negative/codex-clipboard-b94b23a8-aa07-4c3d-a443-04efb9834f7c.png) | Same workbench defect state; not a target. | `c36536e`, `2cf47fd`; standard Edge matrix. | Task controls and task row; no advanced drawer. |
| [26ec572f](evidence/negative/codex-clipboard-26ec572f-eb57-462f-96d3-5958e2d7dfd4.png) | Unsupported plist becomes `失败 · failed`. | `c36536e`; selection filtering and readable diagnostics. | File picker, remove. |
| [74197125](evidence/negative/codex-clipboard-74197125-72bd-4ff5-a894-9e62e84fc696.png) | Duplicate unsupported-file evidence. | `c36536e`; same regression. | File picker, remove. |
| [b26bc3bd](evidence/negative/codex-clipboard-b26bc3bd-aede-4907-9aa1-73c1b301a3d7.png) | Mixed success/failure has only a batch-level error. | `c36536e`; independent file rows. | Result, retry, remove. |
| [8570857f](evidence/negative/codex-clipboard-8570857f-5a44-4e4b-a6a8-2f6008749b92.png) | Current batch and history share an internal scroll card. | `c36536e`; bounded current list plus paged history rail. | Search/filter/page/cleanup. |
| [eecde548](evidence/negative/codex-clipboard-eecde548-037f-43a1-90b4-de20d3b32830.png) | One-file card is stretched to the viewport. | `c36536e`; content-driven card height. | Select one file. |
| [44c78a36](evidence/negative/codex-clipboard-44c78a36-468d-4695-9149-ef0098ebc832.png) | Drop target is excessively tall. | `c36536e`; compact target in four viewports. | Drop target, file/directory picker. |
| [f26acf7a](evidence/negative/codex-clipboard-f26acf7a-95fe-479d-9caa-fc4fbff8d086.png) | Normal flow exposes limits, hosts, MiB, and network internals. | `2cf47fd`; task surface omits format hints, thresholds, MiB/page limits, AI routing mode, and general network policy. Remote OCR shows one per-upload consent immediately beside Start conversion. | Local task controls; remote-source consent check/uncheck. |
| [bbd99b3a](evidence/negative/codex-clipboard-bbd99b3a-0335-4440-b3ea-b6657f1ca44d.png) | Legacy debug workbench and half-screen layout. | `c36536e`; not reused. | Standard workbench matrix. |
| [0729a381](evidence/negative/codex-clipboard-0729a381-8b2a-46d5-bcf0-2807ac9b6855.png) | `准备依赖` is not an actionable capability state. | `78ba923`, `c36536e`; capability action/state. | Install/repair/recheck. |
| [4dcf35d0](evidence/negative/codex-clipboard-4dcf35d0-d629-4821-bf20-73e972459541.png) | Duplicate misleading capability state. | Same as above. | Install/repair/recheck. |
| [23178168](evidence/negative/codex-clipboard-23178168-fb05-4ab9-971d-fa90916f877e.png) | `会议纪要` promises unsupported summary/live behavior. | `c36536e`; `speech-1365x768.png`. | Record/import then submit. |
| [ba7c7bdc](evidence/negative/codex-clipboard-ba7c7bdc-ea40-4d64-a682-360556c492f2.png) | Recorded audio lacks Save; history dominates. | `c36536e`; recording draft evidence. | Save, discard, remove. |
| [affab8e0](evidence/negative/codex-clipboard-affab8e0-b6b4-4b3a-bc9a-c4bb15deecfa.png) | Transcript success only; not visual/quality approval. | Real audio matrix; `recording-real-speech-transcript-1365x768.png`. | Result preview/source. |
| [d71dc194](evidence/negative/codex-clipboard-d71dc194-00df-4cb8-a264-e31ba8cdb8c8.png) | Transcript success only; speaker coverage still required. | Real diarization and rename evidence. | Speaker name save. |
| [f0a5f22b](evidence/negative/codex-clipboard-f0a5f22b-dcc9-4f3c-a852-8e06649b21da.png) | Transcript success cannot replace multi-format E2E. | Audio format/duration evidence. | Import/generate/result. |
| [20f695e4](evidence/negative/codex-clipboard-20f695e4-02ab-46a1-80fc-9623b320d79f.png) | Result functionality only; dialog layout not approved. | `c36536e`; `workbench-json-result-1365x768.png`. | Preview/source/download/details/close. |
| [c8df603e](evidence/negative/codex-clipboard-c8df603e-1c75-425c-9b9f-2644f8deb94b.png) | OCR output leaks intermediate markup. | Real OCR semantic-result regression. | Preview/source/download. |
| [338ba4fb](evidence/negative/codex-clipboard-338ba4fb-19ce-441d-ab82-e4c49e985aea.png) | Expanded result destroys history pagination. | `c36536e`; result dialog + compact paged history. | Open/close/page. |
| [50d78471](evidence/negative/codex-clipboard-50d78471-48ff-49e1-8810-e6ae56e877fa.png) | Pin/retry/delete menu is function-only evidence. | `c36536e`; danger/confirm/focus regression. | Menu, pin, retry, delete, Escape. |
| [58505904](evidence/negative/codex-clipboard-58505904-2f23-48f1-b069-c2c3368584a9.png) | Invalid session replaces the entire shell with English error. | Existing recovery shell tests and `c36536e` stable route. | Reconnect/reload. |
| [0d440beb](evidence/negative/codex-clipboard-0d440beb-76ab-413f-adbe-3ab0f487a001.png) | Inline vendor-specific Provider form and missing pagination. | `c36536e`; neutral AI service Dialog. | Add/edit/page. |
| [c99eecdc](evidence/negative/codex-clipboard-c99eecdc-9ca8-4eb9-9e5e-345dbf999608.png) | Misaligned, clipped, Alibaba-specific fields. | `c36536e`; neutral aligned form. | All fields, cancel/Escape. |
| [f47b0e80](evidence/negative/codex-clipboard-f47b0e80-8ad3-4021-b18b-00fd8c5b4234.png) | Service card does not show supported capabilities. | `c36536e`; readable capability chips. | Test/edit/default/delete. |
| [a9cecd89](evidence/negative/codex-clipboard-a9cecd89-2829-41c9-945a-251371430756.png) | Architecture explanation replaces product state. | `c36536e`; UI state/actions only. | AI/local contextual buttons. |
| [d7871672](evidence/negative/codex-clipboard-d7871672-5dd2-48c7-b546-45ded5266db6.png) | Broken responsive styles and raw signing fields. | `c36536e`; four-viewport admin matrix. | Local extension dialog/details. |
| [e223fdde](evidence/negative/codex-clipboard-e223fdde-a4d1-42f9-9eca-16f65d15a5ff.png) | Raw configuration editor is a top-level product task. | `c36536e`; five user-task preference groups. | All preference controls/save. |
| [e3e210ac](evidence/negative/codex-clipboard-e3e210ac-a0b5-4bfb-923d-ff09178974da.png) | Settings and Provider fields overlap and expose internals. | `c36536e`; separate stable surfaces. | Preferences + Provider Dialog. |
| [62e10287](evidence/negative/codex-clipboard-62e10287-4331-4f39-8407-4faacd037210.png) | Format detection developer form is not a user task. | `c36536e`; contextual support table. | Search/page only. |
| [ccf3a194](evidence/negative/codex-clipboard-ccf3a194-4368-426b-9552-0434bbde88d3.png) | Single local API status consumes an entire page. | `c36536e`; compact header/diagnostics. | Status, diagnostics route. |

## Two clipboard-index captures not listed in sections 12.3–12.4

The handoff index contains two additional files. They are also archived and treated as negative evidence after direct inspection.

| Archived capture | Problem | Fix / Edge evidence | Operated controls |
|---|---|---|---|
| [65a3a5ed](evidence/negative/codex-clipboard-65a3a5ed-8620-4dc6-8775-893add365cec.png) | Six-entry plugin page exposes raw `conflict` and oversized cards; it is not an approved IA. | `78ba923`, `c36536e`; three-entry admin and readable local feedback. | Verify, disable/enable, uninstall. |
| [8e54d1c4](evidence/negative/codex-clipboard-8e54d1c4-aed1-42d7-bc9b-d2ce21325df2.png) | Cropped form defaults to DashScope URL/key and is not vendor neutral. | `c36536e`; `ai-services-dialog-1365x768.png`. | Add form fields, cancel/Escape. |

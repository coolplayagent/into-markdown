# Current-source audio package matrix

Date: 2026-08-23

This is evidence for the current source and locally signed test package. It is not the final installed
release artifact and does not replace the post-merge `~/.local/bin/into-md` black-box pass.

## Provenance

- Branch head during the optimized full-matrix run: `c4b2f56`.
- Host CLI: current optimized `into-md`, SHA-256
  `02b51fa28622c0aee2ed2a45ee367f69a69a73603789c1f6e4ad8157ccd4997c`.
- Media provider: current source, optimized `--release --features metal` build, SHA-256
  `1cdf9ee5bfcd21dfcd9f67c92f54ec70f74a593bcf29722541619e69a72237d7`.
- Test package: `official.media.whisper`, SHA-256
  `ff6bab7a3bae49635ef0e8bdd37d0976e5493f483ec78f7e390f2d45c0fd1d3c`.
- Local test signer: `official.into-markdown`, public-key fingerprint
  `cf103866e6104b337df2c960c37ae68dcc00edf084c6ed2eb26e8d7d58fab053`.
- Isolated user-data root: `/private/tmp/into-md-media-source-e2e.4xsIru`.
- Installed provider, ONNX worker and FFmpeg modes were all inspected as `0500` before conversion.
- Machine report: `docs/qa/evidence/runtime/audio-current-release-report.json`, copied byte-for-byte
  from `/private/tmp/into-md-audio-current-full-release/report.json`, SHA-256
  `18c0b930396a80f0503ca46aa834633c8745f7b0c2a495f14ea1e577a3f81933`.

The test package reused the previously authenticated model, FFmpeg, ONNX and license resources, replaced
the media provider with the current optimized build, regenerated both signed inventories, restored the
declared helper executable modes, and was installed through the public plugin transaction. This isolates
the current provider source from the stale provider executable in the earlier signed test package without
claiming a complete release assembly.

## Regression sequence

1. The previous signed test package completed 6/10 inputs and failed all four recordings at or above about
   30 seconds with `Whisper token text was not valid UTF-8`. Report:
   `/private/tmp/into-md-audio-rerun.hDuyuw/report.json`.
2. The stale installed provider contained that old error string. The current debug and optimized providers
   did not contain it and instead contained the bounded invalid-byte recovery path from `5e80d59`.
3. An intermediate mixed package failed before transcription with `ffmpeg-lgpl: workerLaunch`. Inspection
   showed that extracting the old package with the system ZIP tool had removed executable modes from
   FFmpeg and the ONNX worker; the regenerated `plugin.json` had therefore correctly declared them
   non-executable. That package is rejected as product evidence.
4. After restoring the helper executable modes and regenerating the signed package, a 31-second WebM
   probe passed. The other three previously failing long inputs then passed 3/3.
5. A debug-host uniform matrix passed 10/10 in one `--jobs 1` invocation in 132.29 seconds.
6. The final optimized-host uniform matrix below passed 10/10 in 103.63 seconds. All ten Markdown
   artifacts were byte-identical to the debug-host results.

## Uniform real-audio matrix

| Input | Container / codec evidence | Bytes | Duration | Timed segments | Last segment end | Result |
|---|---|---:|---:|---:|---:|---:|
| `long-real.aac` | ADTS AAC-LC | 997,481 | 61.571 s | 33 | 60.500 s | PASS |
| `magic-mismatch.mp3` | ISO media AAC/M4A magic with `.mp3` name | 113,971 | 9.059 s | 1 | 6.400 s | PASS |
| `very-long-real.webm` | WebM | 1,973,746 | 185.008 s | 75 | 178.800 s | PASS |
| `long-real-vbr.mp3` | MPEG Layer III VBR | 455,037 | 31.140 s | 11 | 29.510 s | PASS |
| `long-real.webm` | WebM | 501,441 | about 31 s | 12 | 29.680 s | PASS |
| `medium-real-aac.m4a` | ISO media AAC/M4A | 113,971 | 9.059 s | 1 | 6.400 s | PASS |
| `medium-real-opus.ogg` | Ogg Opus | 111,231 | 13.326 s | 5 | 13.320 s | PASS |
| `medium-real.flac` | 24-bit FLAC | 796,136 | 10.920 s | 2 | 10.680 s | PASS |
| `short-real-cbr.mp3` | MPEG Layer III CBR | 73,773 | 4.560 s | 1 | 2.580 s | PASS |
| `short-real-pcm24.wav` | 24-bit PCM WAV | 890,022 | 6.180 s | 1 | 5.480 s | PASS |

All ten Markdown artifacts were non-empty. A separate parser checked all 142 timestamp ranges: every
range satisfied `start < end`, and every next range started at or after the prior range ended. The
extension/magic mismatch was detected from bytes rather than accepted only by filename.

## Ten-second-class cold-process timing

The 113,971-byte, 9.059-second `medium-real-aac.m4a` fixture was then converted twice in separate
processes with the same installed test package and isolated user-data root.

| Host CLI | SHA-256 | Run 1 | Run 2 | Output SHA-256 |
|---|---|---:|---:|---|
| Current debug CLI | `f580d8a48a4e9c4208735c2677704774e790d39b67adf170c21e495a3d3866fb` | 34.28 s | 33.36 s | `afa03a7cff0ba4f1dd99771d002116b81c7da7946f0d3a5fea906aa1f736e1ce` |
| Current optimized CLI | `02b51fa28622c0aee2ed2a45ee367f69a69a73603789c1f6e4ad8157ccd4997c` | 9.24 s | 8.10 s | `afa03a7cff0ba4f1dd99771d002116b81c7da7946f0d3a5fea906aa1f736e1ce` |

A one-second sample taken eight seconds into the debug run captured all 713 main-thread samples under
`PluginManager::process_manifest -> copy_verified_runtime_tree -> SHA-256`, with the digest in the
unoptimized `sha2::soft` implementation and no provider process launched yet. The optimized CLI kept the
same authenticated-copy boundary and produced byte-identical transcript output while bringing both cold
process runs below ten seconds. These timings prove the current optimized build behavior; the final
installed binary still needs the same measurement after PR merge and reinstall.

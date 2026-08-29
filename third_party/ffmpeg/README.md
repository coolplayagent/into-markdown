# Audited FFmpeg runtime

FFmpeg does not publish official platform binaries. This repository therefore
pins the official 8.1.2 source tarball and detached signature, then builds one
minimal, deterministic-configuration CLI per supported target in the scheduled
or manually dispatched `ffmpeg-artifact-audit` workflow. Compiler output is recorded; byte-for-byte
reproducibility across toolchain changes is not claimed.
Ordinary Cargo and Bazel builds do not download or execute FFmpeg.

Normal product releases do not compile FFmpeg. They acquire the reviewed
per-platform archives from this repository's reusable `runtime-assets`
Release. `runtime-assets.json` pins each archive URL, byte count and SHA-256,
plus the byte count and SHA-256 of every member. `tools/ffmpeg_runtime.py`
applies the shared release downloader and rejects links, duplicates, extra
entries and content that differs from that authority. Publishing replacement
assets remains an explicit maintainer action after the source audit workflow;
product release jobs never overwrite this reusable authority.

The exact configuration disables GPL, nonfree, networking, autodetection,
devices, and every component before allowlisting the CLI, audio decoders,
container demuxers, PCM encoder/muxer, and resampling filters needed for WAV,
MP3, M4A, FLAC, OGG and common MOV/MP4/Matroska/AVI/MPEG-TS/WebM video inputs.
No external codec library is linked. `tools/ffmpeg-build-audit.sh` verifies the
source hash, signature hash, license text, `-buildconf`, enabled component lists,
binary architecture/imports, positive fixtures, and negative inputs. It emits
an authority JSON containing the actual executable hash and size. The audit
executes origin-overwrite and rename-replacement races only against disposable
copies, then proves the upload binary's hash and size did not change and rejects
any unexpected file in the upload directory.

The four codec samples in `fixtures.json` come from FFmpeg's public samples
server and are downloaded only as transient inputs to the manual networked CI
job. Their individual authorship and redistribution permission are not
documented, so redistribution is treated as prohibited: they are never copied
to the artifact directory. Each artifact contains only the audited executable,
its authority, the upstream LGPL text, and an inventory that records this
exclusion.

The production invocation fixes decoder, encoder, and filter parallelism to one
thread. Unix applies address-space, data, CPU, file-size, descriptor, and
core limits; the audited FFmpeg CLI does not create descendant processes. On
macOS the 2 TiB `RLIMIT_AS` ceiling accommodates dyld's sparse virtual mappings
and neither the address-space nor data ceiling is described as a physical-memory
cap. The parent separately polls Darwin physical footprint/resident bytes and
kills the child above the caller-selected limit (default 512 MiB, compiled
maximum 2 GiB). Windows places the worker in a kill-on-close Job with process-memory
and one-active-process limits. Callers may lower, but never raise, compiled
platform ceilings. PCM grows in small fallible chunks under a 512 MiB protocol
ceiling, and the returned `PcmAudio` exposes only a read-only slice while
retaining its request memory reservation for the sample buffer's lifetime.

The workflow is configured for all four targets. Repository evidence in this
change records an executed macOS ARM64 build and smoke only; the other three
target builds remain evidence produced by the manual CI workflow, not a claim
that they ran on the author's host.

Production packages must authenticate and embed that generated authority next
to the matching CI artifact. `FfmpegRuntime::load` fails closed if either is
missing or differs; it never falls back to a system `ffmpeg` or `PATH` lookup.

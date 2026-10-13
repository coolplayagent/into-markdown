# Runtime model assets

The immutable small model inputs are stored in the repository so release jobs
do not depend on third-party model hosts. Their upstream locations, exact byte
counts, and SHA-256 digests remain authoritative in the platform and macOS
release authority files.

The 3D-Speaker model is acquired through the existing SHA-256-pinned Bazel
download authority in `third_party/licenses/downloads.json`. The `speaker_model`
target reproduces `models/3dspeaker_eres2net_base.onnx` in Bazel runfiles; native
release acquisition uses the identical upstream URL, byte count and SHA-256 in
both release authorities. Its 39,593,761 bytes are unchanged. This avoids keeping
a binary larger than the verification snapshot file limit in the source tree.

The Whisper small model is too large for ordinary GitHub Git storage. It is
hosted by this repository's version-independent `runtime-assets` Release and
is still verified against the upstream digest before packaging.

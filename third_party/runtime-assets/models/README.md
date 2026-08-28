# Runtime model assets

These immutable small model inputs are stored in the repository so release jobs
do not depend on third-party model hosts. Their upstream locations, exact byte
counts, and SHA-256 digests remain authoritative in the platform and macOS
release authority files.

The Whisper small model is too large for ordinary GitHub Git storage. It is
hosted by this repository's version-independent `runtime-assets` Release and
is still verified against the upstream digest before packaging.

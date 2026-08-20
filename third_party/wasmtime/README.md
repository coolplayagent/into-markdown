# Wasmtime runtime authority

Issue #43 uses the exact crates, checksums, upstream tag/commit, feature set, and
license text in `source.json`. The host enables the component model, Cranelift,
fuel, and epoch interruption. It exposes WASI Preview 2 command interfaces with
manifest-scoped authority; it does not enable Wasmtime cache, profiling, pooling,
threads, component async execution, or a system/native runtime download.

An upgrade must renew the Cargo/Bazel dependency graph, unsafe/build-script
review, WASI capability tests, four-host-target matrix, and license/SBOM audit.

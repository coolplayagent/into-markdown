# WASI Preview 2 test guest

This repository-authored, dependency-free Rust program is the source authority
for `tests/fixtures/guest.component.wasm`. It is deliberately small: stdin and
stdout exercise the real `wasi:cli/run` command interface, while request labels
select successful IR, invalid IR, bounded-output failure, or deterministic fuel
exhaustion.

Rebuild with the repository Rust toolchain:

```text
cargo +1.97.1 build --manifest-path crates/plugin-wasi/guest-fixture/Cargo.toml \
  --target wasm32-wasip2 --release --locked
```

The authority JSON binds the Rust compiler, target, source digest, component
digest, and exact command. Network access is neither required nor permitted by
this fixture.

# ADR 0004：Bazel 为权威构建，Cargo 保持开发兼容

状态：已接受

Bazel 9.2.0、Bzlmod、`rules_rust` 0.73.0、Rust 1.97.1 和 Edition 2024
共同构成发布与 CI 构建环境。Cargo 清单和 `Cargo.lock` 为 crate_universe
提供依赖解析，并支持 rust-analyzer 与本地 `cargo check`。

第一方依赖必须在 BUILD 文件中显式声明，架构边界由 Bazel visibility 强制执行。
Cargo 检查必须通过，但不能替代 Bazel 验证。

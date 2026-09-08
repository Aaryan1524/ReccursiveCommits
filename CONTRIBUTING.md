# Contributing

## Prerequisites

- Git with the required plumbing capabilities (Git 2.38 or newer is the expected baseline)
- Rust 1.97.0 or newer from the stable channel selected by `rust-toolchain.toml`
- macOS or Linux for the current development targets

## Local checks

Run the same checks used by continuous integration:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo build --workspace --locked
```

`Cargo.lock` is committed because this workspace produces applications. Add external
dependencies at the workspace level when multiple crates share them, keep features
minimal, and explain dependencies that introduce native libraries or background
runtime behavior in the pull request.

The crate boundaries are deliberate:

- `core` owns domain rules and contains no transport or persistence code.
- `store` owns durable local state and depends on domain types.
- `protocol` owns versioned local API messages.
- `daemon` is the only background owner of mutable queue state.
- `cli` is a client of the local API and never writes the store directly.

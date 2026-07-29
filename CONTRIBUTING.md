# Contributing to Aster

Thanks for helping improve Aster.

## Before opening a change

- Search existing issues and pull requests.
- For substantial behavior or interface changes, open an issue first so the
  design can be discussed.
- Keep changes focused and include tests for observable behavior.

## Development setup

Aster requires Rust 1.85 or newer.

```console
git clone https://github.com/ArchAstro/aster.git
cd aster
cargo build --locked
cargo test --locked --all-targets --all-features
```

Before opening a pull request, run the same core checks as CI:

```console
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
cargo test --locked --all-targets --all-features
cargo audit
```

Install `cargo-audit` with `cargo install cargo-audit --locked` if needed.

## Pull requests

Explain the problem, the chosen solution, and how you verified it. Update
documentation when behavior or configuration changes. By contributing, you
agree that your work is licensed under the repository's MIT license.

Be respectful and follow the [Code of Conduct](CODE_OF_CONDUCT.md).

.PHONY: all build build-release install test test-all clean release-check setup notices

# Default target
all: build

# Debug build
build:
	cargo build

# Release build
build-release:
	cargo build --release

# Install to ~/.cargo/bin (or CARGO_HOME/bin)
install: build-release
	cargo install --path .

# Validate a release candidate. Tagging and pushing stay explicit human actions.
release-check:
	cargo fmt --all -- --check
	cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
	RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --all-features
	cargo test --locked --workspace --all-targets --all-features
	cargo audit

# Regenerate THIRD_PARTY_NOTICES.md (needs: cargo install --locked cargo-about --features cli)
notices:
	cargo about generate --workspace --locked --fail about.hbs -o THIRD_PARTY_NOTICES.md

# Run all tests
test:
	cargo test

# Run the complete test suite.
test-all:
	cargo test --all-targets --all-features

# Clean build artifacts
clean:
	cargo clean
	rm -rf .aster/

# Format code
fmt:
	cargo fmt

# Lint
lint:
	cargo clippy --all-targets --all-features -- -D warnings

# Check (fast compile check without codegen)
check:
	cargo check

# Setup development environment (install git hooks)
setup:
	cp scripts/pre-commit .git/hooks/pre-commit
	chmod +x .git/hooks/pre-commit
	@echo "Git hooks installed"

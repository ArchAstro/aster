.PHONY: all build install test clean release

# Default target
all: build

# Debug build
build:
	cargo build

# Release build
release:
	cargo build --release

# Install to ~/.cargo/bin (or CARGO_HOME/bin)
install: release
	cargo install --path .

# Run all tests
test:
	cargo test

# Run tests including ignored (monorepo integration tests)
test-all:
	cargo test -- --include-ignored

# Clean build artifacts
clean:
	cargo clean
	rm -rf .aster/

# Format code
fmt:
	cargo fmt

# Lint
lint:
	cargo clippy -- -D warnings

# Check (fast compile check without codegen)
check:
	cargo check

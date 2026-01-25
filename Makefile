.PHONY: all build build-release install test clean release setup

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

# Release: update version, commit, tag, and push
# Usage: make release VERSION=0.2.0
release:
ifndef VERSION
	$(error VERSION is required. Usage: make release VERSION=0.2.0)
endif
	@echo "Releasing version $(VERSION)..."
	sed -i '' 's/^version = ".*"/version = "$(VERSION)"/' Cargo.toml
	cargo check
	git add Cargo.toml Cargo.lock
	git commit -m "chore: bump version to $(VERSION)"
	git tag -a "v$(VERSION)" -m "Release v$(VERSION)"
	git push origin main
	git push origin "v$(VERSION)"
	@echo "Released v$(VERSION)"

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

# Setup development environment (install git hooks)
setup:
	cp scripts/pre-commit .git/hooks/pre-commit
	chmod +x .git/hooks/pre-commit
	@echo "Git hooks installed"

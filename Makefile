.PHONY: setup check test lint

setup:
	git config core.hooksPath .githooks
	@echo "pre-commit hook activated"

check:
	cargo check --workspace --all-targets

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

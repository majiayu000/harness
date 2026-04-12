.PHONY: setup check test lint

setup:
	git config core.hooksPath .githooks
	@echo "pre-commit hook activated"

check:
	cargo check --workspace --all-targets

lint:
	cargo fmt --all -- --check
	RUSTFLAGS="-Dwarnings" cargo clippy --workspace --all-targets

test:
	cargo test --workspace

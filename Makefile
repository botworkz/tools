.PHONY: fmt clippy test coverage check

fmt:
	cargo fmt --all

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

coverage:
	cargo tarpaulin

check: fmt clippy test

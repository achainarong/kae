.DEFAULT_GOAL := help

CARGO ?= cargo
ARGS ?=

.PHONY: help build release install run fmt fmt-check lint test check

help:
	@printf '%s\n' \
		'Kae development commands' \
		'' \
		'  make build       Build the debug binary' \
		'  make release     Build the optimized binary' \
		'  make install     Install kae into the Cargo bin directory' \
		'  make run         Run kae (pass arguments with ARGS="-C /repo")' \
		'  make fmt         Format Rust code' \
		'  make fmt-check   Check Rust formatting without changing files' \
		'  make lint        Run Clippy with warnings treated as errors' \
		'  make test        Run all tests' \
		'  make check       Check formatting, lint, and tests'

build:
	$(CARGO) build --locked

release:
	$(CARGO) build --release --locked

install:
	$(CARGO) install --path . --locked

run:
	$(CARGO) run --locked -- $(ARGS)

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all --check

lint:
	$(CARGO) clippy --locked --all-targets -- -D warnings

test:
	$(CARGO) test --locked

check: fmt-check lint test

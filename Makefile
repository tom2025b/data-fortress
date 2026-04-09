# Makefile — Data Fortress
# -------------------------
# Common development tasks collected in one place.
# Run any target with: make <target>
# List all targets with: make help
#
# Why a Makefile?
#   `cargo` handles Rust builds, but this project also has a Python dashboard,
#   integration tests, and install steps. A Makefile gives one entry point for
#   all of them, and the tab-completion in most shells makes it discoverable.

# ── Variables ─────────────────────────────────────────────────────────────────

# Name of the compiled Rust binary (matches [package] name in Cargo.toml).
BINARY      := data-fortress

# Cargo build output directories.
RELEASE_BIN := target/release/$(BINARY)
DEBUG_BIN   := target/debug/$(BINARY)

# Dashboard entry point.
DASHBOARD   := dashboard/app.py

# Installation prefix — binary goes to ~/bin (on PATH for most users).
INSTALL_DIR := $(HOME)/bin

# Python interpreter — prefer python3 explicitly.
PYTHON      := python3

# Default target: what runs when you type `make` with no arguments.
.DEFAULT_GOAL := help

# .PHONY tells Make that these targets are not files.
# Without this, Make would skip a target if a file with that name existed.
.PHONY: help build build-release test test-unit test-integration \
        dashboard install clean lint fmt check doc

# ── Help ──────────────────────────────────────────────────────────────────────

# Print a list of available targets and their descriptions.
# The sed command extracts lines with ## comments after the target name.
help:
	@echo "Data Fortress — available make targets:"
	@echo ""
	@sed -n 's/^##//p' $(MAKEFILE_LIST) | column -t -s ':' | sed -e 's/^/ /'
	@echo ""

## build         : Build a debug binary (fast compile, slower runtime)
build:
	cargo build

## build-release : Build an optimised release binary (slow compile, fast runtime)
build-release:
	cargo build --release
	@echo "Binary at: $(RELEASE_BIN)"

# ── Testing ───────────────────────────────────────────────────────────────────

## test          : Run all tests (unit + integration)
test:
	cargo test

## test-unit     : Run only unit tests (faster — skips integration tests)
test-unit:
	cargo test --lib

## test-integration : Run only integration tests in tests/
test-integration:
	cargo test --test '*'

## test-verbose  : Run all tests and print output even for passing tests
test-verbose:
	cargo test -- --nocapture

# ── Code quality ──────────────────────────────────────────────────────────────

## lint          : Run Clippy (Rust linter) with all warnings as errors
lint:
	cargo clippy -- -D warnings

## fmt           : Auto-format all Rust source files with rustfmt
fmt:
	cargo fmt

## fmt-check     : Check formatting without changing files (good for CI)
fmt-check:
	cargo fmt -- --check

## check         : Fast type-check without producing a binary (cargo check)
check:
	cargo check

# ── Documentation ─────────────────────────────────────────────────────────────

## doc           : Build and open Rust API docs in the browser
doc:
	cargo doc --no-deps --open

# ── Dashboard ─────────────────────────────────────────────────────────────────

## dashboard     : Start the Streamlit dashboard (builds debug binary first)
dashboard: build
	@echo "Starting dashboard at http://localhost:8501"
	streamlit run $(DASHBOARD)

## dashboard-release : Start the dashboard backed by the release binary
dashboard-release: build-release
	streamlit run $(DASHBOARD)

## pip-install   : Install Python dashboard dependencies from requirements.txt
pip-install:
	$(PYTHON) -m pip install -r dashboard/requirements.txt

# ── Installation ──────────────────────────────────────────────────────────────

## install       : Build release binary and copy it to ~/bin
install: build-release
	@mkdir -p $(INSTALL_DIR)
	cp $(RELEASE_BIN) $(INSTALL_DIR)/$(BINARY)
	@echo "Installed to $(INSTALL_DIR)/$(BINARY)"
	@echo "Make sure $(INSTALL_DIR) is on your PATH."

## uninstall     : Remove the installed binary from ~/bin
uninstall:
	rm -f $(INSTALL_DIR)/$(BINARY)
	@echo "Removed $(INSTALL_DIR)/$(BINARY)"

# ── Cleanup ───────────────────────────────────────────────────────────────────

## clean         : Remove all Cargo build artefacts (target/)
clean:
	cargo clean

## clean-pyc     : Remove Python bytecode caches (__pycache__, *.pyc)
clean-pyc:
	find dashboard -type d -name '__pycache__' -exec rm -rf {} + 2>/dev/null || true
	find dashboard -name '*.pyc' -delete 2>/dev/null || true

## clean-all     : Remove build artefacts and Python caches
clean-all: clean clean-pyc

# ── Quick smoke-test ──────────────────────────────────────────────────────────

## smoke         : Build release + run --help to verify the binary works
smoke: build-release
	$(RELEASE_BIN) --help

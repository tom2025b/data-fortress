#!/usr/bin/env bash
# scripts/build-release.sh
# -------------------------
# Build the data-fortress Rust binary in release mode.
#
# Release mode differences from debug:
#   - Optimisation level 3 (set in Cargo.toml [profile.release])
#   - Link-Time Optimisation (lto = "thin") — reduces binary size
#   - Symbol stripping (strip = "symbols") — removes debug symbols
#   - panic = "abort" — smaller binary, no stack unwinding overhead
#
# Usage:
#   ./scripts/build-release.sh
#   ./scripts/build-release.sh --open   # Open the output directory in the file manager

# ── Strict mode ───────────────────────────────────────────────────────────────
# -e  Exit immediately if any command exits with a non-zero status.
# -u  Treat unset variables as errors (prevents silent typos like $DIRECOTRY).
# -o pipefail  A pipe fails if any command in it fails (not just the last one).
set -euo pipefail

# ── Colour helpers ────────────────────────────────────────────────────────────
# These escape sequences colour terminal output.
# \033[0m  resets to default colour.
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'  # No Colour (reset)

info()    { echo -e "${GREEN}[build]${NC} $*"; }
warn()    { echo -e "${YELLOW}[warn]${NC}  $*"; }
error()   { echo -e "${RED}[error]${NC} $*" >&2; }

# ── Locate project root ───────────────────────────────────────────────────────
# SCRIPT_DIR resolves to the directory containing this script, regardless of
# where the user runs it from. $0 is the script path; dirname strips the filename.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The project root is one level up from scripts/.
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

info "Project root: $PROJECT_ROOT"
cd "$PROJECT_ROOT"

# ── Verify Rust toolchain ─────────────────────────────────────────────────────
# `command -v` checks if a program exists on PATH without running it.
if ! command -v cargo &>/dev/null; then
    error "cargo not found. Install Rust from https://rustup.rs/"
    exit 1
fi

# Print the active toolchain so the build is reproducible / auditable.
info "Rust toolchain: $(rustc --version)"
info "Cargo:          $(cargo --version)"

# ── Build ─────────────────────────────────────────────────────────────────────
info "Running: cargo build --release"
echo ""  # Blank line before Cargo's own output

# `time` measures wall-clock duration of the build command.
# The build artefacts land in target/release/ automatically.
time cargo build --release

echo ""  # Blank line after Cargo's output

# ── Report ────────────────────────────────────────────────────────────────────
BINARY="$PROJECT_ROOT/target/release/data-fortress"

if [[ -f "$BINARY" ]]; then
    # `du -sh` reports disk usage in a human-readable format (-h) summarised (-s).
    SIZE=$(du -sh "$BINARY" | cut -f1)
    info "Build succeeded!"
    info "Binary:  $BINARY"
    info "Size:    $SIZE"
else
    error "Build appeared to succeed but binary not found at: $BINARY"
    exit 1
fi

# ── Optional: open output directory ──────────────────────────────────────────
if [[ "${1:-}" == "--open" ]]; then
    # `xdg-open` is the standard Linux way to open a directory in the file manager.
    xdg-open "$PROJECT_ROOT/target/release/" 2>/dev/null || true
fi

info "Done. Run with: ./target/release/data-fortress --help"

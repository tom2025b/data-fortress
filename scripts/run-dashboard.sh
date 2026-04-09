#!/usr/bin/env bash
# scripts/run-dashboard.sh
# -------------------------
# Start the Streamlit dashboard, building the Rust binary first if needed.
#
# What this script does:
#   1. Checks that the Rust binary exists (builds debug if missing)
#   2. Activates the Python virtual environment at dashboard/.venv
#      (or falls back to the system Python if no venv exists)
#   3. Verifies Streamlit is installed
#   4. Launches `streamlit run dashboard/app.py`
#
# Usage:
#   ./scripts/run-dashboard.sh
#   ./scripts/run-dashboard.sh --release   # Use release binary (faster scans)
#   ./scripts/run-dashboard.sh --port 8502 # Run on a different port

set -euo pipefail

# ── Colour helpers ────────────────────────────────────────────────────────────
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

info()  { echo -e "${GREEN}[dashboard]${NC} $*"; }
warn()  { echo -e "${YELLOW}[warn]${NC}      $*"; }
error() { echo -e "${RED}[error]${NC}     $*" >&2; }

# ── Locate project root ───────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

# ── Parse flags ──────────────────────────────────────────────────────────────
USE_RELEASE=false
PORT=8501
ARGS=()

# Iterate over all positional arguments.
while [[ $# -gt 0 ]]; do
    case "$1" in
        --release)
            USE_RELEASE=true
            shift
            ;;
        --port)
            # $2 is the value following --port; shift twice to consume both.
            PORT="$2"
            shift 2
            ;;
        *)
            # Pass any unrecognised flags straight through to streamlit.
            ARGS+=("$1")
            shift
            ;;
    esac
done

# ── Step 1: Ensure the binary exists ─────────────────────────────────────────
DEBUG_BIN="$PROJECT_ROOT/target/debug/data-fortress"
RELEASE_BIN="$PROJECT_ROOT/target/release/data-fortress"
SYSTEM_BIN="$(command -v data-fortress 2>/dev/null || true)"

if [[ "$USE_RELEASE" == true ]]; then
    if [[ ! -f "$RELEASE_BIN" ]]; then
        info "Release binary not found — building now…"
        bash "$SCRIPT_DIR/build-release.sh"
    fi
    info "Using release binary: $RELEASE_BIN"
else
    # Use system binary > debug build > trigger a debug build.
    if [[ -n "$SYSTEM_BIN" ]]; then
        info "Using system binary: $SYSTEM_BIN"
    elif [[ -f "$DEBUG_BIN" ]]; then
        info "Using debug binary: $DEBUG_BIN"
    else
        info "Debug binary not found — building now (use --release for optimised build)…"
        cargo build 2>&1
        info "Debug binary built: $DEBUG_BIN"
    fi
fi

# ── Step 2: Activate Python environment ──────────────────────────────────────
VENV_DIR="$PROJECT_ROOT/dashboard/.venv"

if [[ -d "$VENV_DIR" ]]; then
    info "Activating virtual environment: dashboard/.venv"
    # shellcheck source=/dev/null
    source "$VENV_DIR/bin/activate"
else
    warn "No virtual environment found at dashboard/.venv"
    warn "Using system Python. Run ./scripts/install.sh to set up a venv."
fi

# ── Step 3: Verify Streamlit is installed ─────────────────────────────────────
if ! command -v streamlit &>/dev/null; then
    error "streamlit not found."
    error "Install it with: pip install -r dashboard/requirements.txt"
    error "Or run: ./scripts/install.sh"
    exit 1
fi

STREAMLIT_VERSION=$(streamlit version 2>/dev/null | head -n1 || echo "unknown")
info "Streamlit: $STREAMLIT_VERSION"

# ── Step 4: Launch the dashboard ─────────────────────────────────────────────
info "Starting dashboard on http://localhost:${PORT}"
info "Press Ctrl+C to stop."
echo ""

# Build the Streamlit command.
# --server.port    overrides the default 8501.
# --server.headless=true prevents Streamlit from opening the browser automatically.
# "${ARGS[@]+"${ARGS[@]}"}" safely expands an array that may be empty.
streamlit run \
    "$PROJECT_ROOT/dashboard/app.py" \
    --server.port "$PORT" \
    --server.headless true \
    ${ARGS[@]+"${ARGS[@]}"}

#!/usr/bin/env bash
# scripts/install.sh
# -------------------
# Build the release binary and install it to ~/bin so it's available system-wide.
#
# What this script does:
#   1. Builds the release binary (calls build-release.sh)
#   2. Creates ~/bin if it doesn't exist
#   3. Copies the binary to ~/bin/data-fortress
#   4. Installs Python dashboard dependencies into a venv
#   5. Prints a reminder to add ~/bin to PATH if it isn't already
#
# Usage:
#   ./scripts/install.sh
#   ./scripts/install.sh --no-dashboard   # Skip Python venv setup

set -euo pipefail

# ── Colour helpers ────────────────────────────────────────────────────────────
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

info()  { echo -e "${GREEN}[install]${NC} $*"; }
warn()  { echo -e "${YELLOW}[warn]${NC}   $*"; }
error() { echo -e "${RED}[error]${NC}  $*" >&2; }

# ── Locate project root ───────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

# Parse flags.
SKIP_DASHBOARD=false
for arg in "$@"; do
    [[ "$arg" == "--no-dashboard" ]] && SKIP_DASHBOARD=true
done

# ── Step 1: Build the release binary ─────────────────────────────────────────
info "Building release binary…"
bash "$SCRIPT_DIR/build-release.sh"

# ── Step 2: Install binary to ~/bin ──────────────────────────────────────────
INSTALL_DIR="$HOME/bin"
BINARY_SRC="$PROJECT_ROOT/target/release/data-fortress"
BINARY_DST="$INSTALL_DIR/data-fortress"

# `mkdir -p` creates ~/bin and any missing parent directories without error.
mkdir -p "$INSTALL_DIR"

# `cp` copies the compiled binary; `-f` overwrites an existing installation.
cp -f "$BINARY_SRC" "$BINARY_DST"
info "Installed binary → $BINARY_DST"

# ── Step 3: PATH check ────────────────────────────────────────────────────────
# Check whether ~/bin is already on the user's PATH.
# The colon-delimited PATH variable is searched for the exact string "$HOME/bin".
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
    warn "$INSTALL_DIR is not in your PATH."
    warn "Add this line to your ~/.bashrc or ~/.zshrc:"
    warn ""
    warn '    export PATH="$HOME/bin:$PATH"'
    warn ""
    warn "Then run: source ~/.bashrc   (or open a new terminal)"
fi

# ── Step 4: Python dashboard dependencies ─────────────────────────────────────
if [[ "$SKIP_DASHBOARD" == false ]]; then
    info "Setting up Python dashboard environment…"

    # Check that python3 is available.
    if ! command -v python3 &>/dev/null; then
        warn "python3 not found — skipping dashboard setup."
        warn "Install Python 3.10+ and re-run, or use: ./scripts/run-dashboard.sh"
    else
        VENV_DIR="$PROJECT_ROOT/dashboard/.venv"

        # Create the virtual environment only if it doesn't already exist.
        # A venv is an isolated Python environment: its own pip, site-packages.
        if [[ ! -d "$VENV_DIR" ]]; then
            info "Creating virtual environment at dashboard/.venv"
            python3 -m venv "$VENV_DIR"
        else
            info "Virtual environment already exists at dashboard/.venv"
        fi

        # Activate the venv so pip installs into it, not the system Python.
        # shellcheck source=/dev/null   — tells shellcheck not to analyse this.
        # shellcheck source=/dev/null
        source "$VENV_DIR/bin/activate"

        info "Installing Python dependencies…"
        # --quiet suppresses verbose pip output.
        pip install --quiet --upgrade pip
        pip install --quiet -r "$PROJECT_ROOT/dashboard/requirements.txt"

        info "Dashboard dependencies installed."
        deactivate  # Exit the virtual environment
    fi
fi

# ── Done ──────────────────────────────────────────────────────────────────────
echo ""
info "Installation complete!"
echo ""
echo "  Binary:    data-fortress --help"
echo "  Dashboard: ./scripts/run-dashboard.sh"
echo ""

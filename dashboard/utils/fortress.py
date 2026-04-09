"""
utils/fortress.py
-----------------
Subprocess wrapper for calling the `data-fortress` Rust binary.

This module is the ONLY place in the dashboard that shells out to the binary.
It handles: finding the binary, building the command, capturing JSON output,
and surface-level error handling.

All functions return parsed Python dicts/lists (never raw strings) so callers
can work with structured data immediately.

Why shell out instead of using a Python library?
  The Rust binary is the single source of truth for mutation: scanning,
  deduplication, backup creation. Having Python re-implement this logic would
  create two implementations that could diverge. Subprocess calls keep one
  canonical implementation.
"""

import json
import shutil
import subprocess
from pathlib import Path
from typing import Any, Optional

import streamlit as st


# ---------------------------------------------------------------------------
# Binary discovery
# ---------------------------------------------------------------------------

def find_binary() -> Optional[str]:
    """
    Find the `data-fortress` binary on the system PATH or in the project's
    target/release directory (for development use).

    Returns the absolute path to the binary, or None if not found.
    """
    # First check the system PATH — this is where installed binaries live.
    on_path = shutil.which("data-fortress")
    if on_path:
        return on_path

    # Fallback: look in the Cargo release output directory relative to this file.
    # This works when running the dashboard from the project directory during dev.
    # dashboard/utils/fortress.py → ../../target/release/data-fortress
    project_root = Path(__file__).parent.parent.parent
    release_bin = project_root / "target" / "release" / "data-fortress"
    if release_bin.exists():
        return str(release_bin)

    # Also check the debug build for development convenience.
    debug_bin = project_root / "target" / "debug" / "data-fortress"
    if debug_bin.exists():
        return str(debug_bin)

    return None


def _run(args: list[str], timeout: int = 300) -> dict | list:
    """
    Internal helper: run the binary with the given arguments and parse JSON output.

    Always passes `--json` so the binary outputs structured data.

    Args:
        args: List of arguments after the binary name (e.g. ["scan", "/home/tom"]).
        timeout: Maximum seconds to wait for the binary to finish.

    Returns:
        Parsed JSON (dict or list).

    Raises:
        RuntimeError: If the binary is not found, exits non-zero, or outputs invalid JSON.
    """
    binary = find_binary()
    if not binary:
        raise RuntimeError(
            "data-fortress binary not found. "
            "Run `cargo build --release` from the project root first."
        )

    # Build the full command: binary + --json flag + caller-supplied args.
    # --json tells the Rust binary to output JSON instead of human-readable text.
    cmd = [binary, "--json"] + args

    try:
        # `subprocess.run` executes the command, waits for it to finish, and
        # captures both stdout (JSON data) and stderr (log messages).
        result = subprocess.run(
            cmd,
            capture_output=True,  # Capture both stdout and stderr
            text=True,            # Decode bytes to str using the system encoding
            timeout=timeout,      # Kill the process if it runs too long
        )
    except subprocess.TimeoutExpired:
        raise RuntimeError(
            f"data-fortress timed out after {timeout}s running: {' '.join(cmd)}"
        )
    except FileNotFoundError:
        raise RuntimeError(f"Binary not executable: {binary}")

    # Non-zero exit code means the Rust binary returned an error.
    if result.returncode != 0:
        # The binary writes its error message to stderr.
        error_msg = result.stderr.strip() or "unknown error"
        raise RuntimeError(
            f"data-fortress failed (exit {result.returncode}): {error_msg}"
        )

    # Parse the JSON output from stdout.
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as e:
        raise RuntimeError(
            f"data-fortress output was not valid JSON: {e}\nOutput: {result.stdout[:200]}"
        )


# ---------------------------------------------------------------------------
# Public API — one function per subcommand
# ---------------------------------------------------------------------------

def run_scan(
    directories: list[str],
    hash_files: bool = False,
    dry_run: bool = False,
    timeout: int = 3600,
) -> dict:
    """
    Run `data-fortress scan` and return the ScanStats JSON.

    Args:
        directories: List of absolute directory paths to scan.
        hash_files:  If True, pass --hash to compute BLAKE3 content hashes.
        dry_run:     If True, pass --dry-run (no database writes).
        timeout:     Max seconds to wait (default 1 hour for large scans).

    Returns:
        ScanStats dict: {files_found, files_new, files_skipped, total_bytes, duration_ms}
    """
    args = ["scan"] + directories
    if hash_files:
        args.append("--hash")
    if dry_run:
        args.append("--dry-run")

    return _run(args, timeout=timeout)


def run_dedup(
    hash_first: bool = False,
    min_size: int = 0,
    dry_run: bool = False,
    timeout: int = 1800,
) -> dict:
    """
    Run `data-fortress dedup` and return the dedup summary JSON.

    Args:
        hash_first: Hash un-hashed files before deduplicating.
        min_size:   Only report duplicates larger than this many bytes.
        dry_run:    Preview without deleting.

    Returns:
        Dict: {groups_found, wasted_bytes, files_deleted, delete_errors}
    """
    args = ["dedup"]
    if hash_first:
        args.append("--hash")
    if min_size > 0:
        args += ["--min-size", str(min_size)]
    if dry_run:
        args.append("--dry-run")

    return _run(args, timeout=timeout)


def run_search(
    query: str,
    category: Optional[str] = None,
    content: bool = False,
    limit: int = 50,
    timeout: int = 60,
) -> list[dict]:
    """
    Run `data-fortress search` and return a list of SearchResult dicts.

    Use this for full content search (PDFs, DOCX etc.) when `content=True`.
    For metadata-only search, use utils/db.py::search_files() instead — it's
    faster because it queries SQLite directly without shelling out.

    Args:
        query:    Search query string.
        category: Optional category filter (e.g. "document", "image").
        content:  If True, pass --content to enable full-text extraction.
        limit:    Maximum number of results to return.

    Returns:
        List of SearchResult dicts: [{file: {...}, score: float, snippet: str|None}]
    """
    args = ["search", query, "--limit", str(limit)]
    if category:
        args += ["--category", category]
    if content:
        args.append("--content")

    result = _run(args, timeout=timeout)
    # The Rust binary returns a JSON array for search results.
    return result if isinstance(result, list) else []


def run_backup_create(
    label: Optional[str] = None,
    category: Optional[str] = None,
    compression: int = 3,
    dry_run: bool = False,
    timeout: int = 7200,
) -> dict:
    """
    Run `data-fortress backup create` and return the backup summary JSON.

    Args:
        label:        Human-readable backup label (auto-generated if None).
        category:     Only back up files in this category.
        compression:  zstd compression level (1–22).
        dry_run:      Preview without writing the archive.

    Returns:
        Dict: {archive_path, manifest_path, files_included,
               original_bytes, compressed_bytes, duration_ms, skipped}
    """
    args = ["backup", "create", "--compression", str(compression)]
    if label:
        args += ["--label", label]
    if category:
        args += ["--category", category]
    if dry_run:
        args.append("--dry-run")

    return _run(args, timeout=timeout)


def get_binary_info() -> dict:
    """
    Return version information about the installed binary.

    Used by the sidebar to show which version of Data Fortress is running.

    Returns:
        Dict with keys: version (str), path (str), found (bool)
    """
    binary = find_binary()
    if not binary:
        return {"found": False, "version": "not found", "path": None}

    try:
        # `--version` is handled by Clap and exits 0 with the version string.
        result = subprocess.run(
            [binary, "--version"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        version = result.stdout.strip()
    except Exception:
        version = "unknown"

    return {"found": True, "version": version, "path": binary}


# ---------------------------------------------------------------------------
# Streamlit-specific helpers
# ---------------------------------------------------------------------------

def show_binary_status():
    """
    Display a status indicator in the Streamlit sidebar showing whether
    the data-fortress binary is available.

    Call this from app.py or any page's sidebar section.
    """
    info = get_binary_info()
    if info["found"]:
        # st.success renders a green checkmark box.
        st.sidebar.success(f"Binary: {info['version']}")
    else:
        # st.error renders a red error box.
        st.sidebar.error(
            "data-fortress binary not found. "
            "Run `cargo build --release` from the project root."
        )

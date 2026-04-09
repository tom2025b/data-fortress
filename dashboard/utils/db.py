"""
utils/db.py
-----------
Read-only SQLite helpers for the Streamlit dashboard.

This module is the ONLY place in the dashboard that talks to SQLite directly.
All other pages import functions from here — they never write raw SQL themselves.

Design rules:
  - NEVER writes to the database (no INSERT, UPDATE, DELETE)
  - Always returns pandas DataFrames or plain Python dicts/lists
  - Uses @st.cache_data so repeated calls in the same session hit memory, not disk
  - Accepts an optional `db_path` argument so pages can be tested with a temp DB

Why read SQLite directly instead of shelling out to the Rust binary?
  The Rust binary is called for ACTIONS (scan, dedup, backup). For DISPLAY we read
  SQLite directly — it's faster, simpler, and the schema is stable and well-defined.
"""

import sqlite3
import os
from pathlib import Path
from typing import Optional

import pandas as pd
import streamlit as st


# ---------------------------------------------------------------------------
# Connection helper
# ---------------------------------------------------------------------------

def _default_db_path() -> Path:
    """
    Return the default database path: ~/.local/share/data-fortress/fortress.db
    This must match the path computed by config::Config::default_config() in Rust.
    """
    # XDG_DATA_HOME is the standard Linux env var for user data directories.
    # If not set, we fall back to ~/.local/share (the XDG default).
    xdg_data = os.environ.get("XDG_DATA_HOME", str(Path.home() / ".local" / "share"))
    return Path(xdg_data) / "data-fortress" / "fortress.db"


def get_connection(db_path: Optional[str] = None) -> sqlite3.Connection:
    """
    Open a read-only SQLite connection to the fortress database.

    Uses check_same_thread=False because Streamlit runs callbacks on different
    threads. We only read, so this is safe — SQLite supports concurrent readers.

    Args:
        db_path: Optional override path. Defaults to the XDG data directory.

    Returns:
        An open sqlite3.Connection in read-only mode.

    Raises:
        FileNotFoundError: If the database file does not exist yet.
    """
    # Use the provided path or fall back to the default XDG location.
    path = Path(db_path) if db_path else _default_db_path()

    if not path.exists():
        raise FileNotFoundError(
            f"Database not found at {path}. "
            "Run `data-fortress scan <directory>` first to create it."
        )

    # URI mode with `?mode=ro` opens the database in read-only mode.
    # This prevents any accidental writes from the dashboard.
    uri = f"file:{path}?mode=ro"
    conn = sqlite3.connect(uri, uri=True, check_same_thread=False)

    # `row_factory = sqlite3.Row` makes rows behave like dicts, so we can
    # access columns by name (row["path"]) instead of index (row[1]).
    conn.row_factory = sqlite3.Row

    return conn


# ---------------------------------------------------------------------------
# Overview / stats
# ---------------------------------------------------------------------------

@st.cache_data(ttl=30)  # Cache for 30 seconds — refresh after a new scan
def get_overview_stats(db_path: Optional[str] = None) -> dict:
    """
    Return high-level statistics for the Overview page.

    Returns a dict with keys:
        total_files, total_bytes, total_duplicates, wasted_bytes,
        category_counts (dict), most_recent_scan (str)
    """
    conn = get_connection(db_path)

    # Total files and storage currently present on disk.
    row = conn.execute(
        "SELECT COUNT(*) as n, COALESCE(SUM(size_bytes), 0) as b "
        "FROM files WHERE is_present = 1"
    ).fetchone()
    total_files = row["n"]
    total_bytes = row["b"]

    # Count of duplicate groups and wasted bytes.
    # A duplicate group = content_hash appearing more than once.
    dup_row = conn.execute("""
        SELECT
            COUNT(DISTINCT content_hash)                   AS groups,
            SUM(size_bytes) - SUM(min_size)                AS wasted
        FROM (
            SELECT content_hash,
                   size_bytes,
                   MIN(size_bytes) OVER (PARTITION BY content_hash) AS min_size
            FROM files
            WHERE content_hash IS NOT NULL
              AND is_present = 1
            GROUP BY path
        )
        WHERE content_hash IN (
            SELECT content_hash
            FROM files
            WHERE content_hash IS NOT NULL AND is_present = 1
            GROUP BY content_hash
            HAVING COUNT(*) > 1
        )
    """).fetchone()
    # Use `or 0` to handle the case where there are no duplicates (NULL result).
    total_duplicates = dup_row["groups"] or 0
    wasted_bytes     = dup_row["wasted"] or 0

    # Per-category file counts.
    cat_rows = conn.execute(
        "SELECT category, COUNT(*) as n FROM files WHERE is_present = 1 "
        "GROUP BY category ORDER BY n DESC"
    ).fetchall()
    category_counts = {row["category"]: row["n"] for row in cat_rows}

    # Most recent scan timestamp.
    scan_row = conn.execute(
        "SELECT MAX(scanned_at) as last_scan FROM files"
    ).fetchone()
    most_recent_scan = scan_row["last_scan"] or "Never"

    conn.close()

    return {
        "total_files":      total_files,
        "total_bytes":      total_bytes,
        "total_duplicates": total_duplicates,
        "wasted_bytes":     wasted_bytes,
        "category_counts":  category_counts,
        "most_recent_scan": most_recent_scan,
    }


@st.cache_data(ttl=30)
def get_category_breakdown(db_path: Optional[str] = None) -> pd.DataFrame:
    """
    Return per-category file counts and total bytes as a DataFrame.

    Columns: category, file_count, total_bytes
    """
    conn = get_connection(db_path)
    df = pd.read_sql_query(
        """
        SELECT
            category,
            COUNT(*)            AS file_count,
            SUM(size_bytes)     AS total_bytes
        FROM files
        WHERE is_present = 1
        GROUP BY category
        ORDER BY total_bytes DESC
        """,
        conn,
    )
    conn.close()
    return df


@st.cache_data(ttl=30)
def get_largest_files(limit: int = 20, db_path: Optional[str] = None) -> pd.DataFrame:
    """
    Return the largest files currently present on disk.

    Columns: name, path, size_bytes, category, modified_at
    """
    conn = get_connection(db_path)
    df = pd.read_sql_query(
        f"""
        SELECT name, path, size_bytes, category, modified_at
        FROM files
        WHERE is_present = 1
        ORDER BY size_bytes DESC
        LIMIT {int(limit)}
        """,
        conn,
    )
    conn.close()
    return df


# ---------------------------------------------------------------------------
# Duplicates
# ---------------------------------------------------------------------------

@st.cache_data(ttl=30)
def get_duplicate_groups(
    min_size: int = 0,
    db_path: Optional[str] = None,
) -> pd.DataFrame:
    """
    Return all duplicate file groups as a flat DataFrame.

    Each row represents one file in a duplicate group. The dashboard
    groups rows by content_hash to reconstruct the groups.

    Columns: content_hash, path, name, size_bytes, modified_at, category
    """
    conn = get_connection(db_path)
    df = pd.read_sql_query(
        """
        SELECT
            f.content_hash,
            f.path,
            f.name,
            f.size_bytes,
            f.modified_at,
            f.category
        FROM files f
        WHERE f.content_hash IS NOT NULL
          AND f.is_present = 1
          AND f.size_bytes >= :min_size
          AND f.content_hash IN (
              SELECT content_hash
              FROM files
              WHERE content_hash IS NOT NULL AND is_present = 1
              GROUP BY content_hash
              HAVING COUNT(*) > 1
          )
        ORDER BY f.size_bytes DESC, f.content_hash, f.path
        """,
        conn,
        params={"min_size": min_size},
    )
    conn.close()
    return df


# ---------------------------------------------------------------------------
# Search
# ---------------------------------------------------------------------------

@st.cache_data(ttl=10)  # Short TTL — search results should feel fresh
def search_files(
    query: str,
    category: Optional[str] = None,
    limit: int = 100,
    db_path: Optional[str] = None,
) -> pd.DataFrame:
    """
    Search files by name/path using SQLite LIKE.

    The dashboard's search page calls this for quick metadata-only search.
    Full content search (PDFs, etc.) is triggered by calling the Rust binary
    via utils/fortress.py with the --content flag.

    Columns: name, path, size_bytes, category, mime_type, modified_at
    """
    conn = get_connection(db_path)

    # Build the LIKE pattern: %query% matches anywhere in the string.
    pattern = f"%{query}%"

    # Apply optional category filter.
    category_clause = "AND category = :category" if category else ""
    cat_param = category if category else None

    df = pd.read_sql_query(
        f"""
        SELECT name, path, size_bytes, category, mime_type, modified_at
        FROM files
        WHERE is_present = 1
          AND (name LIKE :pattern OR path LIKE :pattern)
          {category_clause}
        ORDER BY name
        LIMIT :limit
        """,
        conn,
        params={
            "pattern":  pattern,
            "category": cat_param,
            "limit":    limit,
        },
    )
    conn.close()
    return df


# ---------------------------------------------------------------------------
# Backups
# ---------------------------------------------------------------------------

@st.cache_data(ttl=60)
def get_backup_history(db_path: Optional[str] = None) -> pd.DataFrame:
    """
    Return all backup records ordered by creation date (newest first).

    Columns: label, archive_path, original_bytes, compressed_bytes,
             algorithm, created_at
    """
    conn = get_connection(db_path)
    df = pd.read_sql_query(
        """
        SELECT label, archive_path, original_bytes, compressed_bytes,
               algorithm, created_at
        FROM backups
        ORDER BY created_at DESC
        """,
        conn,
    )
    conn.close()
    return df


# ---------------------------------------------------------------------------
# Scan history (derived from scanned_at timestamps)
# ---------------------------------------------------------------------------

@st.cache_data(ttl=60)
def get_scan_history(db_path: Optional[str] = None) -> pd.DataFrame:
    """
    Return daily scan counts — how many new files were first seen each day.

    Used by the Overview page to draw a scan history timeline.

    Columns: scan_date, files_added
    """
    conn = get_connection(db_path)
    df = pd.read_sql_query(
        """
        SELECT
            DATE(scanned_at) AS scan_date,
            COUNT(*)         AS files_added
        FROM files
        GROUP BY DATE(scanned_at)
        ORDER BY scan_date ASC
        """,
        conn,
    )
    conn.close()
    return df

"""
pages/3_search.py — Search Page
---------------------------------
Two-mode search interface:

  1. Metadata search (fast)  — queries SQLite directly via utils/db.py::search_files()
                               Matches on filename and path using LIKE.

  2. Content search (slow)   — shells out to the Rust binary via utils/fortress.py::run_search()
                               Extracts text from PDFs, DOCX, XLSX, code files, and EXIF data.
                               Results include a snippet showing the matched text.

The mode switches automatically based on whether the user ticks "Include file contents".
Results are shown in a sortable table. Clicking a row shows the full path.
"""

# ── Standard library ─────────────────────────────────────────────────────────
import sys
from pathlib import Path

# ── Third-party ──────────────────────────────────────────────────────────────
import humanize
import pandas as pd
import streamlit as st

# ── Local utils ───────────────────────────────────────────────────────────────
sys.path.insert(0, str(Path(__file__).parent.parent))

from utils.db import search_files
from utils.fortress import run_search, show_binary_status

# ── Page config ───────────────────────────────────────────────────────────────
st.set_page_config(
    page_title="Data Fortress — Search",
    page_icon="🏰",
    layout="wide",
)

# ── Sidebar ───────────────────────────────────────────────────────────────────
with st.sidebar:
    st.title("🏰 Data Fortress")
    st.caption("Personal File Management")
    st.divider()
    show_binary_status()

    # Ensure db_path is set (handles direct navigation to this page).
    if "db_path" not in st.session_state:
        import os
        default_db = str(
            Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share"))
            / "data-fortress" / "fortress.db"
        )
        st.session_state["db_path"] = default_db

    db_path = st.session_state["db_path"]
    st.caption(f"DB: `{db_path}`")

    st.divider()

    # ── Search options ────────────────────────────────────────────────────────
    st.subheader("Options")

    # Category filter — "All" means no filter.
    CATEGORIES = ["All", "image", "video", "audio", "document", "archive", "code", "other"]
    category_choice = st.selectbox(
        "Category filter",
        options=CATEGORIES,
        index=0,
        help="Restrict results to files of a specific type.",
    )
    category_param = None if category_choice == "All" else category_choice

    # Result limit — metadata search can return many rows; content search is slower.
    result_limit = st.slider(
        "Max results",
        min_value=10,
        max_value=500,
        value=50,
        step=10,
        help="Maximum number of results to display.",
    )

    # Content search toggle — switches between fast SQLite and Rust binary.
    include_content = st.checkbox(
        "Include file contents",
        value=False,
        help=(
            "Search inside PDFs, DOCX, XLSX, code files, and image EXIF data. "
            "Much slower than metadata-only search."
        ),
    )

# ── Page header ───────────────────────────────────────────────────────────────
st.title("Search")

# Mode indicator — tells the user which engine is active.
if include_content:
    st.caption("Content search — extracts text from documents and EXIF from images (slow)")
else:
    st.caption("Metadata search — matches filenames and paths (fast)")

# ── Search bar ────────────────────────────────────────────────────────────────
# st.text_input returns the current value on every rerun.
# We use on_change to clear old results when the query changes.
query = st.text_input(
    "Search query",
    placeholder="e.g.  invoice  OR  IMG_2024  OR  budget Q3",
    label_visibility="collapsed",  # Hide the label; the placeholder explains it
)

# Run button — also triggers on Enter because Streamlit reruns on text_input change.
run_search_btn = st.button("Search", type="primary", use_container_width=False)

st.divider()

# ── Execute search ────────────────────────────────────────────────────────────
# Only search when there is a non-empty query.
if not query.strip():
    st.info("Enter a search term above to begin.")
    st.stop()

# Show a spinner while searching (especially useful for content search).
with st.spinner(f"Searching for "{query}"…"):
    try:
        if include_content:
            # ── Content search via Rust binary ────────────────────────────────
            # run_search() returns a list of SearchResult dicts from the binary:
            # [{file: {path, name, size_bytes, ...}, score: float, snippet: str|None}]
            raw_results = run_search(
                query=query,
                category=category_param,
                content=True,
                limit=result_limit,
            )

            # Flatten the nested structure into a flat list of dicts for pandas.
            rows = []
            for r in raw_results:
                f = r.get("file", {})
                rows.append({
                    "name":        f.get("name", ""),
                    "path":        f.get("path", ""),
                    "size_bytes":  f.get("size_bytes", 0),
                    "category":    f.get("category", ""),
                    "mime_type":   f.get("mime_type", ""),
                    "modified_at": f.get("modified_at", ""),
                    "score":       round(r.get("score", 0.0), 2),
                    "snippet":     r.get("snippet") or "",
                })
            results_df = pd.DataFrame(rows)
            search_mode = "content"

        else:
            # ── Metadata search via SQLite ────────────────────────────────────
            # search_files() returns a DataFrame with columns:
            # name, path, size_bytes, category, mime_type, modified_at
            results_df = search_files(
                query=query,
                category=category_param,
                limit=result_limit,
                db_path=db_path,
            )
            # Add placeholder columns so the rest of the code is uniform.
            results_df["score"]   = None
            results_df["snippet"] = ""
            search_mode = "metadata"

    except FileNotFoundError:
        st.info(
            "No database found yet. Run your first scan:\n\n"
            "```bash\ndata-fortress scan /path/to/your/files\n```"
        )
        st.stop()
    except RuntimeError as e:
        st.error(f"Search failed: {e}")
        st.stop()
    except Exception as e:
        st.error(f"Unexpected error: {e}")
        st.stop()

# ── Results header ────────────────────────────────────────────────────────────
n_results = len(results_df)

if n_results == 0:
    st.warning(f"No results for **{query}**.")
    if include_content:
        st.caption(
            "Content search requires files to have been scanned with `--hash`. "
            "Try a metadata-only search first."
        )
    st.stop()

# Summary line above the table.
mode_label = "content" if search_mode == "content" else "metadata"
st.success(f"Found **{n_results:,}** result{'s' if n_results != 1 else ''} ({mode_label} search)")

# ── Results table ─────────────────────────────────────────────────────────────
# Build a human-friendly display DataFrame.
display_df = results_df.copy()
display_df["size"] = display_df["size_bytes"].apply(
    lambda b: humanize.naturalsize(int(b), binary=True) if b else ""
)

# Columns to show differ by mode (content search has score + snippet).
if search_mode == "content":
    display_df = display_df.rename(columns={
        "name":        "Filename",
        "size":        "Size",
        "category":    "Category",
        "score":       "Score",
        "snippet":     "Snippet",
        "modified_at": "Modified",
        "path":        "Full Path",
    })[["Filename", "Score", "Size", "Category", "Snippet", "Modified", "Full Path"]]

    st.dataframe(
        display_df,
        use_container_width=True,
        hide_index=True,
        column_config={
            "Score":     st.column_config.NumberColumn(format="%.2f"),
            "Snippet":   st.column_config.TextColumn(max_chars=120),
            "Full Path": st.column_config.TextColumn(max_chars=80),
            "Modified":  st.column_config.TextColumn(max_chars=19),
        },
    )
else:
    display_df = display_df.rename(columns={
        "name":        "Filename",
        "size":        "Size",
        "category":    "Category",
        "mime_type":   "MIME Type",
        "modified_at": "Modified",
        "path":        "Full Path",
    })[["Filename", "Size", "Category", "MIME Type", "Modified", "Full Path"]]

    st.dataframe(
        display_df,
        use_container_width=True,
        hide_index=True,
        column_config={
            "Full Path": st.column_config.TextColumn(max_chars=80),
            "MIME Type": st.column_config.TextColumn(max_chars=40),
            "Modified":  st.column_config.TextColumn(max_chars=19),
        },
    )

st.divider()

# ── Per-result detail expanders ───────────────────────────────────────────────
# Show expandable detail cards for the top-N results.
# This lets the user copy the full path and read the full snippet.
DETAIL_LIMIT = 10  # Only expand details for the top results to keep the page fast

if n_results > 0:
    st.subheader(f"File Details (top {min(n_results, DETAIL_LIMIT)})")

    for _, row in results_df.head(DETAIL_LIMIT).iterrows():
        name     = row["name"]
        path     = row["path"]
        size_str = humanize.naturalsize(int(row["size_bytes"]), binary=True) if row["size_bytes"] else "unknown"
        cat      = row["category"]
        snippet  = row.get("snippet", "")
        score    = row.get("score")

        # Build the expander label: filename + size + optional score.
        score_str = f" · score {score:.2f}" if score is not None and score != "" else ""
        label = f"{name} · {size_str} · {cat}{score_str}"

        with st.expander(label, expanded=False):
            # Full path in a copyable code block.
            st.code(path, language=None)

            # Show the snippet if this was a content search result.
            if snippet:
                st.markdown("**Matched text:**")
                # Use st.text to preserve whitespace in the snippet.
                st.text(snippet)

            # Quick metadata grid.
            d1, d2, d3 = st.columns(3)
            d1.metric("Size",     size_str)
            d2.metric("Category", cat)
            d3.metric("Modified", str(row.get("modified_at", ""))[:10])

# ── Learning Notes ────────────────────────────────────────────────────────────
# Key concepts used in this file:
#
# st.text_input()                — single-line text field; returns current value
# st.button(type="primary")      — styled action button
# st.spinner()                   — shows a spinner while a block of code runs
# label_visibility="collapsed"   — hides the widget label (placeholder is enough)
# pd.DataFrame(list_of_dicts)    — builds a DataFrame from a list of row dicts
# results_df.head(N)             — returns the first N rows of a DataFrame
# st.column_config.NumberColumn  — formats numbers in st.dataframe cells
# st.column_config.TextColumn    — truncates long strings with ellipsis
# st.code(text, language=None)   — monospace copyable block without syntax highlighting
# st.text(text)                  — plain preformatted text (preserves whitespace)
# search_mode flag               — controls which columns are shown (content vs metadata)
# DETAIL_LIMIT                   — prevents rendering hundreds of expanders (performance)

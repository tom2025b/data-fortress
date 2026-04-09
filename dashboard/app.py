"""
app.py — Data Fortress Streamlit Dashboard
------------------------------------------
Root entry point for the dashboard. Streamlit runs this file first and
auto-discovers the pages in dashboard/pages/ from the numeric prefixes in
their filenames (1_overview.py, 2_duplicates.py, etc.).

This file sets up:
  - Page configuration (title, icon, layout)
  - Sidebar elements common to all pages (binary status, DB path, nav links)
  - A landing message shown before the user navigates to a page

Run with:
    streamlit run dashboard/app.py
"""

import os
from pathlib import Path

import streamlit as st

# Import our utility modules.
# The `sys.path` manipulation below makes `utils/` importable from any page.
import sys
sys.path.insert(0, str(Path(__file__).parent))

from utils.db import get_overview_stats, get_connection
from utils.fortress import show_binary_status, get_binary_info

# ---------------------------------------------------------------------------
# Page configuration (must be the first Streamlit call in the file)
# ---------------------------------------------------------------------------

st.set_page_config(
    page_title="Data Fortress",
    page_icon="🏰",
    layout="wide",          # Use the full browser width
    initial_sidebar_state="expanded",
)

# ---------------------------------------------------------------------------
# Sidebar
# ---------------------------------------------------------------------------

with st.sidebar:
    st.title("🏰 Data Fortress")
    st.caption("Personal File Management")
    st.divider()

    # Show whether the Rust binary is reachable.
    show_binary_status()

    # Database path selector — lets the user point at a different DB for testing.
    default_db = str(
        Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share"))
        / "data-fortress" / "fortress.db"
    )
    db_path = st.text_input(
        "Database path",
        value=default_db,
        help="Path to the fortress.db SQLite file created by `data-fortress scan`",
    )

    # Store the selected DB path in session state so all pages can read it.
    # st.session_state persists across page navigation within a session.
    st.session_state["db_path"] = db_path

    st.divider()
    st.caption("Navigate using the pages above ↑")

# ---------------------------------------------------------------------------
# Main area — shown on the root URL before any page is selected
# ---------------------------------------------------------------------------

st.title("🏰 Data Fortress")
st.subheader("Personal Data Management System")

st.markdown("""
Welcome to **Data Fortress** — your personal file management dashboard.

Use the sidebar to navigate between pages:

| Page | What it shows |
|------|--------------|
| **1 · Overview** | Storage usage, category breakdown, scan history |
| **2 · Duplicates** | Files with identical content, wasted space |
| **3 · Search** | Find files by name, content, or metadata |
| **4 · Backup** | Backup history and create new backups |
""")

st.divider()

# Try to show a quick summary if the database exists.
db = st.session_state.get("db_path", default_db)

try:
    # Import humanize here so the error message is clear if it's not installed.
    import humanize

    stats = get_overview_stats(db)

    # Display four key metrics in a row using st.columns.
    # st.metric shows a number with an optional delta indicator.
    col1, col2, col3, col4 = st.columns(4)

    with col1:
        st.metric(
            label="Total Files",
            value=f"{stats['total_files']:,}",
        )
    with col2:
        st.metric(
            label="Storage Used",
            value=humanize.naturalsize(stats["total_bytes"], binary=True),
        )
    with col3:
        st.metric(
            label="Duplicate Groups",
            value=f"{stats['total_duplicates']:,}",
            delta=f"-{humanize.naturalsize(stats['wasted_bytes'], binary=True)} wasted"
                  if stats["wasted_bytes"] > 0 else None,
            delta_color="inverse",  # Red delta = bad (wasted space)
        )
    with col4:
        st.metric(
            label="Last Scan",
            value=stats["most_recent_scan"][:10] if stats["most_recent_scan"] != "Never"
                  else "Never",
        )

except FileNotFoundError:
    # Database hasn't been created yet — first-run state.
    st.info(
        "No database found yet. Run your first scan to get started:\n\n"
        "```bash\n"
        "data-fortress scan /path/to/your/files\n"
        "```"
    )
except Exception as e:
    st.warning(f"Could not load stats: {e}")

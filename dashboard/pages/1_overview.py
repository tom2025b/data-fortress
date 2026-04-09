"""
pages/1_overview.py — Overview Page
-------------------------------------
Streamlit auto-discovers this file because of the numeric prefix (1_).
It appears as "Overview" in the sidebar navigation.

This page shows the big picture:
  - Four key metrics at the top (same as the landing page)
  - Category breakdown: pie chart + bar chart + data table
  - Top 20 largest files
  - Scan history timeline (files added per day)

All data comes from utils/db.py — read-only SQLite queries.
No binary subprocess calls are made from this page.
"""

# ── Standard library ─────────────────────────────────────────────────────────
import sys
from pathlib import Path

# ── Third-party ──────────────────────────────────────────────────────────────
import humanize
import pandas as pd
import plotly.express as px
import streamlit as st

# ── Local utils ───────────────────────────────────────────────────────────────
# Insert dashboard/ into the path so `utils` is importable from any page.
# Streamlit changes cwd when running pages, so we anchor relative to __file__.
sys.path.insert(0, str(Path(__file__).parent.parent))

from utils.db import (
    get_category_breakdown,
    get_largest_files,
    get_overview_stats,
    get_scan_history,
)

# ── Page config ───────────────────────────────────────────────────────────────
# Each page CAN call st.set_page_config, but only once and it must be first.
# We set just the title; the icon is inherited from app.py.
st.set_page_config(
    page_title="Data Fortress — Overview",
    page_icon="🏰",
    layout="wide",
)

# ── Sidebar ───────────────────────────────────────────────────────────────────
# Re-import fortress for the binary status widget.
from utils.fortress import show_binary_status

with st.sidebar:
    st.title("🏰 Data Fortress")
    st.caption("Personal File Management")
    st.divider()
    show_binary_status()

    # Read the db_path set by app.py, or compute the default if navigating
    # directly to this page without going through the root first.
    if "db_path" not in st.session_state:
        import os
        default_db = str(
            Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share"))
            / "data-fortress" / "fortress.db"
        )
        st.session_state["db_path"] = default_db

    db_path = st.session_state["db_path"]
    st.caption(f"DB: `{db_path}`")

# ── Page header ───────────────────────────────────────────────────────────────
st.title("Overview")
st.caption("Storage summary, category breakdown, and scan history")
st.divider()

# ── Fetch data ────────────────────────────────────────────────────────────────
# Wrap all DB reads in a try/except so the page degrades gracefully when the
# database doesn't exist yet (first-run state).
try:
    stats     = get_overview_stats(db_path)
    cat_df    = get_category_breakdown(db_path)
    large_df  = get_largest_files(limit=20, db_path=db_path)
    history_df = get_scan_history(db_path)

except FileNotFoundError:
    # Database hasn't been created yet — show friendly first-run message.
    st.info(
        "No database found yet. Run your first scan to get started:\n\n"
        "```bash\n"
        "data-fortress scan /path/to/your/files\n"
        "```"
    )
    # st.stop() tells Streamlit to stop rendering the rest of the page.
    st.stop()

except Exception as e:
    st.error(f"Could not load data: {e}")
    st.stop()

# ── Section 1: Key metrics ────────────────────────────────────────────────────
# Display four numbers side by side using st.columns.
# st.metric(label, value, delta) renders a styled number card.
col1, col2, col3, col4 = st.columns(4)

with col1:
    st.metric(
        label="Total Files",
        value=f"{stats['total_files']:,}",
    )

with col2:
    # humanize.naturalsize converts raw bytes to a human-readable string.
    # binary=True uses KiB/MiB/GiB instead of KB/MB/GB.
    st.metric(
        label="Storage Used",
        value=humanize.naturalsize(stats["total_bytes"], binary=True),
    )

with col3:
    # delta_color="inverse" makes the delta red (bad) for wasted space.
    wasted = stats["wasted_bytes"]
    st.metric(
        label="Duplicate Groups",
        value=f"{stats['total_duplicates']:,}",
        delta=f"-{humanize.naturalsize(wasted, binary=True)} wasted" if wasted > 0 else None,
        delta_color="inverse",
    )

with col4:
    last_scan = stats["most_recent_scan"]
    # Slice to YYYY-MM-DD if it's a full ISO 8601 timestamp, else show as-is.
    display_scan = last_scan[:10] if last_scan != "Never" else "Never"
    st.metric(label="Last Scan", value=display_scan)

st.divider()

# ── Section 2: Category breakdown ─────────────────────────────────────────────
st.subheader("Category Breakdown")

if cat_df.empty:
    st.info("No files indexed yet.")
else:
    # Two columns: pie chart on the left, bar chart on the right.
    pie_col, bar_col = st.columns(2)

    with pie_col:
        # Pie chart: proportion of STORAGE used by each category.
        # plotly.express handles the Figure creation; Streamlit renders it.
        fig_pie = px.pie(
            cat_df,
            names="category",
            values="total_bytes",
            title="Storage by Category",
            # hole=0.3 makes it a donut chart — easier to read the center.
            hole=0.3,
            color_discrete_sequence=px.colors.qualitative.Set2,
        )
        # Disable the legend inside the chart; the labels on slices are enough.
        fig_pie.update_traces(textposition="inside", textinfo="percent+label")
        fig_pie.update_layout(showlegend=False, margin=dict(t=40, b=0, l=0, r=0))
        st.plotly_chart(fig_pie, use_container_width=True)

    with bar_col:
        # Horizontal bar chart: file COUNT per category.
        # orientation="h" flips the bars to horizontal for better label readability.
        fig_bar = px.bar(
            cat_df.sort_values("file_count"),  # Sort ascending so largest is on top
            x="file_count",
            y="category",
            orientation="h",
            title="File Count by Category",
            labels={"file_count": "Files", "category": "Category"},
            color="file_count",
            color_continuous_scale="Blues",
        )
        fig_bar.update_layout(
            coloraxis_showscale=False,  # Hide the colour legend
            margin=dict(t=40, b=0, l=0, r=0),
        )
        st.plotly_chart(fig_bar, use_container_width=True)

    # Data table below the charts — lets users see exact numbers.
    with st.expander("Category data table", expanded=False):
        # Add a human-readable size column alongside the raw bytes.
        display_df = cat_df.copy()
        display_df["size"] = display_df["total_bytes"].apply(
            lambda b: humanize.naturalsize(b, binary=True)
        )
        # Rename for the display table.
        display_df = display_df.rename(columns={
            "category":   "Category",
            "file_count": "Files",
            "size":       "Size",
        })[["Category", "Files", "Size"]]

        # st.dataframe renders an interactive sortable table.
        st.dataframe(display_df, use_container_width=True, hide_index=True)

st.divider()

# ── Section 3: Largest files ──────────────────────────────────────────────────
st.subheader("Largest Files (Top 20)")

if large_df.empty:
    st.info("No files indexed yet.")
else:
    # Add a human-readable size column.
    display_large = large_df.copy()
    display_large["size"] = display_large["size_bytes"].apply(
        lambda b: humanize.naturalsize(b, binary=True)
    )
    # Keep only columns the user cares about and rename them.
    display_large = display_large.rename(columns={
        "name":        "Filename",
        "path":        "Full Path",
        "size":        "Size",
        "category":    "Category",
        "modified_at": "Modified",
    })[["Filename", "Size", "Category", "Modified", "Full Path"]]

    # column_config lets us control how individual columns render.
    # Here we set a max width for the "Full Path" column to avoid overflow.
    st.dataframe(
        display_large,
        use_container_width=True,
        hide_index=True,
        column_config={
            "Full Path": st.column_config.TextColumn(max_chars=80),
            "Modified":  st.column_config.TextColumn(max_chars=19),
        },
    )

st.divider()

# ── Section 4: Scan history timeline ─────────────────────────────────────────
st.subheader("Scan History")
st.caption("Files indexed per day (first seen per scan)")

if history_df.empty:
    st.info("No scan history yet.")
else:
    # Area chart: shows cumulative ingest rate over time.
    # px.area fills the region below the line, which reads well for "growth".
    fig_hist = px.area(
        history_df,
        x="scan_date",
        y="files_added",
        labels={"scan_date": "Date", "files_added": "Files Added"},
        title="New Files Indexed Over Time",
        color_discrete_sequence=["#4C78A8"],  # Blue that matches the bar chart palette
    )
    fig_hist.update_layout(
        hovermode="x unified",  # Show all values at the same x on hover
        margin=dict(t=40, b=0, l=0, r=0),
    )
    # Ensure the x-axis displays as dates, not raw strings.
    fig_hist.update_xaxes(type="date")
    st.plotly_chart(fig_hist, use_container_width=True)

    # Summary stats below the chart.
    total_days   = len(history_df)
    total_events = int(history_df["files_added"].sum())
    avg_per_day  = total_events / total_days if total_days > 0 else 0
    first_scan   = history_df["scan_date"].min()
    last_scan_d  = history_df["scan_date"].max()

    m1, m2, m3, m4 = st.columns(4)
    m1.metric("Scan Days",    f"{total_days:,}")
    m2.metric("Total Events", f"{total_events:,}")
    m3.metric("Avg / Day",    f"{avg_per_day:,.1f}")
    m4.metric("First Scan",   str(first_scan)[:10])

# ── Learning Notes ────────────────────────────────────────────────────────────
# Key concepts used in this file:
#
# st.set_page_config() — must be the FIRST Streamlit call per page; sets tab title
# st.stop()            — halts rendering immediately (used for error/empty states)
# st.columns(n)        — creates n side-by-side layout columns
# st.metric()          — renders a KPI card with an optional delta indicator
# st.expander()        — collapsible section to hide secondary content
# st.dataframe()       — sortable, scrollable table with optional column_config
# st.plotly_chart()    — embeds any Plotly Figure; use_container_width fills column
# px.pie()             — Plotly Express pie/donut chart
# px.bar(orientation)  — horizontal bar chart (orientation="h")
# px.area()            — area/line chart, good for time series
# humanize.naturalsize — bytes → "1.2 GiB" (binary=True uses powers of 1024)
# @st.cache_data       — memoises DB results for TTL seconds (defined in db.py)

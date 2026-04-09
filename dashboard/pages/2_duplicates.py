"""
pages/2_duplicates.py — Duplicates Page
-----------------------------------------
Shows all files that share an identical content hash (BLAKE3), grouped so
the user can see exactly which copies exist and how much space they waste.

Features:
  - Summary metrics: groups found, files affected, bytes wasted
  - Min-size filter slider (ignore tiny duplicates like empty files)
  - Per-group expanders showing every copy with path, size, modified date
  - "Run Dedup (dry run)" button — calls the Rust binary via fortress.py
  - "Delete duplicates" button — calls the binary for real (with confirmation)

Data flow:
  - Display: utils/db.py::get_duplicate_groups() → SQLite (fast, read-only)
  - Mutation: utils/fortress.py::run_dedup() → shells out to Rust binary
"""

# ── Standard library ─────────────────────────────────────────────────────────
import sys
from pathlib import Path

# ── Third-party ──────────────────────────────────────────────────────────────
import humanize
import plotly.express as px
import streamlit as st

# ── Local utils ───────────────────────────────────────────────────────────────
sys.path.insert(0, str(Path(__file__).parent.parent))

from utils.db import get_duplicate_groups, get_overview_stats
from utils.fortress import run_dedup, show_binary_status

# ── Page config ───────────────────────────────────────────────────────────────
st.set_page_config(
    page_title="Data Fortress — Duplicates",
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

    # ── Filter controls ───────────────────────────────────────────────────────
    st.subheader("Filters")

    # Slider: ignore duplicates smaller than this size.
    # Values jump through common thresholds: 0 B, 1 KiB, 1 MiB, 10 MiB, 100 MiB.
    min_size_labels = {
        0:          "No minimum",
        1_024:      "1 KiB",
        102_400:    "100 KiB",
        1_048_576:  "1 MiB",
        10_485_760: "10 MiB",
    }
    min_size_bytes = st.select_slider(
        "Minimum file size",
        options=list(min_size_labels.keys()),
        value=0,
        format_func=lambda v: min_size_labels[v],
        help="Only show duplicate groups where each file is at least this large.",
    )

    # ── Dedup action controls ─────────────────────────────────────────────────
    st.divider()
    st.subheader("Actions")

    # keep_strategy must match KeepStrategy in cli.rs (lowercase ValueEnum names).
    keep_strategy = st.selectbox(
        "Keep strategy",
        options=["oldest", "newest", "first-alpha", "shortest-path"],
        index=0,
        help="Which copy to keep when deleting duplicates.",
    )

    hash_first = st.checkbox(
        "Hash un-hashed files first",
        value=False,
        help="Run BLAKE3 on files that haven't been hashed yet before deduplicating.",
    )

# ── Page header ───────────────────────────────────────────────────────────────
st.title("Duplicates")
st.caption("Files with identical content, grouped by BLAKE3 hash")
st.divider()

# ── Fetch data ────────────────────────────────────────────────────────────────
try:
    stats  = get_overview_stats(db_path)
    dup_df = get_duplicate_groups(min_size=min_size_bytes, db_path=db_path)

except FileNotFoundError:
    st.info(
        "No database found yet. Run your first scan:\n\n"
        "```bash\ndata-fortress scan /path/to/your/files\n```"
    )
    st.stop()

except Exception as e:
    st.error(f"Could not load duplicates: {e}")
    st.stop()

# ── Section 1: Summary metrics ────────────────────────────────────────────────
# Compute group-level stats from the flat DataFrame.
# dup_df has one row per FILE; we group by content_hash to get per-group info.
if dup_df.empty:
    n_groups      = 0
    n_files       = 0
    total_wasted  = 0
else:
    # Each unique hash is one duplicate group.
    n_groups = dup_df["content_hash"].nunique()
    n_files  = len(dup_df)

    # Wasted bytes = total size of all copies MINUS one copy per group.
    # We keep the smallest file in each group as the "canonical" copy.
    group_min = dup_df.groupby("content_hash")["size_bytes"].min()
    group_sum = dup_df.groupby("content_hash")["size_bytes"].sum()
    total_wasted = int((group_sum - group_min).sum())

col1, col2, col3 = st.columns(3)

with col1:
    st.metric(
        label="Duplicate Groups",
        value=f"{n_groups:,}",
        help="Each group contains 2+ files with identical content.",
    )
with col2:
    st.metric(
        label="Files Affected",
        value=f"{n_files:,}",
    )
with col3:
    st.metric(
        label="Space Wasted",
        value=humanize.naturalsize(total_wasted, binary=True),
        delta=f"-{humanize.naturalsize(total_wasted, binary=True)}" if total_wasted > 0 else None,
        delta_color="inverse",
    )

st.divider()

# ── Section 2: Dedup action buttons ──────────────────────────────────────────
st.subheader("Run Deduplication")

# Two columns: dry run (safe) on the left, real delete (dangerous) on the right.
btn_col1, btn_col2 = st.columns(2)

with btn_col1:
    dry_run_clicked = st.button(
        "Preview (dry run)",
        type="secondary",
        help="Show what would be deleted without removing any files.",
        use_container_width=True,
    )

with btn_col2:
    # Use a red "primary" button for the destructive action.
    delete_clicked = st.button(
        "Delete duplicates",
        type="primary",
        help="Permanently delete duplicate files. The kept copy is chosen by your Keep strategy.",
        use_container_width=True,
    )

# Confirmation gate for the real delete — require an extra checkbox.
# st.session_state persists the confirmation state across reruns.
if delete_clicked:
    st.session_state["confirm_delete"] = True

if st.session_state.get("confirm_delete") and not dry_run_clicked:
    st.warning(
        "This will permanently delete files from disk. Confirm below to proceed."
    )
    confirmed = st.checkbox("Yes, I understand — delete the duplicates")
    if not confirmed:
        # Reset so the warning doesn't linger after the user clicks away.
        st.session_state["confirm_delete"] = False
        delete_clicked = False

# ── Execute dedup ─────────────────────────────────────────────────────────────
# Both buttons call run_dedup(); only the dry_run flag differs.
if dry_run_clicked or (delete_clicked and st.session_state.get("confirm_delete")):
    is_dry = dry_run_clicked or not delete_clicked

    with st.spinner("Running dedup…"):
        try:
            report = run_dedup(
                hash_first=hash_first,
                min_size=min_size_bytes,
                dry_run=is_dry,
            )

            # Show results in a nice expandable box.
            prefix = "Dry-run preview" if is_dry else "Dedup complete"
            with st.expander(f"{prefix} — results", expanded=True):
                r1, r2, r3, r4 = st.columns(4)
                r1.metric("Groups Found",    f"{report.get('groups_found', 0):,}")
                r2.metric(
                    "Space Wasted",
                    humanize.naturalsize(report.get("wasted_bytes", 0), binary=True),
                )
                r3.metric("Files Deleted",   f"{report.get('files_deleted', 0):,}")
                r4.metric("Delete Errors",   f"{report.get('delete_errors', 0):,}")

            if not is_dry:
                # Clear caches so the page reloads fresh data after deletion.
                st.cache_data.clear()
                st.success("Duplicates deleted. Refresh the page to see updated stats.")
                st.session_state["confirm_delete"] = False

        except RuntimeError as e:
            st.error(f"Dedup failed: {e}")

st.divider()

# ── Section 3: Duplicate groups browser ──────────────────────────────────────
st.subheader("Duplicate Groups")

if dup_df.empty:
    if min_size_bytes > 0:
        st.info(
            f"No duplicates larger than {humanize.naturalsize(min_size_bytes, binary=True)} found. "
            "Lower the minimum size filter to see smaller duplicates."
        )
    else:
        st.success(
            "No duplicates found! Run a scan with `--hash` to detect content-identical files:\n\n"
            "```bash\ndata-fortress scan /path --hash\n```"
        )
    st.stop()

# Group the flat DataFrame by content_hash so we can show one expander per group.
# sort=False preserves the size-descending order from the SQL query.
groups = dup_df.groupby("content_hash", sort=False)

# Pagination: show 20 groups at a time to avoid overwhelming the page.
PAGE_SIZE = 20
total_groups = n_groups
total_pages  = max(1, (total_groups + PAGE_SIZE - 1) // PAGE_SIZE)

# Page selector in the sidebar-adjacent area.
page_num = st.number_input(
    f"Page (1–{total_pages})",
    min_value=1,
    max_value=total_pages,
    value=1,
    step=1,
)

# Slice the list of hashes for the current page.
all_hashes   = list(groups.groups.keys())  # Ordered list of unique hashes
page_start   = (page_num - 1) * PAGE_SIZE
page_hashes  = all_hashes[page_start : page_start + PAGE_SIZE]

st.caption(
    f"Showing groups {page_start + 1}–{min(page_start + PAGE_SIZE, total_groups)} "
    f"of {total_groups:,} total"
)

# ── Render one expander per duplicate group ───────────────────────────────────
for i, content_hash in enumerate(page_hashes, start=page_start + 1):
    # Get all rows for this hash as a sub-DataFrame.
    group = groups.get_group(content_hash)

    # Pick a representative name from the group (most common, or the first).
    rep_name   = group["name"].mode().iloc[0] if not group.empty else "unknown"
    group_size = int(group["size_bytes"].iloc[0])  # All copies have the same size
    copy_count = len(group)
    wasted     = group_size * (copy_count - 1)

    # expander label: index, representative filename, size, copy count
    label = (
        f"#{i} · {rep_name} · "
        f"{humanize.naturalsize(group_size, binary=True)} × {copy_count} copies "
        f"({humanize.naturalsize(wasted, binary=True)} wasted)"
    )

    with st.expander(label, expanded=False):
        # Show the truncated hash so power users can verify.
        st.caption(f"BLAKE3: `{content_hash[:16]}…`")

        # Build a display table for this group's files.
        display_group = group[["path", "name", "size_bytes", "category", "modified_at"]].copy()
        display_group["size"] = display_group["size_bytes"].apply(
            lambda b: humanize.naturalsize(b, binary=True)
        )
        display_group = display_group.rename(columns={
            "path":        "Full Path",
            "name":        "Filename",
            "size":        "Size",
            "category":    "Category",
            "modified_at": "Modified",
        })[["Filename", "Size", "Category", "Modified", "Full Path"]]

        st.dataframe(
            display_group,
            use_container_width=True,
            hide_index=True,
            column_config={
                "Full Path": st.column_config.TextColumn(max_chars=80),
            },
        )

        # Show the category breakdown within the group (usually all the same).
        cats = group["category"].value_counts()
        if len(cats) > 1:
            st.caption(f"Mixed categories: {', '.join(f'{v}× {k}' for k, v in cats.items())}")

# ── Section 4: Wasted space chart ────────────────────────────────────────────
st.divider()
st.subheader("Wasted Space by Category")

if not dup_df.empty:
    # For each file in a duplicate group, compute the wasted bytes.
    # Wasted = size × (copies - 1) / copies for each file's contribution,
    # but it's simpler to compute it at the group level and then join category.
    grp_stats = (
        dup_df
        .groupby("content_hash")
        .agg(
            size_bytes=("size_bytes", "first"),   # All copies same size
            copies=("path", "count"),
            category=("category", "first"),        # Representative category
        )
        .reset_index()
    )
    # Wasted bytes per group = size × (copies − 1)
    grp_stats["wasted"] = grp_stats["size_bytes"] * (grp_stats["copies"] - 1)

    # Aggregate wasted bytes per category.
    cat_wasted = (
        grp_stats
        .groupby("category")["wasted"]
        .sum()
        .reset_index()
        .rename(columns={"wasted": "wasted_bytes", "category": "Category"})
        .sort_values("wasted_bytes", ascending=False)
    )
    cat_wasted["Wasted"] = cat_wasted["wasted_bytes"].apply(
        lambda b: humanize.naturalsize(b, binary=True)
    )

    fig_waste = px.bar(
        cat_wasted,
        x="Category",
        y="wasted_bytes",
        labels={"wasted_bytes": "Wasted Bytes", "Category": "Category"},
        title="Wasted Space by File Category",
        color="wasted_bytes",
        color_continuous_scale="Reds",
        hover_data={"Wasted": True, "wasted_bytes": False},
    )
    fig_waste.update_layout(
        coloraxis_showscale=False,
        margin=dict(t=40, b=0, l=0, r=0),
    )
    # Format y-axis ticks as human-readable sizes.
    fig_waste.update_yaxes(
        tickformat=".2s",   # SI prefix formatting: 1.2G, 500M, etc.
    )
    st.plotly_chart(fig_waste, use_container_width=True)

# ── Learning Notes ────────────────────────────────────────────────────────────
# Key concepts used in this file:
#
# groupby("content_hash")        — splits flat DataFrame into per-hash groups
# groups.groups.keys()           — ordered list of unique group keys for pagination
# groups.get_group(key)          — retrieves one group's rows as a DataFrame
# .mode().iloc[0]                — most common value in a column
# st.session_state               — persists values (like confirm_delete) across reruns
# st.button(type="primary")      — blue/red emphasis button for primary actions
# st.checkbox() confirmation      — extra safety gate before destructive operations
# st.cache_data.clear()          — invalidate all cached queries after a mutation
# st.number_input(step=1)        — integer-only input for page numbers
# px.bar(hover_data)             — add extra columns to the hover tooltip
# tickformat=".2s"               — Plotly SI-prefix formatting for axis labels
# st.stop()                      — abort page rendering (used for error/empty states)

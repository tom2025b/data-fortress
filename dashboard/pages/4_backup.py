"""
pages/4_backup.py — Backup Page
---------------------------------
Shows backup history and lets the user trigger a new backup via the Rust binary.

Features:
  - Backup history table (from SQLite backups table)
  - Compression ratio chart over time
  - "Create backup" form with label, category filter, compression level
  - Dry-run preview before committing
  - Space savings summary per backup

Data flow:
  - History: utils/db.py::get_backup_history() → SQLite (fast, read-only)
  - Create:  utils/fortress.py::run_backup_create() → Rust binary (slow, writes archive)
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

from utils.db import get_backup_history, get_overview_stats
from utils.fortress import run_backup_create, show_binary_status

# ── Page config ───────────────────────────────────────────────────────────────
st.set_page_config(
    page_title="Data Fortress — Backup",
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

# ── Page header ───────────────────────────────────────────────────────────────
st.title("Backup")
st.caption("Create and review versioned backups of your indexed files")
st.divider()

# ── Fetch existing backup history ─────────────────────────────────────────────
try:
    history_df = get_backup_history(db_path)
    stats      = get_overview_stats(db_path)

except FileNotFoundError:
    st.info(
        "No database found yet. Run your first scan:\n\n"
        "```bash\ndata-fortress scan /path/to/your/files\n```"
    )
    st.stop()

except Exception as e:
    st.error(f"Could not load backup history: {e}")
    st.stop()

# ── Section 1: Summary metrics ────────────────────────────────────────────────
n_backups = len(history_df)

col1, col2, col3 = st.columns(3)

with col1:
    st.metric("Total Backups", f"{n_backups:,}")

with col2:
    if n_backups > 0:
        total_archive_bytes = int(history_df["compressed_bytes"].sum())
        st.metric(
            "Total Archive Size",
            humanize.naturalsize(total_archive_bytes, binary=True),
        )
    else:
        st.metric("Total Archive Size", "—")

with col3:
    if n_backups > 0:
        # Overall compression ratio across all backups.
        total_original   = int(history_df["original_bytes"].sum())
        total_compressed = int(history_df["compressed_bytes"].sum())
        if total_original > 0:
            ratio = 100 * (1 - total_compressed / total_original)
            st.metric("Avg Compression", f"{ratio:.1f}%")
        else:
            st.metric("Avg Compression", "—")
    else:
        st.metric("Avg Compression", "—")

st.divider()

# ── Section 2: Create new backup ──────────────────────────────────────────────
st.subheader("Create Backup")

# Use st.form so all inputs are submitted together with a single button click.
# This prevents Streamlit from rerunning on every widget change.
with st.form("create_backup_form"):
    form_col1, form_col2 = st.columns(2)

    with form_col1:
        # Optional human-readable label for this backup.
        backup_label = st.text_input(
            "Label (optional)",
            placeholder="e.g. before-migration, monthly-2026-04",
            help="Short name for this backup. Auto-generated if left blank.",
        )

        # Optional category filter — only back up files of a specific type.
        CATEGORIES = ["All files", "image", "video", "audio", "document", "archive", "code", "other"]
        cat_choice = st.selectbox(
            "Category filter",
            options=CATEGORIES,
            index=0,
            help="Only include files of this type in the archive.",
        )
        category_param = None if cat_choice == "All files" else cat_choice

    with form_col2:
        # zstd compression level: 1 (fastest) to 22 (smallest).
        # Level 3 is the zstd default — good balance of speed and size.
        compression = st.slider(
            "Compression level (zstd)",
            min_value=1,
            max_value=22,
            value=3,
            help=(
                "1 = fastest (largest file), 22 = smallest (slowest). "
                "Level 3 is the recommended default."
            ),
        )

        dry_run = st.checkbox(
            "Dry run (preview only)",
            value=True,   # Default to safe preview mode
            help="Show what would be included without writing an archive file.",
        )

    # The form submit button — must be inside the form block.
    submitted = st.form_submit_button(
        "Preview backup" if dry_run else "Create backup",
        type="primary",
        use_container_width=True,
    )

# ── Execute backup ────────────────────────────────────────────────────────────
if submitted:
    label_param = backup_label.strip() or None  # Empty string → None (auto-generate)

    action_label = "Previewing" if dry_run else "Creating"
    with st.spinner(f"{action_label} backup…"):
        try:
            report = run_backup_create(
                label=label_param,
                category=category_param,
                compression=compression,
                dry_run=dry_run,
            )

            # ── Show results ─────────────────────────────────────────────────
            prefix = "Dry-run preview" if dry_run else "Backup created"
            st.success(f"{prefix} — results below")

            r1, r2, r3, r4 = st.columns(4)

            r1.metric(
                "Files Included",
                f"{report.get('files_included', 0):,}",
            )
            r2.metric(
                "Original Size",
                humanize.naturalsize(report.get("original_bytes", 0), binary=True),
            )
            r3.metric(
                "Compressed Size",
                humanize.naturalsize(report.get("compressed_bytes", 0), binary=True),
            )

            # Compute compression ratio for this backup.
            orig = report.get("original_bytes", 0)
            comp = report.get("compressed_bytes", 0)
            ratio_str = (
                f"{100 * (1 - comp / orig):.1f}%" if orig > 0 else "—"
            )
            r4.metric("Compression", ratio_str)

            # Show skipped count if any files were excluded.
            skipped = report.get("skipped", 0)
            if skipped:
                st.caption(f"{skipped:,} files skipped (permissions or read errors).")

            # Show the archive path for real (non-dry) backups.
            if not dry_run:
                archive_path = report.get("archive_path", "")
                manifest_path = report.get("manifest_path", "")
                if archive_path:
                    st.markdown("**Archive written to:**")
                    st.code(archive_path, language=None)
                if manifest_path:
                    st.markdown("**Manifest:**")
                    st.code(manifest_path, language=None)

                # Clear the backup history cache so the table refreshes.
                st.cache_data.clear()

        except RuntimeError as e:
            st.error(f"Backup failed: {e}")

st.divider()

# ── Section 3: Backup history table ──────────────────────────────────────────
st.subheader("Backup History")

if history_df.empty:
    st.info(
        "No backups yet. Use the form above to create your first backup.\n\n"
        "Or run from the command line:\n\n"
        "```bash\ndata-fortress backup create --label my-first-backup\n```"
    )
else:
    # Build a display-friendly version of the history DataFrame.
    display_history = history_df.copy()

    # Add human-readable size columns.
    display_history["original"] = display_history["original_bytes"].apply(
        lambda b: humanize.naturalsize(int(b), binary=True)
    )
    display_history["compressed"] = display_history["compressed_bytes"].apply(
        lambda b: humanize.naturalsize(int(b), binary=True)
    )

    # Compute per-row compression ratio.
    def _ratio(row) -> str:
        orig = row["original_bytes"]
        comp = row["compressed_bytes"]
        if orig and orig > 0:
            return f"{100 * (1 - comp / orig):.1f}%"
        return "—"

    display_history["ratio"] = display_history.apply(_ratio, axis=1)

    # Select and rename columns for the display table.
    display_history = display_history.rename(columns={
        "label":       "Label",
        "original":    "Original",
        "compressed":  "Compressed",
        "ratio":       "Ratio",
        "algorithm":   "Algorithm",
        "created_at":  "Created",
        "archive_path": "Archive Path",
    })[["Label", "Original", "Compressed", "Ratio", "Algorithm", "Created", "Archive Path"]]

    st.dataframe(
        display_history,
        use_container_width=True,
        hide_index=True,
        column_config={
            "Archive Path": st.column_config.TextColumn(max_chars=80),
            "Created":      st.column_config.TextColumn(max_chars=19),
        },
    )

    st.divider()

    # ── Section 4: Compression ratio chart ───────────────────────────────────
    st.subheader("Compression Over Time")

    # Re-add the ratio as a float for charting.
    chart_df = history_df.copy()
    chart_df["compression_pct"] = chart_df.apply(
        lambda r: (
            100 * (1 - r["compressed_bytes"] / r["original_bytes"])
            if r["original_bytes"] > 0 else 0
        ),
        axis=1,
    )
    # Parse created_at to datetime so Plotly renders it as a proper time axis.
    chart_df["created_at"] = chart_df["created_at"].str[:19]  # Trim sub-seconds

    if len(chart_df) >= 2:
        fig = px.scatter(
            chart_df,
            x="created_at",
            y="compression_pct",
            size="original_bytes",        # Bubble size = original file size
            hover_name="label",
            hover_data={
                "created_at":      True,
                "compression_pct": ":.1f",
                "original_bytes":  False,  # Raw bytes — hidden (size conveys it)
            },
            labels={
                "created_at":      "Created",
                "compression_pct": "Compression (%)",
            },
            title="Compression Ratio per Backup (bubble = original size)",
            color_discrete_sequence=["#4C78A8"],
        )
        fig.update_layout(margin=dict(t=40, b=0, l=0, r=0))
        fig.update_xaxes(type="date")
        st.plotly_chart(fig, use_container_width=True)
    else:
        # A single data point isn't meaningful as a time chart.
        st.caption("Create more backups to see compression trends over time.")

    # ── Section 5: Storage savings summary ───────────────────────────────────
    st.divider()
    st.subheader("Cumulative Savings")

    total_orig_bytes = int(history_df["original_bytes"].sum())
    total_comp_bytes = int(history_df["compressed_bytes"].sum())
    saved_bytes      = max(0, total_orig_bytes - total_comp_bytes)

    s1, s2, s3 = st.columns(3)
    s1.metric(
        "Total Data Backed Up",
        humanize.naturalsize(total_orig_bytes, binary=True),
    )
    s2.metric(
        "Total Archive Size",
        humanize.naturalsize(total_comp_bytes, binary=True),
    )
    s3.metric(
        "Space Saved by Compression",
        humanize.naturalsize(saved_bytes, binary=True),
        delta=f"-{humanize.naturalsize(saved_bytes, binary=True)}",
        delta_color="normal",   # Green = good (compression saved space)
    )

# ── Learning Notes ────────────────────────────────────────────────────────────
# Key concepts used in this file:
#
# st.form("key")                  — groups widgets; submits all values together
# st.form_submit_button()         — the submit trigger; must be inside the form
# st.slider(min, max, value)      — numeric range input; good for compression level
# st.selectbox(options, index)    — dropdown; index=0 selects the first option
# st.text_input(placeholder)      — single-line text; placeholder explains expected input
# st.checkbox(value=True)         — boolean toggle; default to safe dry-run
# df.apply(_ratio, axis=1)        — apply a row-wise function to compute a new column
# st.cache_data.clear()           — invalidate all query caches after a write operation
# px.scatter(size="col")          — bubble chart where size encodes a third dimension
# hover_data={"col": ":.1f"}      — format a hover column with a Python format string
# humanize.naturalsize(binary=True) — KiB/MiB/GiB instead of KB/MB/GB
# label_param = label or None     — convert empty string to None before passing to API

# Data Fortress — Architecture

## Overview

Data Fortress is a personal file management system built as a **monolithic Rust binary** with a **Python Streamlit dashboard**. The binary is the single source of truth for all mutations (scanning, deduplication, backup creation, file organization). The dashboard is purely read-oriented: it queries SQLite directly for display and shells out to the binary only when triggering an action.

```
┌──────────────────────────────────────────────────────────────────┐
│                        User interfaces                           │
│                                                                  │
│   CLI: data-fortress scan / dedup / search / organize / backup   │
│   Web: Streamlit dashboard (dashboard/app.py + pages/)           │
└────────────────────┬───────────────────────────────┬────────────┘
                     │ CLI args                       │ subprocess
                     ▼                               ▼
┌──────────────────────────────────┐   ┌─────────────────────────┐
│       Rust binary (src/)         │   │  Python dashboard reads  │
│                                  │   │  SQLite directly for     │
│  main.rs → cmd_* dispatch        │◄──│  display (utils/db.py)   │
│                                  │   └─────────────────────────┘
│  ┌────────┐  ┌───────┐           │
│  │scanner │  │ dedup │           │
│  ├────────┤  ├───────┤           │
│  │organiz.│  │search │           │
│  ├────────┤  ├───────┤           │
│  │ backup │  │  db   │           │
│  └────────┘  └───────┘           │
└──────────────────┬───────────────┘
                   │ rusqlite (bundled)
                   ▼
┌──────────────────────────────────┐
│  SQLite database (fortress.db)   │
│  ~/.local/share/data-fortress/   │
│                                  │
│  tables: files, backups          │
└──────────────────────────────────┘
```

---

## Repository layout

```
data-fortress/
├── Cargo.toml                  Single-crate binary; all deps declared here
├── Makefile                    Common dev tasks (build, test, dashboard, install)
├── .gitignore
│
├── src/                        Rust source — one module per subsystem
│   ├── main.rs                 Entry point; CLI dispatch; logging init
│   ├── models.rs               Shared data types (FileRecord, ScanStats, …)
│   ├── error.rs                FortressError enum; FortressResult<T> type alias
│   ├── config.rs               Config struct; JSON load/save; XDG path resolution
│   ├── db.rs                   SQLite helpers (open, schema, upsert, queries)
│   ├── cli.rs                  Clap 4 derive-based CLI definition
│   ├── scanner/
│   │   ├── mod.rs              Scan orchestration; walkdir loop; two-phase design
│   │   └── classifier.rs       MIME + category detection (magic bytes → extension)
│   ├── dedup/
│   │   ├── mod.rs              Dedup orchestration; group selection; deletion
│   │   └── hasher.rs           BLAKE3 streaming hash; parallel hash_files_parallel()
│   ├── organizer/
│   │   └── mod.rs              File move logic; undo log; OrganizeMode dispatch
│   ├── search/
│   │   ├── mod.rs              Score-weighted search; query tokenisation
│   │   ├── extractor.rs        Text extraction (PDF, DOCX, PPTX, XLSX, plain text)
│   │   └── exif.rs             EXIF metadata extraction; GPS DMS→decimal conversion
│   └── backup/
│       └── mod.rs              TAR+zstd streaming archive; manifest; DB record
│
├── tests/                      Black-box integration tests (assert_cmd)
│   ├── test_scanner.rs
│   ├── test_dedup.rs
│   └── fixtures/               Static test files (sample.txt, sample.rs)
│
├── dashboard/                  Python Streamlit web UI
│   ├── app.py                  Root page; sidebar; session state; landing metrics
│   ├── requirements.txt        streamlit, plotly, pandas, humanize
│   ├── utils/
│   │   ├── __init__.py
│   │   ├── db.py               Read-only SQLite helpers with @st.cache_data
│   │   └── fortress.py         Subprocess wrapper for the Rust binary
│   └── pages/
│       ├── 1_overview.py       Storage metrics, category charts, scan history
│       ├── 2_duplicates.py     Duplicate groups browser; dedup action buttons
│       ├── 3_search.py         Metadata + content search interface
│       └── 4_backup.py         Backup history; create backup form
│
├── scripts/
│   ├── build-release.sh        cargo build --release with size report
│   ├── install.sh              Build → ~/bin + Python venv setup
│   └── run-dashboard.sh        Ensure binary exists → activate venv → streamlit run
│
└── docs/
    └── architecture.md         This file
```

---

## Key design decisions

### 1. Monolithic binary, not a workspace

All Rust code lives in a single crate (`src/`). Internal modules communicate through normal function calls — no crate boundaries, no cross-crate dependency management. This trades some theoretical compile-time parallelism for a dramatically simpler development experience: one `cargo build`, one binary, one `Cargo.toml`.

### 2. SQLite as the data layer

`rusqlite` with the `bundled` feature compiles SQLite into the binary — no system library dependency. The database uses:

- **WAL mode** (`PRAGMA journal_mode = WAL`) — allows concurrent readers while a writer is active. The dashboard can query while a scan runs.
- **Foreign key enforcement** (`PRAGMA foreign_keys = ON`) — prevents orphaned records.
- **`ON CONFLICT DO UPDATE`** (upsert) — re-scanning the same file updates its record rather than duplicating it.

### 3. Two-phase scan design

Detecting deleted files requires knowing which files existed before the scan. The scanner solves this without storing a separate snapshot:

```
Phase 1 — mark_all_absent(dir):
    UPDATE files SET is_present = 0 WHERE path LIKE '<dir>%'

Phase 2 — walk the directory:
    for each file on disk:
        upsert_file(record)
        mark_present(path)    ← SET is_present = 1
```

After phase 2, any file that was present before the scan but not found on disk retains `is_present = 0`. No extra bookkeeping required.

### 4. Parallel hashing, serial writes

BLAKE3 hashing is CPU-bound and embarrassingly parallel. Database writes require exclusive access to the connection (SQLite's `Connection` is not `Send`). The solution:

```rust
// Parallel: hash all files concurrently on rayon's thread pool
let results: Vec<HashResult> = paths.par_iter()
    .map(|p| hash_file(p))
    .collect();

// Serial: write all hashes in a single transaction
let tx = conn.transaction()?;
for r in results { tx.execute(SET_HASH_SQL, [&r.hash, &r.path])?; }
tx.commit()?;
```

One transaction for the entire batch is also faster than per-file commits (each commit is a disk sync).

### 5. Duplicate detection via BLAKE3

Files are considered identical if their BLAKE3 hashes match. BLAKE3 properties relevant here:

- **Collision resistance** — two different files producing the same hash is computationally infeasible.
- **Speed** — faster than SHA-256 on modern hardware, especially with SIMD.
- **Streaming** — hashed in 1 MiB chunks, so arbitrarily large files don't need to be loaded into memory.

### 6. Dashboard communication split

| Operation | Path | Why |
|-----------|------|-----|
| Display stats, search results, history | Python → SQLite directly | Fast; no subprocess overhead; SQLite supports concurrent readers |
| Trigger scan, dedup, backup | Python → subprocess → Rust binary | Rust is the authoritative implementation; avoids duplicating mutation logic in Python |

The dashboard's `utils/fortress.py` always passes `--json` so the binary outputs structured data that Python can parse directly.

### 7. Error handling layers

```
Internal code:  anyhow::Result<T>     — easy error propagation with `?`
Public API:     FortressResult<T>     — typed FortressError enum for callers
main.rs:        match err → eprintln! + process::exit(1)
Dashboard:      try/except RuntimeError around subprocess calls
```

`thiserror` generates `Display` and `Error` trait implementations for `FortressError` from `#[error("...")]` attributes, avoiding boilerplate.

---

## Data model

### `files` table

| Column | Type | Notes |
|--------|------|-------|
| `id` | INTEGER PRIMARY KEY | Auto-increment |
| `path` | TEXT UNIQUE NOT NULL | Absolute path — the natural key |
| `name` | TEXT NOT NULL | Filename without directory |
| `extension` | TEXT | Lowercase, no dot |
| `category` | TEXT | image / video / audio / document / archive / code / other |
| `mime_type` | TEXT | e.g. `image/png`, `text/x-rust` |
| `size_bytes` | INTEGER | Stored as i64 in SQLite; cast to u64 in Rust |
| `content_hash` | TEXT | BLAKE3 hex (64 chars); NULL until `--hash` is run |
| `modified_at` | TEXT | RFC 3339 timestamp |
| `scanned_at` | TEXT | RFC 3339 timestamp of last scan that saw this file |
| `is_present` | INTEGER | 1 = on disk; 0 = was indexed but now missing |

Indexes: `content_hash WHERE content_hash IS NOT NULL`, `is_present`, `category`.

### `backups` table

| Column | Type | Notes |
|--------|------|-------|
| `id` | INTEGER PRIMARY KEY | |
| `label` | TEXT NOT NULL | Human-readable name |
| `archive_path` | TEXT NOT NULL | Absolute path to `.tar.zst` file |
| `manifest_path` | TEXT NOT NULL | Absolute path to `.json` manifest |
| `original_bytes` | INTEGER | Total uncompressed size |
| `compressed_bytes` | INTEGER | Archive size on disk |
| `algorithm` | TEXT | Always `"zstd"` for now |
| `created_at` | TEXT | RFC 3339 timestamp |

---

## Module responsibilities

### `scanner/`

- **`classifier.rs`** — stateless file type detection. Reads 16 bytes of magic, falls back to extension, falls back to `application/octet-stream`. No I/O side effects.
- **`mod.rs`** — scan orchestration. Walks directories with `walkdir`, calls `classifier`, writes `FileRecord` to SQLite. Optionally hashes files with rayon after the walk.

### `dedup/`

- **`hasher.rs`** — pure BLAKE3 hashing. `hash_file()` streams a file in 1 MiB chunks. `hash_files_parallel()` scatters work across rayon's thread pool.
- **`mod.rs`** — dedup orchestration. Queries duplicate groups from SQLite, applies `min_size` filter, selects the keeper via `KeepStrategy`, deletes the rest (or previews in `--dry-run`).

### `organizer/`

- **`mod.rs`** — file movement. Computes destination paths based on `OrganizeMode` (by-type-and-date / by-date / by-type). Uses `fs::rename` with an `EXDEV` fallback to `copy + delete` for cross-device moves. Writes an undo log (`.fortress_undo.json`) for reversibility.

### `search/`

- **`extractor.rs`** — text extraction from document formats: `pdf-extract` for PDFs, `zip + quick-xml` for DOCX/PPTX, `calamine` for XLSX, `fs::read` for plain text. Returns `Option<String>` — `None` for binary files that yield no text.
- **`exif.rs`** — EXIF metadata extraction via `kamadak-exif`. Reads camera make/model, GPS coordinates (DMS → decimal), date taken, and image dimensions. Builds a `searchable_text` string for scoring.
- **`mod.rs`** — score-weighted ranking. Tokenises the query, scores each candidate file, sorts by score descending. Score weights: `NAME_EXACT=10`, `NAME_TOKEN=4`, `CONTENT_TOKEN=3`, `EXIF_TOKEN=2`, `PATH_TOKEN=1.5`.

### `backup/`

- **`mod.rs`** — TAR+zstd streaming archive creation. Pipeline: `File → BufWriter → zstd::Encoder → tar::Builder`. Files are appended with leading `/` stripped from paths. Writes a JSON manifest alongside the archive. Records the backup in the `backups` table.

---

## Dependency rationale

| Crate | Role | Why this one |
|-------|------|-------------|
| `clap 4` | CLI parsing | Industry standard; derive macros; shell completion generation |
| `rusqlite` (bundled) | SQLite | No system lib dependency; WAL support; good ergonomics |
| `rayon` | Parallelism | Work-stealing thread pool; `par_iter()` is drop-in for `iter()` |
| `blake3` | Content hashing | Faster than SHA-256; streaming API; 256-bit collision resistance |
| `walkdir` | Directory traversal | Handles symlinks, `same_file_system`, `filter_entry` pruning |
| `infer` | Magic-byte detection | Pure Rust; no native deps; covers 150+ file types |
| `mime_guess` | Extension→MIME | Fast fallback when magic bytes are inconclusive |
| `kamadak-exif` | EXIF reading | Pure Rust; handles malformed EXIF gracefully |
| `pdf-extract` | PDF text | Pure Rust; no poppler/ghostscript dep |
| `zstd` | Compression | Meta's algorithm; excellent ratio/speed trade-off; `zstdmt` for multi-threading |
| `tar` | Archive format | Streaming; no random access needed for creation |
| `serde` / `serde_json` | Serialization | JSON IPC between Rust binary and Python dashboard |
| `thiserror` | Error types | Generates `Display` + `Error` impls from attributes |
| `anyhow` | Error propagation | `?` works everywhere internally; rich context chains |
| `dirs` | XDG directories | Reads `XDG_DATA_HOME`, `XDG_CONFIG_HOME` — correct on all Linux DEs |
| `tracing` | Structured logging | Async-compatible; `RUST_LOG` env var; no performance cost when disabled |
| `indicatif` | Progress display | Spinner + ETA; plays nicely with tracing |
| `humanize` (Python) | Byte formatting | `naturalsize(n, binary=True)` → `"1.2 GiB"` |
| `plotly` (Python) | Interactive charts | Browser-native; works well with Streamlit's component model |

---

## Running locally

```bash
# Build the release binary
make build-release

# Run the dashboard (also builds debug binary if missing)
make dashboard

# Run all tests
make test

# Install to ~/bin + set up Python venv
./scripts/install.sh

# Scan a directory and hash files
data-fortress scan ~/Documents --hash

# Find duplicates (dry run first)
data-fortress dedup --dry-run
data-fortress dedup --delete --keep oldest

# Search files
data-fortress search "invoice 2024" --category document
data-fortress search "paris eiffel" --content   # full content + EXIF search

# Create a backup
data-fortress backup create --label "before-cleanup" --compression 5
```

Environment variable overrides:

| Variable | Effect |
|----------|--------|
| `RUST_LOG` | Set log verbosity: `error`, `warn`, `info`, `debug`, `trace` |
| `XDG_DATA_HOME` | Override data directory (database, backups) |
| `XDG_CONFIG_HOME` | Override config directory |
| `FORTRESS_CONFIG` | Override config file path (also settable with `--config`) |

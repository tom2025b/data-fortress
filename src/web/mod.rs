//! src/web/mod.rs — Axum web dashboard for Data Fortress
//!
//! Serves a read-only HTML dashboard over HTTP using:
//!   - Axum 0.7  — async HTTP routing
//!   - Askama    — compile-time Jinja2-style HTML templates (no runtime cost)
//!   - HTMX      — live search via HTML attributes, no JS boilerplate
//!   - Tailwind  — utility CSS via CDN (no build step)
//!
//! Architecture rules (matches the Python dashboard's design contract):
//!   READ-ONLY  — this module never writes to the database.
//!   REAL DATA  — all handlers call actual SQL queries; no fake hardcoded values.
//!   BLOCKING   — rusqlite is synchronous; every DB call is wrapped in
//!                `spawn_blocking` so it doesn't stall the async executor.
//!
//! Thread safety:
//!   `rusqlite::Connection` is `Send` but NOT `Sync`. Axum's state must be
//!   `Clone + Send + Sync`. Solution: `Arc<Mutex<Connection>>` — the Mutex
//!   makes it Sync, the Arc makes it cheaply cloneable across handler tasks.

use std::sync::{Arc, Mutex};

use askama::Template;
use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use bytesize::ByteSize;
use rusqlite::Connection;
use serde::Deserialize;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;
use tracing::info;

// =============================================================================
// App State
// =============================================================================

/// Shared state injected into every Axum handler via `State<AppState>`.
///
/// `#[derive(Clone)]` is required by Axum — it clones the state per request.
/// Cloning an `Arc` is cheap (increments a reference count; no deep copy).
#[derive(Clone)]
pub struct AppState {
    /// Mutex-protected database connection.
    ///
    /// The Mutex serialises access so only one handler queries the DB at a time.
    /// This is acceptable for a single-user personal tool with low concurrency.
    pub db: Arc<Mutex<Connection>>,
}

// =============================================================================
// Template structs
// =============================================================================
// Each struct maps to one HTML template file in templates/.
// Askama renders them at compile time — typos in field names are caught by
// `cargo build`, not at runtime.

/// Overview page — storage summary, category breakdown.
#[derive(Template)]
#[template(path = "overview.html")]
struct OverviewTemplate {
    total_files:      u64,
    total_bytes:      String,   // Human-readable, e.g. "14.2 GiB"
    duplicate_groups: u64,
    wasted_bytes:     String,
    last_scan:        String,   // YYYY-MM-DD or "Never"
    categories:       Vec<CategoryRow>,
}

/// One row in the category breakdown table on the overview page.
struct CategoryRow {
    name:  String,
    count: u64,
    bytes: String,  // Human-readable
}

/// Duplicates page — all duplicate groups.
#[derive(Template)]
#[template(path = "duplicates.html")]
struct DuplicatesTemplate {
    groups:        Vec<DupGroupRow>,
    total_wasted:  String,
    group_count:   usize,
}

/// One duplicate group (files sharing the same BLAKE3 hash).
struct DupGroupRow {
    hash_preview: String,       // First 12 chars of the 64-char hash
    file_count:   usize,
    size_each:    String,       // All copies are the same size
    wasted:       String,       // size × (copies − 1)
    paths:        Vec<String>,  // Absolute paths of every copy
}

/// Search page — query box; results rendered by the HTMX partial below.
#[derive(Template)]
#[template(path = "search.html")]
struct SearchTemplate {
    query: String,
}

/// HTMX partial — rendered in response to `GET /api/search?q=...`.
/// Replaces only the results container, not the whole page.
#[derive(Template)]
#[template(path = "search_results.html")]
struct SearchResultsTemplate {
    results: Vec<SearchRow>,
    query:   String,
    count:   usize,
}

/// One row in the search results table.
struct SearchRow {
    name:     String,
    path:     String,
    size:     String,
    category: String,
    modified: String,  // YYYY-MM-DD
}

/// Backup history page.
#[derive(Template)]
#[template(path = "backup.html")]
struct BackupTemplate {
    backups: Vec<BackupRow>,
    total_original:   String,
    total_compressed: String,
    total_saved:      String,
}

/// One row in the backup history table.
struct BackupRow {
    label:        String,
    created_at:   String,
    original:     String,
    compressed:   String,
    ratio:        String,   // "42.3%"
    archive_path: String,
}

// =============================================================================
// Router
// =============================================================================

/// Build the Axum router with all routes attached.
///
/// Called once at startup. The state is cloned into each handler by Axum.
pub fn create_router(state: AppState) -> Router {
    Router::new()
        // HTML pages
        .route("/",           get(overview_handler))
        .route("/duplicates", get(duplicates_handler))
        .route("/search",     get(search_handler))
        .route("/backup",     get(backup_handler))
        // HTMX API — returns HTML fragments, not full pages
        .route("/api/search", get(search_api_handler))
        // Static assets (CSS overrides, icons, etc.) served from static/
        // Falls back gracefully if the directory doesn't exist yet.
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state)
}

// =============================================================================
// Handlers
// =============================================================================
// Each handler delegates to `run_db`, which runs the synchronous SQL query on
// the blocking thread-pool and collapses both failure modes to a `String`.
// The handler then maps Ok to a rendered template and Err to an error page.

/// `GET /` — Overview page.
async fn overview_handler(State(state): State<AppState>) -> Response {
    match run_db(state.db, query_overview).await {
        Ok(tmpl) => tmpl.into_response(),
        Err(msg) => error_page(&msg),
    }
}

/// `GET /duplicates` — Duplicate groups page.
async fn duplicates_handler(State(state): State<AppState>) -> Response {
    match run_db(state.db, query_duplicates).await {
        Ok(tmpl) => tmpl.into_response(),
        Err(msg) => error_page(&msg),
    }
}

/// `GET /search` — Search page (empty query box; results loaded by HTMX).
async fn search_handler() -> impl IntoResponse {
    SearchTemplate { query: String::new() }
}

/// `GET /api/search?q=<query>&category=<cat>` — HTMX search results partial.
///
/// Returns an HTML fragment (not a full page) that HTMX swaps into the DOM.
async fn search_api_handler(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> Response {
    let query = params.q.unwrap_or_default().trim().to_string();
    let category = params.category;

    // Return an empty fragment for blank queries instead of a full table scan.
    if query.is_empty() {
        return SearchResultsTemplate {
            results: vec![],
            query:   String::new(),
            count:   0,
        }
        .into_response();
    }

    let q = query.clone();
    let c = category.clone();

    match run_db(state.db, move |conn| query_search(conn, &q, c.as_deref(), 100)).await {
        Ok(rows) => {
            let count = rows.len();
            SearchResultsTemplate { results: rows, query, count }.into_response()
        }
        Err(msg) => error_fragment(&msg),
    }
}

/// `GET /backup` — Backup history page.
async fn backup_handler(State(state): State<AppState>) -> Response {
    match run_db(state.db, query_backups).await {
        Ok(tmpl) => tmpl.into_response(),
        Err(msg) => error_page(&msg),
    }
}

// =============================================================================
// Query parameters
// =============================================================================

/// Deserialized from `?q=...&category=...` on the search endpoint.
#[derive(Deserialize)]
struct SearchQuery {
    q:        Option<String>,
    category: Option<String>,
}

// =============================================================================
// Database query helpers (synchronous — called inside spawn_blocking)
// =============================================================================
// These functions take a plain `&Connection` — no async, no Arc, no Mutex.
// The locking is handled by the caller in the handler above.

/// Query all data needed for the overview page.
fn query_overview(conn: &Connection) -> rusqlite::Result<OverviewTemplate> {
    // Total file count and storage used.
    let (total_files, total_raw_bytes): (u64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0)
         FROM files WHERE is_present = 1",
        [],
        |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)?)),
    )?;

    // Duplicate group count and wasted bytes.
    // A group = content_hash appearing > 1 time among present files.
    let (dup_groups, wasted_raw): (u64, i64) = conn
        .query_row(
            "SELECT
                COUNT(DISTINCT content_hash),
                COALESCE(SUM(size_bytes) - SUM(min_size), 0)
             FROM (
                 SELECT content_hash,
                        size_bytes,
                        MIN(size_bytes) OVER (PARTITION BY content_hash) AS min_size
                 FROM files
                 WHERE content_hash IS NOT NULL AND is_present = 1
                 GROUP BY path
             )
             WHERE content_hash IN (
                 SELECT content_hash FROM files
                 WHERE content_hash IS NOT NULL AND is_present = 1
                 GROUP BY content_hash HAVING COUNT(*) > 1
             )",
            [],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)?)),
        )
        .unwrap_or((0, 0));

    // Most recent scan timestamp.
    let last_scan: String = conn
        .query_row(
            "SELECT COALESCE(MAX(scanned_at), 'Never') FROM files",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "Never".into());

    // Per-category breakdown.
    let mut stmt = conn.prepare(
        "SELECT category, COUNT(*) as n, COALESCE(SUM(size_bytes), 0) as b
         FROM files WHERE is_present = 1
         GROUP BY category ORDER BY b DESC",
    )?;
    let categories: Vec<CategoryRow> = stmt
        .query_map([], |row| {
            Ok(CategoryRow {
                name:  row.get::<_, String>(0)?,
                count: row.get::<_, i64>(1)? as u64,
                bytes: fmt_bytes(row.get::<_, i64>(2)? as u64),
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(OverviewTemplate {
        total_files,
        total_bytes:      fmt_bytes(total_raw_bytes as u64),
        duplicate_groups: dup_groups,
        wasted_bytes:     fmt_bytes(wasted_raw.max(0) as u64),
        last_scan:        last_scan.chars().take(10).collect(),
        categories,
    })
}

/// Query all duplicate groups for the duplicates page.
fn query_duplicates(conn: &Connection) -> rusqlite::Result<DuplicatesTemplate> {
    // First: find all hashes that appear more than once.
    let mut hash_stmt = conn.prepare(
        "SELECT content_hash, COUNT(*) as copies, MAX(size_bytes) as size
         FROM files
         WHERE content_hash IS NOT NULL AND is_present = 1
         GROUP BY content_hash
         HAVING copies > 1
         ORDER BY size DESC, content_hash",
    )?;

    let hash_rows: Vec<(String, usize, u64)> = hash_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, i64>(2)? as u64,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut groups    = Vec::with_capacity(hash_rows.len());
    let mut total_wasted_bytes: u64 = 0;

    for (hash, copies, size_bytes) in hash_rows {
        // For each hash, fetch all the file paths.
        let mut path_stmt = conn.prepare(
            "SELECT path FROM files
             WHERE content_hash = ?1 AND is_present = 1
             ORDER BY path",
        )?;
        let paths: Vec<String> = path_stmt
            .query_map([&hash], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let wasted = size_bytes * (copies as u64 - 1);
        total_wasted_bytes += wasted;

        groups.push(DupGroupRow {
            hash_preview: hash[..12].to_string(),
            file_count:   copies,
            size_each:    fmt_bytes(size_bytes),
            wasted:       fmt_bytes(wasted),
            paths,
        });
    }

    let group_count = groups.len();
    Ok(DuplicatesTemplate {
        groups,
        total_wasted: fmt_bytes(total_wasted_bytes),
        group_count,
    })
}

/// Search files by name/path using SQLite LIKE.
fn query_search(
    conn: &Connection,
    query: &str,
    category: Option<&str>,
    limit: i64,
) -> rusqlite::Result<Vec<SearchRow>> {
    // `%query%` matches the query string anywhere in the column value.
    let pattern = format!("%{query}%");

    // Build the SQL dynamically based on whether a category filter is active.
    // We avoid string interpolation for the user-supplied `query` (uses `?`
    // placeholders) but the category is matched against our known enum values.
    let sql = if category.is_some() {
        "SELECT name, path, size_bytes, category, modified_at
         FROM files
         WHERE is_present = 1
           AND (name LIKE ?1 OR path LIKE ?1)
           AND category = ?2
         ORDER BY name LIMIT ?3"
    } else {
        "SELECT name, path, size_bytes, category, modified_at
         FROM files
         WHERE is_present = 1
           AND (name LIKE ?1 OR path LIKE ?1)
         ORDER BY name LIMIT ?3"
    };

    let mut stmt = conn.prepare(sql)?;

    // `rusqlite::params!` builds a type-safe parameter list.
    // We use two branches because the parameter count differs.
    let rows: Vec<SearchRow> = if let Some(cat) = category {
        stmt.query_map(rusqlite::params![pattern, cat, limit], map_search_row)?
            .filter_map(|r| r.ok())
            .collect()
    } else {
        stmt.query_map(rusqlite::params![pattern, limit], map_search_row)?
            .filter_map(|r| r.ok())
            .collect()
    };

    Ok(rows)
}

/// Map a `query_search` result row to a `SearchRow`.
fn map_search_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchRow> {
    Ok(SearchRow {
        name:     row.get(0)?,
        path:     row.get(1)?,
        size:     fmt_bytes(row.get::<_, i64>(2)? as u64),
        category: row.get(3)?,
        modified: row.get::<_, String>(4)?.chars().take(10).collect(),
    })
}

/// Query all backups for the backup history page.
fn query_backups(conn: &Connection) -> rusqlite::Result<BackupTemplate> {
    let mut stmt = conn.prepare(
        "SELECT label, archive_path, original_bytes, compressed_bytes, created_at
         FROM backups ORDER BY created_at DESC",
    )?;

    let mut total_original:   u64 = 0;
    let mut total_compressed: u64 = 0;

    // Build the Vec with a for loop so the accumulator updates are explicit
    // rather than hidden inside a `.map()` closure (which should be pure).
    let mut backups: Vec<BackupRow> = Vec::new();
    for row in stmt
        .query_map([], |row| {
            let label:        String = row.get(0)?;
            let archive_path: String = row.get(1)?;
            let orig:         i64    = row.get(2)?;
            let comp:         i64    = row.get(3)?;
            let created_at:   String = row.get(4)?;
            Ok((label, archive_path, orig as u64, comp as u64, created_at))
        })?
        .filter_map(|r| r.ok())
    {
        let (label, archive_path, orig, comp, created_at) = row;
        total_original   += orig;
        total_compressed += comp;

        let ratio = if orig > 0 {
            format!("{:.1}%", 100.0 * (1.0 - comp as f64 / orig as f64))
        } else {
            "—".into()
        };

        backups.push(BackupRow {
            label,
            created_at: created_at.chars().take(19).collect(),
            original:   fmt_bytes(orig),
            compressed: fmt_bytes(comp),
            ratio,
            archive_path,
        });
    }

    let saved = total_original.saturating_sub(total_compressed);

    Ok(BackupTemplate {
        backups,
        total_original:   fmt_bytes(total_original),
        total_compressed: fmt_bytes(total_compressed),
        total_saved:      fmt_bytes(saved),
    })
}

// =============================================================================
// Utilities
// =============================================================================

/// Run a synchronous database operation on the blocking thread-pool.
///
/// `rusqlite` is not async-aware, so every DB call must happen on a real OS
/// thread (via `spawn_blocking`) rather than inside the async executor, which
/// is a lightweight cooperative runtime — blocking it stalls all other tasks.
///
/// Both failure modes are collapsed to a `String` so callers decide how to
/// render the error (full-page vs. HTMX fragment):
///   - join error   — the blocking task panicked
///   - rusqlite error — bad query, schema mismatch, missing table, etc.
async fn run_db<F, T>(db: Arc<Mutex<Connection>>, f: F) -> Result<T, String>
where
    F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        // `unwrap_or_else(|p| p.into_inner())` recovers from mutex poisoning
        // (occurs if a previous thread panicked while holding the lock).
        let conn = db.lock().unwrap_or_else(|p| p.into_inner());
        f(&conn)
    })
    .await
    // Flatten the nested Result: JoinError first, then rusqlite::Error.
    .map_err(|e| format!("Task error: {e}"))?
    .map_err(|e| format!("Database error: {e}"))
}

/// Format raw bytes as a human-readable string using binary prefixes.
/// e.g. 1_073_741_824 → "1.0 GiB"
fn fmt_bytes(bytes: u64) -> String {
    ByteSize(bytes).to_string()
}

/// Return a full-page HTML error response (for page-level failures).
///
/// Uses inline CSS so the error page renders correctly even if static assets
/// or the CDN are unreachable (which is likely when something has gone wrong).
fn error_page(msg: &str) -> Response {
    Html(format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Error — Data Fortress</title>
  <style>
    *, *::before, *::after {{ box-sizing: border-box; }}
    body {{
      font-family: system-ui, sans-serif;
      background: #0f172a; color: #e2e8f0;
      display: flex; align-items: center; justify-content: center;
      min-height: 100vh; margin: 0; padding: 1rem;
    }}
    .card {{
      background: #1e293b;
      border: 1px solid rgba(239, 68, 68, 0.4);
      border-radius: .75rem;
      padding: 2rem 2.5rem;
      max-width: 480px; width: 100%;
    }}
    h1 {{ margin: 0 0 .75rem; color: #ef4444; font-size: 1.4rem; }}
    pre {{
      margin: 0 0 1.5rem; color: #94a3b8;
      font-size: .85rem; font-family: ui-monospace, monospace;
      white-space: pre-wrap; word-break: break-all;
    }}
    a {{ color: #38bdf8; text-decoration: none; font-size: .9rem; }}
    a:hover {{ text-decoration: underline; }}
  </style>
</head>
<body>
  <div class="card">
    <h1>Something went wrong</h1>
    <pre>{msg}</pre>
    <a href="/">&#8592; Back to overview</a>
  </div>
</body>
</html>"#
    ))
    .into_response()
}

/// Return a small HTML error fragment (for HTMX partial failures).
fn error_fragment(msg: &str) -> Response {
    Html(format!(
        r#"<p class="text-red-400 p-4">Error: {msg}</p>"#
    ))
    .into_response()
}

// =============================================================================
// Server entry point
// =============================================================================

/// Start the Axum HTTP server on the given host and port.
///
/// Opens the database at `db_path` (creating it if missing), wraps it in
/// `Arc<Mutex<_>>`, builds the router, and starts listening.
///
/// This function is `async` and blocks until the server is shut down (Ctrl-C).
pub async fn run(host: &str, port: u16, db_path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;

    // Open the SQLite connection. WAL mode and FK enforcement are set by db::open,
    // but we call Connection::open directly here so the web server can start
    // without duplicating the full init_schema logic on an already-initialised DB.
    let conn = crate::db::open(db_path)
        .with_context(|| format!("could not open database at {}", db_path.display()))?;

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
    };

    let app = create_router(state);

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("could not bind to {addr}"))?;

    info!("Data Fortress dashboard → http://localhost:{port}");
    info!("Press Ctrl-C to stop.");

    // `axum::serve` runs until the process is killed or Ctrl-C is pressed.
    axum::serve(listener, app)
        .await
        .context("server error")?;

    Ok(())
}

//! src/web/mod.rs — Axum web dashboard for Data Fortress
//!
//! Serves a read-only HTML dashboard over HTTP using:
//!   - Axum 0.7  — async HTTP routing
//!   - Askama    — compile-time Jinja2-style HTML templates (no runtime cost)
//!   - HTMX      — live search via HTML attributes, no JS boilerplate
//!   - Tailwind  — utility CSS via CDN (no build step)
//!
//! Architecture rules:
//!   MOSTLY READ-ONLY — page handlers never write; the /api/duplicates/* and
//!                      /api/backups/* POST routes do delete/create operations.
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
    extract::{Form, Query, State},
    http::{header::HeaderName, HeaderMap, HeaderValue},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use bytesize::ByteSize;
use rusqlite::{Connection, OptionalExtension};
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

    /// The loaded user config — needed by backup create so it knows where to
    /// write archives (`config.backup_dir`).
    ///
    /// Wrapped in Arc so it can be shared across handler tasks cheaply.
    pub config: Arc<crate::config::Config>,
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
    /// Full 64-char BLAKE3 hash — sent as the API key to the delete endpoints.
    hash:         String,
    /// First 12 chars of the hash — shown in the UI badge to save space.
    hash_preview: String,
    file_count:   usize,
    /// `file_count - 1` — pre-computed so the template avoids custom filters.
    delete_count: usize,
    size_each:    String,          // All copies are the same size
    wasted:       String,          // size × (copies − 1)
    paths:        Vec<PathInfo>,   // Paths annotated with keep/delete status
}

/// One file within a duplicate group, annotated for display.
///
/// The "newest" copy (highest `modified_at`) is the file that will be kept
/// when the user clicks "Keep Newest" — all others are candidates for deletion.
struct PathInfo {
    path:        String,  // Absolute filesystem path
    modified_at: String,  // YYYY-MM-DD — shown next to the path
    is_newest:   bool,    // true → shows a green KEEP badge; false → red DELETE
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
    /// SQLite primary key — sent to the delete endpoint as a form field.
    id:           i64,
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
        .route("/api/search",                     get(search_api_handler))
        // Duplicate management — delete files from disk, mark absent in DB.
        .route("/api/duplicates/keep-newest",     post(keep_newest_handler))
        .route("/api/duplicates/keep-newest-all", post(keep_newest_all_handler))
        // Backup management — create archives, delete old ones.
        .route("/api/backups/create",             post(create_backup_handler))
        .route("/api/backups/delete",             post(delete_backup_handler))
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

/// `POST /api/backups/create` — create a new backup archive.
///
/// Accepts an optional `label` form field. Runs `backup::create` on the
/// blocking thread pool (it compresses files and can take a while).
/// Returns an HTML fragment: a success card on success, an error on failure.
async fn create_backup_handler(
    State(state): State<AppState>,
    Form(form): Form<BackupCreateForm>,
) -> Response {
    // Clone what we need to move into the blocking closure.
    let db     = state.db.clone();
    let config = Arc::clone(&state.config);
    let label  = form.label.filter(|l| !l.trim().is_empty());

    let result = tokio::task::spawn_blocking(move || {
        let conn = db.lock().unwrap_or_else(|p| p.into_inner());

        // Build BackupCreateArgs with defaults — only label is user-supplied.
        let args = crate::cli::BackupCreateArgs {
            label,
            category:          None,  // back up all categories
            compression_level: 3,     // zstd level 3 — speed/ratio balance
            dry_run:           false,
        };

        crate::backup::create(&conn, &config, &args)
    })
    .await;

    match result {
        Ok(Ok(report)) => {
            // Show the archive path and stats so the user knows it worked.
            let saved = report.original_bytes.saturating_sub(report.compressed_bytes);
            Html(format!(
                r#"<div class="bg-green-900/30 border border-green-700 rounded-xl p-5">
                     <p class="text-green-400 font-semibold mb-1">✓ Backup created</p>
                     <p class="text-sm text-slate-300 font-mono break-all">{}</p>
                     <p class="text-xs text-slate-400 mt-2">
                       {} files &middot; {} → {} &middot; {} saved &middot; {}ms
                     </p>
                     <p class="text-xs text-slate-500 mt-2">Reload the page to see it in the list.</p>
                   </div>"#,
                report.archive_path.display(),
                report.files_included,
                fmt_bytes(report.original_bytes),
                fmt_bytes(report.compressed_bytes),
                fmt_bytes(saved),
                report.duration_ms,
            ))
            .into_response()
        }
        Ok(Err(e)) => Html(format!(
            r#"<div class="bg-red-900/30 border border-red-700 rounded-xl p-5">
                 <p class="text-red-400 font-semibold">Backup failed</p>
                 <p class="text-sm text-slate-400 mt-1">{e:#}</p>
               </div>"#
        ))
        .into_response(),
        Err(e) => Html(format!(
            r#"<p class="text-red-400 text-sm p-4">Task error: {e}</p>"#
        ))
        .into_response(),
    }
}

/// `POST /api/backups/delete` — delete one backup archive and its DB record.
///
/// Removes the .tar.zst archive, the companion .json manifest (if present),
/// and the row from the `backups` table.
/// Returns an empty body on success (HTMX removes the row) or an error fragment.
async fn delete_backup_handler(
    State(state): State<AppState>,
    Form(form): Form<BackupDeleteForm>,
) -> Response {
    let id = form.id;
    match run_db(state.db, move |conn| delete_backup(conn, id)).await {
        Ok(()) => Html("").into_response(),
        Err(msg) => Html(format!(
            r#"<p class="text-red-400 text-sm p-3">Delete failed: {msg}</p>"#
        ))
        .into_response(),
    }
}

/// `POST /api/duplicates/keep-newest` — delete all but the newest copy in one group.
///
/// Receives a form field `hash=<64-char-blake3>` sent by HTMX.
/// On success, returns an empty body — HTMX swaps the group's `<details>` with
/// nothing (`hx-swap="outerHTML"`), which removes the card from the DOM.
/// On failure, returns a small error fragment that replaces the card instead.
async fn keep_newest_handler(
    State(state): State<AppState>,
    Form(form): Form<DeleteGroupForm>,
) -> Response {
    // Move the hash into the closure so it is owned (required by spawn_blocking).
    let hash = form.hash;
    match run_db(state.db, move |conn| delete_group_keep_newest(conn, &hash)).await {
        Ok((_deleted, errors)) if errors.is_empty() => {
            // Empty body → HTMX removes the group card from the DOM entirely.
            Html("").into_response()
        }
        Ok((deleted, errors)) => {
            // Partial success: some files were deleted but others failed.
            // Return an error fragment so the card stays visible with the problem.
            Html(format!(
                r#"<p class="text-amber-400 text-sm p-4">
                     Deleted {deleted} file(s), but some errors occurred:<br>
                     <span class="font-mono text-xs">{}</span>
                   </p>"#,
                errors.join("<br>")
            ))
            .into_response()
        }
        Err(msg) => Html(format!(
            r#"<p class="text-red-400 text-sm p-4">Error: {msg}</p>"#
        ))
        .into_response(),
    }
}

/// `POST /api/duplicates/keep-newest-all` — apply keep-newest to every group at once.
///
/// No request body needed — this acts on all current duplicate groups.
/// On success, sends the HTMX `HX-Refresh: true` header, which tells the
/// browser to do a full page reload so the summary bar updates correctly.
async fn keep_newest_all_handler(State(state): State<AppState>) -> Response {
    match run_db(state.db, delete_all_groups_keep_newest).await {
        Ok((_deleted, errors)) if errors.is_empty() => {
            // HX-Refresh causes HTMX to reload the full page after the response.
            // This is cleaner than trying to OOB-update multiple DOM regions.
            let mut headers = HeaderMap::new();
            headers.insert(
                HeaderName::from_static("hx-refresh"),
                HeaderValue::from_static("true"),
            );
            (headers, Html("")).into_response()
        }
        Ok((deleted, errors)) => {
            // Some deletions failed — still refresh, but warn in the fragment.
            // We can't show the warning AND refresh, so we skip the refresh
            // and show the error inside the groups container instead.
            Html(format!(
                r#"<div class="bg-amber-900/30 border border-amber-700 rounded-xl p-6">
                     <p class="text-amber-400 font-semibold mb-2">
                       Deleted {deleted} file(s), but some errors occurred:
                     </p>
                     <ul class="text-xs font-mono text-amber-300 space-y-1">
                       {}
                     </ul>
                     <p class="text-slate-400 text-sm mt-3">Reload the page to see the updated list.</p>
                   </div>"#,
                errors
                    .iter()
                    .map(|e| format!("<li>{e}</li>"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
            .into_response()
        }
        Err(msg) => Html(format!(
            r#"<p class="text-red-400 text-sm p-4">Error: {msg}</p>"#
        ))
        .into_response(),
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

/// Deserialized from the `POST /api/duplicates/keep-newest` form body.
///
/// HTMX sends form data as `application/x-www-form-urlencoded` by default,
/// which `axum::extract::Form` decodes into this struct automatically.
#[derive(Deserialize)]
struct DeleteGroupForm {
    /// The full 64-char BLAKE3 hash that uniquely identifies the duplicate group.
    hash: String,
}

/// Form body for `POST /api/backups/create`.
///
/// Only `label` is user-supplied from the web form. All other backup options
/// use sensible defaults (compression level 3, all categories, no dry-run).
#[derive(Deserialize)]
struct BackupCreateForm {
    /// Human-readable archive label. Defaults to "backup-YYYY-MM-DD" if blank.
    label: Option<String>,
}

/// Form body for `POST /api/backups/delete`.
#[derive(Deserialize)]
struct BackupDeleteForm {
    /// The SQLite row ID of the backup record to remove.
    id: i64,
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
        // Fetch paths with modified_at, sorted newest-first.
        // Index 0 will be the file we keep on "Keep Newest" — we annotate it.
        let mut path_stmt = conn.prepare(
            "SELECT path, modified_at FROM files
             WHERE content_hash = ?1 AND is_present = 1
             ORDER BY modified_at DESC, path ASC",
        )?;
        let raw: Vec<(String, String)> = path_stmt
            .query_map([&hash], |row| Ok((row.get(0)?, row.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();

        // Annotate each path: the first (newest) gets is_newest=true, rest false.
        let paths: Vec<PathInfo> = raw
            .into_iter()
            .enumerate()
            .map(|(i, (path, modified_at))| PathInfo {
                // Truncate the ISO 8601 timestamp to just the date for display.
                modified_at: modified_at.chars().take(10).collect(),
                is_newest:   i == 0,
                path,
            })
            .collect();

        let wasted = size_bytes * (copies as u64 - 1);
        total_wasted_bytes += wasted;

        // Borrow hash before moving it into the struct.
        let hash_preview = hash[..12.min(hash.len())].to_string();
        groups.push(DupGroupRow {
            hash,
            hash_preview,
            delete_count: copies.saturating_sub(1),
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
    // Select all columns (including id and algorithm) so we have the row id
    // for the delete button. Column order: id(0), label(1), archive_path(2),
    // original_bytes(3), compressed_bytes(4), algorithm(5), created_at(6).
    let mut stmt = conn.prepare(
        "SELECT id, label, archive_path, original_bytes, compressed_bytes, algorithm, created_at
         FROM backups ORDER BY created_at DESC",
    )?;

    let mut total_original:   u64 = 0;
    let mut total_compressed: u64 = 0;

    // Build the Vec with a for loop so the accumulator updates are explicit
    // rather than hidden inside a `.map()` closure (which should be pure).
    let mut backups: Vec<BackupRow> = Vec::new();
    for row in stmt
        .query_map([], |row| {
            let id:           i64    = row.get(0)?;
            let label:        String = row.get(1)?;
            let archive_path: String = row.get(2)?;
            let orig:         i64    = row.get(3)?;
            let comp:         i64    = row.get(4)?;
            let created_at:   String = row.get(6)?;  // col 5 = algorithm
            Ok((id, label, archive_path, orig as u64, comp as u64, created_at))
        })?
        .filter_map(|r| r.ok())
    {
        let (id, label, archive_path, orig, comp, created_at) = row;
        total_original   += orig;
        total_compressed += comp;

        let ratio = if orig > 0 {
            format!("{:.1}%", 100.0 * (1.0 - comp as f64 / orig as f64))
        } else {
            "—".into()
        };

        backups.push(BackupRow {
            id,
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
// Duplicate deletion helpers (synchronous — called inside spawn_blocking)
// =============================================================================
// These are the only functions in this module that mutate the database or
// touch the filesystem. Everything else is read-only.

/// Keep the newest copy of a duplicate group and delete all others.
///
/// Steps:
///   1. Query all `(path, modified_at)` rows for `hash`, sorted newest-first.
///   2. The first row is the keeper; all others are deleted from disk.
///   3. Each successfully deleted file is marked `is_present = 0` in the DB.
///
/// File-system errors are collected into the returned `Vec<String>` rather than
/// aborting — one permission error shouldn't prevent the other deletions.
fn delete_group_keep_newest(
    conn: &Connection,
    hash: &str,
) -> rusqlite::Result<(usize, Vec<String>)> {
    // Fetch all paths for the group, newest first.
    let mut stmt = conn.prepare(
        "SELECT path FROM files
         WHERE content_hash = ?1 AND is_present = 1
         ORDER BY modified_at DESC, path ASC",
    )?;
    let paths: Vec<String> = stmt
        .query_map([hash], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    // Nothing to do if the group has fewer than 2 present files.
    if paths.len() < 2 {
        return Ok((0, vec![]));
    }

    // `paths[0]` is the newest — skip it; delete all the rest.
    let mut deleted = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for path in &paths[1..] {
        match std::fs::remove_file(path) {
            Ok(()) => {
                // Mark the row absent so the dashboard stops counting it.
                // Ignore the rusqlite error here — the file is already gone,
                // so propagating the DB error would be misleading.
                let _ = conn.execute(
                    "UPDATE files SET is_present = 0 WHERE path = ?1",
                    [path.as_str()],
                );
                deleted += 1;
            }
            Err(e) => {
                // Record the error but continue deleting the other files.
                errors.push(format!("{path}: {e}"));
            }
        }
    }

    Ok((deleted, errors))
}

/// Apply `delete_group_keep_newest` to every current duplicate group.
///
/// Fetches the list of duplicate hashes from the database, then calls the
/// per-group helper for each one. Errors across all groups are accumulated
/// so the caller gets a complete picture of what failed.
fn delete_all_groups_keep_newest(
    conn: &Connection,
) -> rusqlite::Result<(usize, Vec<String>)> {
    // Find all hashes that appear more than once among present files.
    let mut stmt = conn.prepare(
        "SELECT content_hash FROM files
         WHERE content_hash IS NOT NULL AND is_present = 1
         GROUP BY content_hash
         HAVING COUNT(*) > 1",
    )?;
    let hashes: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    let mut total_deleted = 0usize;
    let mut all_errors:    Vec<String> = Vec::new();

    for hash in hashes {
        // Re-use the single-group helper; propagate any rusqlite::Error up.
        let (n, errs) = delete_group_keep_newest(conn, &hash)?;
        total_deleted += n;
        all_errors.extend(errs);
    }

    Ok((total_deleted, all_errors))
}

/// Delete a backup record by its SQLite row id.
///
/// Removes, in order:
///   1. The .tar.zst archive file (if it still exists on disk).
///   2. The companion .json manifest (same base path, .json extension).
///   3. The row from the `backups` table.
///
/// File-not-found errors are silently ignored — the archive may have already
/// been deleted manually. Other I/O errors are also ignored (we still remove
/// the DB record so the dashboard stays clean).
fn delete_backup(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    // Fetch the archive path for this backup so we can delete the file.
    let archive_path: Option<String> = conn.query_row(
        "SELECT archive_path FROM backups WHERE id = ?1",
        [id],
        |row| row.get(0),
    )
    .optional()?;

    if let Some(path) = archive_path {
        // Delete the compressed archive (ignore any I/O error).
        let _ = std::fs::remove_file(&path);

        // Derive the manifest path: "backup-label-uuid.tar.zst" →
        // "backup-label-uuid.json" by replacing the double extension.
        let manifest = path.trim_end_matches(".tar.zst").to_string() + ".json";
        let _ = std::fs::remove_file(&manifest);
    }

    // Remove the DB record regardless of whether the file existed.
    conn.execute("DELETE FROM backups WHERE id = ?1", [id])?;
    Ok(())
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
/// Accepts the full user `Config` so the backup-create endpoint can write
/// archives to `config.backup_dir` without re-reading the config file.
///
/// This function is `async` and blocks until the server is shut down (Ctrl-C).
pub async fn run(host: &str, port: u16, config: &crate::config::Config) -> anyhow::Result<()> {
    use anyhow::Context;

    // Open the SQLite connection. WAL mode and FK enforcement are set by db::open.
    let conn = crate::db::open(&config.db_path)
        .with_context(|| format!("could not open database at {}", config.db_path.display()))?;

    let state = AppState {
        db:     Arc::new(Mutex::new(conn)),
        config: Arc::new(config.clone()),
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

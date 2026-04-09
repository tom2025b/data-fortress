//! # db.rs
//!
//! SQLite database layer for Data Fortress.
//!
//! This is the single file that owns all SQL. No other module writes raw SQL —
//! they call functions defined here instead. This keeps the database schema in
//! one place and makes it easy to audit, migrate, and test.
//!
//! ## Schema overview
//!
//! - `files`   — one row per file discovered during scanning (FileRecord)
//! - `backups` — one row per versioned backup operation (BackupRecord)
//!
//! ## Connection model
//!
//! We pass a `&Connection` into each function rather than using a global.
//! This makes functions easy to test (pass a temp-file connection) and keeps
//! the caller in control of transaction boundaries.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::models::{BackupRecord, DuplicateGroup, FileCategory, FileRecord, ScanStats};

// =============================================================================
// Open / Init
// =============================================================================

/// Open (or create) the SQLite database at `path` and return a connection.
///
/// This is the first function called at startup. It:
/// 1. Creates the file if it does not exist.
/// 2. Enables WAL mode for better concurrent read performance.
/// 3. Runs `init_schema` to create tables if they are missing.
pub fn open(path: &Path) -> Result<Connection> {
    // `Connection::open` creates the file if it doesn't exist, or opens it if
    // it does. rusqlite wraps the underlying libsqlite3 C library call.
    let conn = Connection::open(path)
        .with_context(|| format!("could not open database at {}", path.display()))?;

    // WAL (Write-Ahead Logging) mode allows concurrent readers while a write
    // is in progress. Without it, any write locks out all readers entirely.
    // This matters for the dashboard reading while a scan writes simultaneously.
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .context("could not enable WAL mode")?;

    // Foreign key enforcement is OFF by default in SQLite for compatibility.
    // Turn it on so referential integrity is actually checked.
    conn.execute_batch("PRAGMA foreign_keys=ON;")
        .context("could not enable foreign keys")?;

    // Create tables if this is the first run.
    init_schema(&conn)?;

    Ok(conn)
}

/// Create the database schema (tables + indexes) if they do not already exist.
///
/// Uses `CREATE TABLE IF NOT EXISTS` so this is safe to call on every startup —
/// it is a no-op when the schema is already in place.
fn init_schema(conn: &Connection) -> Result<()> {
    // `execute_batch` runs multiple semicolon-separated SQL statements at once.
    // We use a raw string literal (`r#"..."#`) to avoid escaping quotes inside.
    conn.execute_batch(r#"
        -- ── files table ────────────────────────────────────────────────────
        CREATE TABLE IF NOT EXISTS files (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            path            TEXT    NOT NULL UNIQUE,  -- absolute path, must be unique
            name            TEXT    NOT NULL,
            extension       TEXT    NOT NULL DEFAULT '',
            category        TEXT    NOT NULL DEFAULT 'other',
            mime_type       TEXT    NOT NULL DEFAULT '',
            size_bytes      INTEGER NOT NULL DEFAULT 0,
            content_hash    TEXT,                     -- NULL until hashed
            modified_at     TEXT    NOT NULL,         -- ISO 8601 UTC string
            scanned_at      TEXT    NOT NULL,
            is_present      INTEGER NOT NULL DEFAULT 1 -- 1 = present, 0 = deleted
        );

        -- Index on content_hash accelerates duplicate detection queries
        -- (GROUP BY content_hash WHERE content_hash IS NOT NULL).
        CREATE INDEX IF NOT EXISTS idx_files_content_hash
            ON files (content_hash)
            WHERE content_hash IS NOT NULL;

        -- Index on is_present speeds up "show only present files" filters.
        CREATE INDEX IF NOT EXISTS idx_files_is_present
            ON files (is_present);

        -- Index on category accelerates dashboard category-breakdown queries.
        CREATE INDEX IF NOT EXISTS idx_files_category
            ON files (category);

        -- ── backups table ────────────────────────────────────────────────────
        CREATE TABLE IF NOT EXISTS backups (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            label             TEXT    NOT NULL,
            archive_path      TEXT    NOT NULL UNIQUE,
            original_bytes    INTEGER NOT NULL DEFAULT 0,
            compressed_bytes  INTEGER NOT NULL DEFAULT 0,
            algorithm         TEXT    NOT NULL DEFAULT 'zstd',
            created_at        TEXT    NOT NULL
        );
    "#).context("could not initialize database schema")?;

    Ok(())
}

// =============================================================================
// FileRecord — insert / upsert
// =============================================================================

/// Insert a new `FileRecord` into the database, or update it if the path
/// already exists (upsert by path).
///
/// Returns the SQLite row ID of the inserted or updated row.
///
/// We use `INSERT OR REPLACE` (SQLite's upsert shorthand) so re-scanning the
/// same path updates the existing record rather than failing with a UNIQUE
/// constraint violation.
pub fn upsert_file(conn: &Connection, record: &FileRecord) -> Result<i64> {
    conn.execute(
        r#"
        INSERT INTO files
            (path, name, extension, category, mime_type, size_bytes,
             content_hash, modified_at, scanned_at, is_present)
        VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(path) DO UPDATE SET
            name         = excluded.name,
            extension    = excluded.extension,
            category     = excluded.category,
            mime_type    = excluded.mime_type,
            size_bytes   = excluded.size_bytes,
            content_hash = excluded.content_hash,
            modified_at  = excluded.modified_at,
            scanned_at   = excluded.scanned_at,
            is_present   = excluded.is_present
        "#,
        params![
            record.path,
            record.name,
            record.extension,
            record.category.to_string(), // Display impl → "image", "video", etc.
            record.mime_type,
            record.size_bytes as i64,    // SQLite INTEGER is signed; cast from u64
            record.content_hash,
            record.modified_at.to_rfc3339(),  // Store as ISO 8601 string
            record.scanned_at.to_rfc3339(),
            record.is_present as i32,    // SQLite has no BOOLEAN; 1/0
        ],
    )
    .context("could not upsert file record")?;

    // `last_insert_rowid()` returns the rowid of the most recent INSERT.
    // For ON CONFLICT UPDATE, it returns the rowid of the updated row.
    Ok(conn.last_insert_rowid())
}

/// Update only the `content_hash` field for a file identified by its path.
///
/// Called by the hasher after it computes the BLAKE3 hash of a file.
/// Updating one column is much cheaper than a full upsert.
pub fn set_content_hash(conn: &Connection, path: &str, hash: &str) -> Result<()> {
    conn.execute(
        "UPDATE files SET content_hash = ?1 WHERE path = ?2",
        params![hash, path],
    )
    .context("could not update content hash")?;
    Ok(())
}

/// Mark all files under `root_dir` as not present (`is_present = 0`).
///
/// Called at the start of a re-scan. After the scan completes, any file that
/// was not visited will still have `is_present = 0` — meaning it was deleted.
pub fn mark_all_absent(conn: &Connection, root_dir: &str) -> Result<()> {
    // `LIKE ?1 || '%'` matches any path that starts with root_dir.
    // This is safe from SQL injection because we use a parameter (?1), not
    // string concatenation inside the SQL string itself.
    conn.execute(
        "UPDATE files SET is_present = 0 WHERE path LIKE ?1 || '%'",
        params![root_dir],
    )
    .context("could not mark files as absent")?;
    Ok(())
}

/// Mark a specific file path as present (`is_present = 1`).
///
/// Called during re-scan for every file successfully visited.
pub fn mark_present(conn: &Connection, path: &str) -> Result<()> {
    conn.execute(
        "UPDATE files SET is_present = 1 WHERE path = ?1",
        params![path],
    )
    .context("could not mark file as present")?;
    Ok(())
}

// =============================================================================
// FileRecord — queries
// =============================================================================

/// Fetch a single `FileRecord` by its absolute path.
///
/// Returns `None` if the path is not in the database.
pub fn get_file_by_path(conn: &Connection, path: &str) -> Result<Option<FileRecord>> {
    // `query_row` runs the query and maps the first result row.
    // `Optional()` converts "no rows found" from an error into `Ok(None)`.
    let result = conn.query_row(
        "SELECT * FROM files WHERE path = ?1",
        params![path],
        row_to_file_record,  // see the helper function below
    )
    .optional()  // rusqlite::OptionalExtension trait method
    .context("could not query file by path")?;

    Ok(result)
}

/// Return all `FileRecord`s where `is_present = 1`, ordered by path.
///
/// Used by the dashboard overview page to list all known files.
pub fn get_all_present_files(conn: &Connection) -> Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM files WHERE is_present = 1 ORDER BY path",
    )
    .context("could not prepare file query")?;

    // `query_map` runs the query and applies a mapping function to each row.
    // We collect the results into a Vec, propagating any row errors.
    let records = stmt
        .query_map([], row_to_file_record)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("could not fetch file records")?;

    Ok(records)
}

/// Return all files that have no `content_hash` yet and are within the size
/// limit, ordered by size ascending (smallest files hashed first).
///
/// Used by the hasher to find files that still need hashing.
pub fn get_unhashed_files(conn: &Connection, max_size: u64) -> Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT * FROM files
        WHERE content_hash IS NULL
          AND is_present = 1
          AND size_bytes <= ?1
        ORDER BY size_bytes ASC
        "#,
    )
    .context("could not prepare unhashed files query")?;

    let records = stmt
        .query_map(params![max_size as i64], row_to_file_record)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("could not fetch unhashed files")?;

    Ok(records)
}

// =============================================================================
// Duplicate detection
// =============================================================================

/// Find all groups of files that share the same `content_hash`.
///
/// Returns only groups with 2 or more files — a hash that appears once is
/// not a duplicate. Results are ordered by wasted bytes descending so the
/// biggest wins appear first in the dashboard.
pub fn get_duplicate_groups(conn: &Connection) -> Result<Vec<DuplicateGroup>> {
    // Step 1: find hashes that appear more than once.
    // We query for the hash and the shared file size in one pass.
    let mut hash_stmt = conn.prepare(
        r#"
        SELECT content_hash, size_bytes, COUNT(*) as count
        FROM files
        WHERE content_hash IS NOT NULL
          AND is_present = 1
        GROUP BY content_hash
        HAVING COUNT(*) > 1
        ORDER BY (size_bytes * (COUNT(*) - 1)) DESC
        "#,
    )
    .context("could not prepare duplicate hash query")?;

    // Collect (hash, size_bytes, count) tuples first.
    let hash_rows: Vec<(String, u64, u64)> = hash_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,  // content_hash
                row.get::<_, i64>(1)? as u64, // size_bytes
                row.get::<_, i64>(2)? as u64, // count
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("could not fetch duplicate hashes")?;

    // Step 2: for each duplicate hash, fetch all the matching FileRecords.
    let mut groups = Vec::with_capacity(hash_rows.len());

    for (hash, size_bytes, count) in hash_rows {
        let mut file_stmt = conn.prepare(
            "SELECT * FROM files WHERE content_hash = ?1 AND is_present = 1 ORDER BY path",
        )
        .context("could not prepare duplicate files query")?;

        let files: Vec<FileRecord> = file_stmt
            .query_map(params![hash], row_to_file_record)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("could not fetch duplicate file records")?;

        groups.push(DuplicateGroup {
            content_hash: hash,
            // Wasted bytes = size * (copies - 1). One copy is the "keeper".
            wasted_bytes: size_bytes * (count - 1),
            files,
        });
    }

    Ok(groups)
}

// =============================================================================
// Search
// =============================================================================

/// Search for files whose `name` or `path` contains `query` (case-insensitive).
///
/// This is the fast metadata-only search. Full content search (PDF text, etc.)
/// is handled in `search/mod.rs` which calls this function and then re-ranks.
pub fn search_files_by_name(conn: &Connection, query: &str) -> Result<Vec<FileRecord>> {
    // SQLite's LIKE is case-insensitive for ASCII characters by default.
    // The `%` wildcards match any sequence of characters on either side.
    let pattern = format!("%{}%", query);

    let mut stmt = conn.prepare(
        r#"
        SELECT * FROM files
        WHERE is_present = 1
          AND (name LIKE ?1 OR path LIKE ?1)
        ORDER BY name ASC
        LIMIT 500
        "#,
    )
    .context("could not prepare name search query")?;

    let records = stmt
        .query_map(params![pattern], row_to_file_record)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("could not fetch search results")?;

    Ok(records)
}

/// Search for files within a specific category.
pub fn search_files_by_category(
    conn: &Connection,
    category: &FileCategory,
) -> Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM files WHERE is_present = 1 AND category = ?1 ORDER BY name ASC",
    )
    .context("could not prepare category search query")?;

    let records = stmt
        .query_map(params![category.to_string()], row_to_file_record)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("could not fetch files by category")?;

    Ok(records)
}

// =============================================================================
// Statistics
// =============================================================================

/// Compute aggregate statistics for the dashboard overview page.
///
/// Returns a `ScanStats` populated with totals derived from the database.
/// `duration_ms` is left at 0 here — the scanner sets it after a live scan.
pub fn compute_stats(conn: &Connection) -> Result<ScanStats> {
    // A single SQL query computes all counters in one pass over the table.
    let (files_found, total_bytes): (u64, u64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0) FROM files WHERE is_present = 1",
        [],
        |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
    )
    .context("could not compute file statistics")?;

    Ok(ScanStats {
        files_found,
        total_bytes,
        // files_new and files_skipped are populated during a live scan, not
        // computed from the database retrospectively.
        files_new: 0,
        files_skipped: 0,
        duration_ms: 0,
    })
}

// =============================================================================
// BackupRecord — insert / query
// =============================================================================

/// Insert a completed `BackupRecord` into the database.
///
/// Returns the assigned row ID.
pub fn insert_backup(conn: &Connection, record: &BackupRecord) -> Result<i64> {
    conn.execute(
        r#"
        INSERT INTO backups
            (label, archive_path, original_bytes, compressed_bytes, algorithm, created_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        "#,
        params![
            record.label,
            record.archive_path,
            record.original_bytes as i64,
            record.compressed_bytes as i64,
            record.algorithm,
            record.created_at.to_rfc3339(),
        ],
    )
    .context("could not insert backup record")?;

    Ok(conn.last_insert_rowid())
}

/// Return all backup records, most recent first.
pub fn get_all_backups(conn: &Connection) -> Result<Vec<BackupRecord>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM backups ORDER BY created_at DESC",
    )
    .context("could not prepare backup query")?;

    let records = stmt
        .query_map([], row_to_backup_record)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("could not fetch backup records")?;

    Ok(records)
}

// =============================================================================
// Row mapping helpers
// =============================================================================

/// Maps a SQLite row from the `files` table into a `FileRecord`.
///
/// This is a free function (not a closure) so it can be passed by name to
/// `query_map` and `query_row` without repeating the mapping logic.
///
/// Column order matches the `files` table schema defined in `init_schema`.
fn row_to_file_record(row: &Row) -> rusqlite::Result<FileRecord> {
    // Parse the category string back into a FileCategory enum.
    // We use unwrap_or(Other) because FromStr for FileCategory never actually
    // errors (unknown strings → Other), so the parse will always succeed.
    let category_str: String = row.get(4)?;
    let category = FileCategory::from_str(&category_str)
        .unwrap_or(FileCategory::Other);

    // Parse ISO 8601 timestamp strings back into DateTime<Utc>.
    // `parse::<DateTime<Utc>>()` understands RFC 3339 format.
    let modified_at_str: String = row.get(8)?;
    let modified_at = modified_at_str
        .parse::<DateTime<Utc>>()
        .unwrap_or_else(|_| Utc::now()); // Fall back to now if corrupt

    let scanned_at_str: String = row.get(9)?;
    let scanned_at = scanned_at_str
        .parse::<DateTime<Utc>>()
        .unwrap_or_else(|_| Utc::now());

    Ok(FileRecord {
        id:           Some(row.get(0)?),               // column 0: id
        path:         row.get(1)?,                     // column 1: path
        name:         row.get(2)?,                     // column 2: name
        extension:    row.get(3)?,                     // column 3: extension
        category,                                      // column 4: parsed above
        mime_type:    row.get(5)?,                     // column 5: mime_type
        size_bytes:   row.get::<_, i64>(6)? as u64,   // column 6: size_bytes
        content_hash: row.get(7)?,                     // column 7: content_hash (Option)
        modified_at,                                   // column 8: parsed above
        scanned_at,                                    // column 9: parsed above
        is_present:   row.get::<_, i32>(10)? != 0,    // column 10: 1/0 → bool
    })
}

/// Maps a SQLite row from the `backups` table into a `BackupRecord`.
fn row_to_backup_record(row: &Row) -> rusqlite::Result<BackupRecord> {
    let created_at_str: String = row.get(6)?;
    let created_at = created_at_str
        .parse::<DateTime<Utc>>()
        .unwrap_or_else(|_| Utc::now());

    Ok(BackupRecord {
        id:               Some(row.get(0)?),
        label:            row.get(1)?,
        archive_path:     row.get(2)?,
        original_bytes:   row.get::<_, i64>(3)? as u64,
        compressed_bytes: row.get::<_, i64>(4)? as u64,
        algorithm:        row.get(5)?,
        created_at,
    })
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY keep all SQL in db.rs?
//    If SQL is scattered across scanner/, dedup/, and search/, the schema
//    becomes implicit — you have to grep the whole codebase to understand
//    what columns exist. One file = one place to audit, migrate, and test.
//
// 2. WHY `&Connection` as a parameter instead of a global?
//    A function that takes `&Connection` is easy to test: pass in a connection
//    to an in-memory database (`Connection::open_in_memory()`). A global
//    connection can't be easily swapped for tests. Dependency injection via
//    function parameters is the idiomatic Rust approach.
//
// 3. WHY `ON CONFLICT(path) DO UPDATE`?
//    This is SQLite's UPSERT syntax: "insert if new, update if the UNIQUE
//    constraint fires". Without it, re-scanning the same file would fail with
//    a constraint violation. The `excluded.` prefix refers to the row that
//    would have been inserted — the incoming values.
//
// 4. WHY store DateTime as an ISO 8601 TEXT string?
//    SQLite has no native datetime type. Storing as TEXT in RFC 3339 format
//    ("2025-04-08T14:30:00Z") keeps values human-readable in a DB browser,
//    sortable as strings (lexicographic order = chronological order for ISO
//    8601), and trivially parseable by Python's datetime.fromisoformat().
//
// 5. WHY `i64` casts for u64 values?
//    SQLite's INTEGER type is a signed 64-bit integer. Rust's u64 can hold
//    values larger than i64::MAX (~9.2 EB), but file sizes on real drives
//    are nowhere near that limit. We cast to i64 for storage and back to u64
//    when reading, which is safe for any realistic file size.
//
// 6. WHY `query_map` + `collect`?
//    `query_map` lazily applies a mapping function to each row as the cursor
//    advances. `collect::<rusqlite::Result<Vec<_>>>()` gathers all results
//    and short-circuits on the first error, propagating it up with `?`.

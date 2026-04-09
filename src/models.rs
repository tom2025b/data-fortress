//! # models.rs
//!
//! Central data model definitions for Data Fortress.
//!
//! Every module (scanner, dedup, organizer, search, backup) imports from here.
//! Keeping all shared types in one file prevents circular dependencies and gives
//! the Streamlit dashboard a single, stable JSON contract to deserialize against.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// =============================================================================
// FileCategory
// =============================================================================

/// Broad classification of a file's purpose, derived during scanning.
///
/// Stored as a lowercase TEXT string in SQLite (e.g. "image", "document").
/// Serde serializes it the same way for JSON output to the dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")] // JSON: "image" not "Image"
pub enum FileCategory {
    Image,    // JPEG, PNG, GIF, HEIF, WebP, etc.
    Video,    // MP4, MKV, AVI, MOV, etc.
    Audio,    // MP3, FLAC, WAV, OGG, etc.
    Document, // PDF, DOCX, TXT, ODT, etc.
    Archive,  // ZIP, TAR, GZ, 7Z, etc.
    Code,     // RS, PY, JS, TS, C, CPP, etc.
    Other,    // anything that does not fit the above
}

impl std::fmt::Display for FileCategory {
    /// Converts a FileCategory to the lowercase string stored in SQLite.
    ///
    /// We implement Display so we can write `category.to_string()` when
    /// building SQL INSERT statements in db.rs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            FileCategory::Image    => "image",
            FileCategory::Video    => "video",
            FileCategory::Audio    => "audio",
            FileCategory::Document => "document",
            FileCategory::Archive  => "archive",
            FileCategory::Code     => "code",
            FileCategory::Other    => "other",
        };
        write!(f, "{}", s)
    }
}

impl std::str::FromStr for FileCategory {
    type Err = anyhow::Error;

    /// Parses the TEXT value from a SQLite row back into a FileCategory.
    ///
    /// Unknown strings fall back to `Other` rather than erroring, so new
    /// category values added in future versions don't crash old readers.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "image"    => FileCategory::Image,
            "video"    => FileCategory::Video,
            "audio"    => FileCategory::Audio,
            "document" => FileCategory::Document,
            "archive"  => FileCategory::Archive,
            "code"     => FileCategory::Code,
            _          => FileCategory::Other,
        })
    }
}

// =============================================================================
// FileRecord
// =============================================================================

/// Represents a single file discovered during a scan.
///
/// This is the core unit of the entire system. Every feature — deduplication,
/// search, organization, backup — operates on FileRecords stored in SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    /// Auto-incremented primary key assigned by SQLite.
    ///
    /// `Option<i64>` because the ID doesn't exist yet before the first INSERT.
    /// After insertion, db.rs fills this in with the assigned row ID.
    pub id: Option<i64>,

    /// Absolute path to the file on disk (e.g. "/mnt/drive2/Photos/IMG_001.jpg").
    /// Stored as a String (not PathBuf) so serde can serialize it to JSON easily.
    pub path: String,

    /// File name without any directory component (e.g. "IMG_001.jpg").
    /// Derived from `path` during scanning; stored separately for fast queries.
    pub name: String,

    /// File extension in lowercase with no leading dot (e.g. "jpg", "pdf", "").
    /// Empty string when the file has no extension.
    pub extension: String,

    /// Broad category assigned by the classifier (image, video, document, etc.)
    pub category: FileCategory,

    /// MIME type string detected from magic bytes (e.g. "image/jpeg").
    /// More reliable than the extension alone.
    pub mime_type: String,

    /// File size in bytes, as reported by the filesystem metadata.
    /// u64 because files can be up to ~18 exabytes; i64 would overflow at 8 EB.
    pub size_bytes: u64,

    /// BLAKE3 hex-encoded hash of the full file content.
    ///
    /// `None` if the file hasn't been hashed yet — large files are hashed in a
    /// second pass so the initial scan stays fast. Identical hashes = duplicate.
    pub content_hash: Option<String>,

    /// Last-modified timestamp from the filesystem (mtime).
    /// Stored as UTC; displayed in local time by the dashboard.
    pub modified_at: DateTime<Utc>,

    /// Timestamp of when this record was first inserted into the database.
    /// Used to track scan history and detect newly added files.
    pub scanned_at: DateTime<Utc>,

    /// Whether this file still exists on disk.
    ///
    /// Set to `false` when a re-scan finds that a previously recorded file
    /// has been deleted or moved. We keep the record for history rather than
    /// deleting it from the database.
    pub is_present: bool,
}

// =============================================================================
// DuplicateGroup
// =============================================================================

/// A set of files that share the same BLAKE3 content hash — confirmed duplicates.
///
/// The first file in `files` (ordered by path for determinism) is treated as
/// the "canonical" copy to keep. The rest are candidates for deletion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateGroup {
    /// The BLAKE3 hash shared by all files in this group.
    pub content_hash: String,

    /// Bytes that could be reclaimed by deleting all but one copy.
    /// Calculated as: `size_bytes * (files.len() as u64 - 1)`.
    pub wasted_bytes: u64,

    /// All files sharing this content hash, sorted by path for stable ordering.
    pub files: Vec<FileRecord>,
}

// =============================================================================
// SearchResult
// =============================================================================

/// A ranked result returned by the search engine.
///
/// Results are ordered by `score` descending — highest relevance first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// The file that matched the query.
    pub file: FileRecord,

    /// Relevance score. Higher is a better match. Not normalized to any fixed
    /// range — use it only for relative ordering within a single result set.
    pub score: f64,

    /// A short excerpt of the matched text content from the file, with the
    /// query terms highlighted (using **asterisks** for plain text output).
    ///
    /// `None` for binary files (images, videos) that matched on metadata only,
    /// since there is no text content to excerpt.
    pub snippet: Option<String>,
}

// =============================================================================
// ScanStats
// =============================================================================

/// Summary statistics emitted as JSON to stdout at the end of a scan run.
///
/// The Streamlit dashboard parses this with `json.loads()` to update the
/// overview page without having to re-query the database itself.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScanStats {
    /// Total number of files discovered in this scan pass.
    pub files_found: u64,

    /// Files that were not present in the database before this scan (new files).
    pub files_new: u64,

    /// Files skipped due to permission errors, broken symlinks, or size limits.
    pub files_skipped: u64,

    /// Sum of `size_bytes` across all discovered files.
    pub total_bytes: u64,

    /// Wall-clock duration of the scan in milliseconds.
    pub duration_ms: u64,
}

// =============================================================================
// BackupRecord
// =============================================================================

/// Metadata for a single versioned backup operation, stored in SQLite.
///
/// The actual archive lives at `archive_path` on disk. This record lets us
/// display backup history in the dashboard and verify archive integrity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRecord {
    /// Auto-incremented primary key (None before first INSERT).
    pub id: Option<i64>,

    /// Human-readable label for this backup (e.g. "weekly-2025-04-08").
    /// Set by the user via the CLI `--label` flag, or auto-generated from date.
    pub label: String,

    /// Absolute path to the compressed archive file on disk.
    pub archive_path: String,

    /// Total uncompressed size of all files included in this backup.
    pub original_bytes: u64,

    /// Actual size of the compressed archive on disk after zstd compression.
    pub compressed_bytes: u64,

    /// Compression algorithm used — always "zstd" for now.
    /// Stored as a string so future algorithms can be added without a migration.
    pub algorithm: String,

    /// When this backup was created.
    pub created_at: DateTime<Utc>,
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY one central models.rs?
//    If FileRecord were defined inside scanner/ and also needed by dedup/ and
//    search/, Rust would require complex re-exports or you'd have to duplicate
//    the definition. One shared file = one source of truth, zero duplication.
//
// 2. WHY `Option<i64>` for database IDs?
//    Before a record is inserted into SQLite, it has no ID yet. Option<i64>
//    lets the same struct represent both "about to insert" (id = None) and
//    "already stored" (id = Some(42)) states, without needing two structs.
//
// 3. WHY `#[derive(Serialize, Deserialize)]`?
//    serde's derive macros generate all the JSON conversion code automatically.
//    The Rust binary can call `serde_json::to_string(&record)?` and the Python
//    dashboard calls `json.loads(output)` — no manual parsing on either side.
//
// 4. WHY BLAKE3 for content hashes?
//    BLAKE3 is faster than SHA-256 and not broken like MD5/SHA-1. For
//    deduplication we need collision resistance (two different files must never
//    produce the same hash), but we don't need the full overhead of SHA-512.
//    BLAKE3 is the modern performance-and-safety sweet spot.
//
// 5. WHY `DateTime<Utc>` instead of a Unix timestamp (i64)?
//    chrono's DateTime<Utc> serializes to a human-readable ISO 8601 string
//    ("2025-04-08T14:30:00Z") in JSON, which the dashboard can display directly.
//    A raw i64 would require an extra conversion step on the Python side.

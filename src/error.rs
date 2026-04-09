//! # error.rs
//!
//! Project-wide error type for Data Fortress.
//!
//! We use a two-layer error strategy:
//!   - Inside functions: `anyhow::Result<T>` for convenience and rich context.
//!   - Public module APIs: `FortressResult<T>` so callers can match on specific
//!     failure kinds (e.g. "was it a permission error or a hash failure?").
//!
//! `thiserror` generates the boilerplate `std::error::Error` impl for us.

// thiserror::Error is the derive macro that turns our enum into a proper
// std::error::Error type without writing hundreds of lines of boilerplate.
use thiserror::Error;

// =============================================================================
// FortressError
// =============================================================================

/// All error conditions that Data Fortress can produce.
///
/// Each variant represents a distinct failure domain. The `#[error("...")]`
/// attribute on each variant defines the human-readable message shown to the
/// user when the error is printed.
#[derive(Debug, Error)]
pub enum FortressError {
    // ── I/O ──────────────────────────────────────────────────────────────────

    /// Wraps std::io::Error for file-system failures: not found, permission
    /// denied, disk full, broken pipe, etc.
    ///
    /// `#[from]` generates a `From<std::io::Error> for FortressError` impl
    /// automatically, so any `io::Error` can be converted with `?`.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    // ── Database ──────────────────────────────────────────────────────────────

    /// Wraps rusqlite::Error for all SQLite failures: constraint violations,
    /// locked database, malformed SQL, type conversion errors, etc.
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    // ── Serialization ─────────────────────────────────────────────────────────

    /// Wraps serde_json::Error for JSON encode/decode failures.
    /// Raised when producing dashboard IPC output or reading config files.
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    // ── Scanner ───────────────────────────────────────────────────────────────

    /// The scanner encountered an error while processing a specific path.
    ///
    /// We store named fields (`path` and `reason`) so callers can log which
    /// file caused the problem without parsing a string.
    #[error("Scanner error at '{path}': {reason}")]
    ScanError { path: String, reason: String },

    // ── Hasher / Deduplication ────────────────────────────────────────────────

    /// Hashing a file failed (e.g. the file was deleted mid-scan, or I/O error
    /// reading its bytes). Distinct from ScanError so the dedup module can
    /// handle hash failures separately from scan failures.
    #[error("Failed to hash file '{path}': {reason}")]
    HashError { path: String, reason: String },

    // ── Organizer ─────────────────────────────────────────────────────────────

    /// The organizer tried to move a file that no longer exists on disk.
    /// Contains the source path that was not found.
    #[error("File not found for organization: {0}")]
    FileNotFound(String),

    /// The organizer's target destination path already exists and the user did
    /// not pass `--overwrite`. Contains the destination path.
    #[error("Destination already exists: {0}")]
    DestinationExists(String),

    // ── Backup ────────────────────────────────────────────────────────────────

    /// A backup operation failed. Contains a description of what went wrong
    /// (e.g. "could not create archive at /mnt/drive2/backups/...").
    #[error("Backup failed: {0}")]
    BackupError(String),

    // ── Search ────────────────────────────────────────────────────────────────

    /// The search query was malformed or the search index is inconsistent.
    #[error("Search error: {0}")]
    SearchError(String),

    // ── Configuration ─────────────────────────────────────────────────────────

    /// The config file could not be read, parsed, or written.
    /// Contains a human-readable description of what failed.
    #[error("Configuration error: {0}")]
    ConfigError(String),

    // ── Catch-all ─────────────────────────────────────────────────────────────

    /// Any error that doesn't fit a specific variant above.
    ///
    /// This exists as a bridge so functions using `anyhow::Result` internally
    /// can be converted to `FortressResult` at module boundaries via `?`.
    /// See the `From<anyhow::Error>` impl below.
    #[error("Unexpected error: {0}")]
    Unexpected(String),
}

// =============================================================================
// From<anyhow::Error>
// =============================================================================

impl From<anyhow::Error> for FortressError {
    /// Converts an `anyhow::Error` into `FortressError::Unexpected`.
    ///
    /// This allows internal helper functions that use `anyhow::Result` to be
    /// called with `?` from functions that return `FortressResult`. Without
    /// this impl, you'd need an explicit `.map_err(...)` at every call site.
    fn from(e: anyhow::Error) -> Self {
        // Preserve the full anyhow context chain in the error message so
        // nothing useful is lost during the conversion.
        FortressError::Unexpected(format!("{:#}", e))
    }
}

// =============================================================================
// FortressResult type alias
// =============================================================================

/// Shorthand for `Result<T, FortressError>`.
///
/// Public module functions return `FortressResult<T>` instead of the more
/// verbose `Result<T, FortressError>`. Usage:
///
/// ```rust
/// pub fn scan(path: &Path) -> FortressResult<ScanStats> { ... }
/// ```
pub type FortressResult<T> = Result<T, FortressError>;

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY thiserror instead of writing Error impls by hand?
//    Implementing std::error::Error requires writing Display, Error, and
//    sometimes Source by hand — that's 20+ lines per error type. `thiserror`
//    generates all of it from the `#[error("...")]` attributes, keeping the
//    code at the level of intent (what went wrong) not mechanics (how Rust
//    error traits work).
//
// 2. WHY `#[from]`?
//    `#[from]` on a variant field tells thiserror to generate:
//      `impl From<io::Error> for FortressError { ... }`
//    This makes the `?` operator work automatically. When a function returns
//    `FortressResult<T>` and you call `some_io_operation()?`, Rust sees the
//    io::Error and calls From::from() to turn it into FortressError::Io(...).
//
// 3. WHY keep anyhow AND thiserror?
//    - anyhow is for application internals: easy context chaining with
//      `.context("while opening config")`, no need to define variants.
//    - thiserror is for public APIs: callers need to pattern-match on the
//      error to decide what to do (retry? skip? abort?).
//    A common Rust pattern: use anyhow inside functions, convert to a typed
//    error at the public boundary. That's exactly what FortressResult does.
//
// 4. WHY named fields in ScanError / HashError?
//    `ScanError { path, reason }` instead of `ScanError(String)` means you
//    can write `match err { FortressError::ScanError { path, .. } => ... }`
//    and get the path directly, without parsing a string. Structured data
//    is always easier to handle programmatically than a message string.
//
// 5. WHY `FortressResult<T>` type alias?
//    Type aliases reduce repetition. Without it, every public function
//    signature would read `Result<ScanStats, FortressError>`. With it, they
//    read `FortressResult<ScanStats>` — shorter and clearly project-specific.

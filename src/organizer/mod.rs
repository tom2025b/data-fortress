//! # organizer/mod.rs
//!
//! Intelligent file organizer for Data Fortress.
//!
//! Takes files recorded in the database and moves them into a clean, structured
//! directory layout under a user-specified destination root. Three modes are
//! supported (see `OrganizeMode` in cli.rs):
//!
//! - **ByTypeAndDate** — `<dest>/<Category>/<Year>/<Month>/<file>`
//! - **ByDate**        — `<dest>/<Year>/<Month>/<file>`
//! - **ByType**        — `<dest>/<Category>/<file>`
//!
//! ## Safety
//!
//! - **Dry-run mode** — prints every planned move without touching the filesystem.
//! - **Undo log** — every actual move is recorded in `<dest>/.fortress_undo.json`
//!   so the operation can be reversed if needed.
//! - **Conflict handling** — if the destination already exists and `--overwrite`
//!   was not passed, the file is skipped and reported as a conflict.
//!
//! ## Design note
//!
//! The organizer reads `FileRecord`s from the database rather than walking the
//! filesystem directly. This means only files that have been scanned can be
//! organized — which is intentional. Scan first, organize second.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{Datelike, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::cli::{OrganizeMode, OrganizeArgs};
use crate::db;
use crate::models::{FileCategory, FileRecord};

// =============================================================================
// Public report type
// =============================================================================

/// Result of an organize run, returned to `main.rs`.
#[derive(Debug, Default)]
pub struct OrganizeReport {
    /// Number of files successfully moved (or that would be moved in dry-run).
    pub files_moved: usize,

    /// Files skipped because the destination already existed.
    pub conflicts: Vec<Conflict>,

    /// Files that could not be moved due to I/O errors.
    pub errors: Vec<String>,

    /// Path to the undo log written on disk (None in dry-run mode).
    pub undo_log_path: Option<PathBuf>,
}

/// A move that was skipped because the destination already exists.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub source: String,
    pub destination: String,
}

// =============================================================================
// Undo log
// =============================================================================

/// A single entry in the undo log — records one completed file move.
///
/// Serialized to JSON so the user (or a future `undo` command) can reverse
/// each move with a simple `fs::rename(destination, source)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoEntry {
    /// The original absolute path before the move.
    pub source: String,
    /// The new absolute path after the move.
    pub destination: String,
    /// When the move was performed (UTC).
    pub moved_at: chrono::DateTime<Utc>,
}

// =============================================================================
// Public entry point
// =============================================================================

/// Run the organizer and return a report.
///
/// Reads all present files from the database, computes a destination path for
/// each one, and moves them (unless dry-run). Writes an undo log on success.
pub fn run(conn: &Connection, args: &OrganizeArgs) -> Result<OrganizeReport> {
    let mut report = OrganizeReport::default();
    let mut undo_entries: Vec<UndoEntry> = Vec::new();

    // Fetch every file currently present in the database that lives under
    // the source directory. We'll filter to the source dir below.
    let all_files = db::get_all_present_files(conn)
        .context("could not load files from database")?;

    // Canonicalize the source path so comparison with stored paths works.
    let source_root = args.source.canonicalize()
        .with_context(|| format!("source directory not found: {}", args.source.display()))?;

    // Filter to only files that are under the source directory.
    let files: Vec<FileRecord> = all_files
        .into_iter()
        .filter(|f| f.path.starts_with(source_root.to_str().unwrap_or("")))
        .collect();

    if files.is_empty() {
        info!(
            "No scanned files found under {}. Run `scan` first.",
            source_root.display()
        );
        return Ok(report);
    }

    info!(
        "Organizing {} files from {} → {}",
        files.len(),
        source_root.display(),
        args.dest.display()
    );

    // Ensure the destination root exists (unless dry-run).
    if !args.dry_run {
        fs::create_dir_all(&args.dest)
            .with_context(|| format!("could not create destination: {}", args.dest.display()))?;
    }

    // Process each file.
    for file in &files {
        let dest_path = compute_destination(&file, &args.dest, &args.mode);

        if args.dry_run {
            // Print the planned move without touching anything.
            println!("{}\n  → {}", file.path, dest_path.display());
            report.files_moved += 1;
            continue;
        }

        // Check for destination conflict.
        if dest_path.exists() && !args.overwrite {
            warn!("Conflict: destination exists: {}", dest_path.display());
            report.conflicts.push(Conflict {
                source:      file.path.clone(),
                destination: dest_path.to_string_lossy().to_string(),
            });
            continue;
        }

        // Attempt the move.
        match move_file(Path::new(&file.path), &dest_path) {
            Ok(_) => {
                debug!("Moved: {} → {}", file.path, dest_path.display());
                report.files_moved += 1;
                undo_entries.push(UndoEntry {
                    source:      file.path.clone(),
                    destination: dest_path.to_string_lossy().to_string(),
                    moved_at:    Utc::now(),
                });
            }
            Err(e) => {
                warn!("Could not move {}: {}", file.path, e);
                report.errors.push(format!("{}: {}", file.path, e));
            }
        }
    }

    // Write the undo log if any files were actually moved.
    if !undo_entries.is_empty() {
        let undo_path = write_undo_log(&args.dest, &undo_entries)
            .context("could not write undo log")?;
        report.undo_log_path = Some(undo_path);
    }

    info!(
        "Organize complete: {} moved, {} conflicts, {} errors",
        report.files_moved,
        report.conflicts.len(),
        report.errors.len(),
    );

    Ok(report)
}

// =============================================================================
// Destination path computation
// =============================================================================

/// Compute the destination path for a single file under `dest_root`.
///
/// The path structure depends on `mode`:
/// - ByTypeAndDate → `<dest>/<Category>/<Year>/<Month-Name>/<filename>`
/// - ByDate        → `<dest>/<Year>/<Month-Name>/<filename>`
/// - ByType        → `<dest>/<Category>/<filename>`
pub fn compute_destination(file: &FileRecord, dest_root: &Path, mode: &OrganizeMode) -> PathBuf {
    // Convert the file's modification timestamp into its year and month.
    // We use modified_at because that's closest to when the content was created.
    let year  = file.modified_at.year();
    let month = month_name(file.modified_at.month()); // e.g. "03-March"

    // Convert the FileCategory enum to a human-readable folder name.
    let category_dir = category_folder(&file.category);

    // The file's base name (e.g. "IMG_0042.jpg") stays the same after moving.
    let filename = Path::new(&file.path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Build the destination path according to the selected mode.
    match mode {
        OrganizeMode::ByTypeAndDate => dest_root
            .join(category_dir)          // e.g. dest/Photos/
            .join(year.to_string())      // e.g. dest/Photos/2024/
            .join(month)                 // e.g. dest/Photos/2024/03-March/
            .join(filename),             // e.g. dest/Photos/2024/03-March/IMG_0042.jpg

        OrganizeMode::ByDate => dest_root
            .join(year.to_string())      // e.g. dest/2024/
            .join(month)                 // e.g. dest/2024/03-March/
            .join(filename),             // e.g. dest/2024/03-March/IMG_0042.jpg

        OrganizeMode::ByType => dest_root
            .join(category_dir)          // e.g. dest/Documents/
            .join(filename),             // e.g. dest/Documents/report.pdf
    }
}

/// Map a `FileCategory` to a human-readable folder name.
fn category_folder(category: &FileCategory) -> &'static str {
    // Using descriptive names (not the enum names) so the resulting folders
    // look natural to a non-technical user browsing with a file manager.
    match category {
        FileCategory::Image    => "Photos",
        FileCategory::Video    => "Videos",
        FileCategory::Audio    => "Music",
        FileCategory::Document => "Documents",
        FileCategory::Archive  => "Archives",
        FileCategory::Code     => "Code",
        FileCategory::Other    => "Other",
    }
}

/// Format a month number (1–12) as a zero-padded "MM-MonthName" string.
///
/// Including the number prefix keeps months in chronological order when
/// sorted alphabetically (01-January sorts before 02-February, etc.).
fn month_name(month: u32) -> String {
    // A fixed lookup array indexed by month number (1-based).
    // We use an array instead of a match because the indexing is cleaner
    // and the compiler can verify we haven't missed any month.
    const MONTHS: [&str; 12] = [
        "01-January", "02-February", "03-March",
        "04-April",   "05-May",      "06-June",
        "07-July",    "08-August",   "09-September",
        "10-October", "11-November", "12-December",
    ];
    // Months are 1-indexed; array is 0-indexed. Clamp to valid range defensively.
    MONTHS[(month.clamp(1, 12) - 1) as usize].to_string()
}

// =============================================================================
// File move
// =============================================================================

/// Move a file from `source` to `destination`.
///
/// Creates all required parent directories at the destination before moving.
/// Uses `fs::rename` for same-filesystem moves (atomic, no data copying).
/// Falls back to copy-then-delete for cross-filesystem moves (different drives).
fn move_file(source: &Path, destination: &Path) -> Result<()> {
    // Create the destination directory tree if it doesn't exist.
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create directory: {}", parent.display()))?;
    }

    // Try a rename first — this is atomic on the same filesystem and requires
    // no data copying (just an inode update). This is the fast path.
    match fs::rename(source, destination) {
        Ok(_) => return Ok(()),
        Err(e) if cross_device_error(&e) => {
            // `rename` fails with EXDEV when source and destination are on
            // different filesystems/mount points. Fall through to copy+delete.
            debug!("Cross-device move detected, using copy+delete for {}", source.display());
        }
        Err(e) => {
            // Any other error (permission denied, file not found, etc.) is fatal.
            return Err(e).with_context(|| {
                format!("could not rename {} → {}", source.display(), destination.display())
            });
        }
    }

    // Cross-filesystem fallback: copy the file data, then delete the source.
    // `fs::copy` preserves file content but not all metadata (timestamps, etc.)
    // on all platforms — acceptable for a personal organizer.
    fs::copy(source, destination)
        .with_context(|| format!("could not copy {} → {}", source.display(), destination.display()))?;

    fs::remove_file(source)
        .with_context(|| format!("could not remove source after copy: {}", source.display()))?;

    Ok(())
}

/// Returns true if the I/O error is an EXDEV (cross-device link) error.
///
/// `fs::rename` returns this error code when the source and destination are
/// on different filesystems. We check for it to trigger the copy+delete path.
fn cross_device_error(e: &std::io::Error) -> bool {
    // `raw_os_error()` returns the underlying OS error code (e.g. 18 = EXDEV on Linux).
    // We compare against the `libc` constant for portability, or just use the
    // raw code directly since we target Linux only.
    e.raw_os_error() == Some(18) // 18 = EXDEV on Linux
}

// =============================================================================
// Undo log
// =============================================================================

/// Write the undo log to `<dest_root>/.fortress_undo.json`.
///
/// Appends to an existing log if one is already present (from a previous run),
/// so multiple organize passes build up a complete undo history.
fn write_undo_log(dest_root: &Path, entries: &[UndoEntry]) -> Result<PathBuf> {
    let log_path = dest_root.join(".fortress_undo.json");

    // Read the existing undo log if it exists, so we can append to it.
    let mut existing: Vec<UndoEntry> = if log_path.exists() {
        let contents = fs::read_to_string(&log_path)
            .context("could not read existing undo log")?;
        serde_json::from_str(&contents).unwrap_or_default()
    } else {
        Vec::new()
    };

    // Append the new entries to the existing ones.
    existing.extend_from_slice(entries);

    // Write the combined log back to disk as pretty-printed JSON.
    let json = serde_json::to_string_pretty(&existing)
        .context("could not serialize undo log")?;

    fs::write(&log_path, json)
        .with_context(|| format!("could not write undo log at {}", log_path.display()))?;

    info!("Undo log written: {}", log_path.display());
    Ok(log_path)
}

// =============================================================================
// Report printing
// =============================================================================

/// Print a human-readable organize report to stdout.
pub fn print_report(report: &OrganizeReport, dry_run: bool) {
    if dry_run {
        println!("\n[dry-run] {} file(s) would be moved.", report.files_moved);
        return;
    }

    println!("\nOrganize complete:");
    println!("  {} file(s) moved", report.files_moved);

    if !report.conflicts.is_empty() {
        println!("  {} conflict(s) skipped (destination already exists):", report.conflicts.len());
        for c in &report.conflicts {
            println!("    {} → {}", c.source, c.destination);
        }
    }

    if !report.errors.is_empty() {
        println!("  {} error(s):", report.errors.len());
        for e in &report.errors {
            println!("    ✗ {}", e);
        }
    }

    if let Some(ref undo_path) = report.undo_log_path {
        println!("\n  Undo log: {}", undo_path.display());
        println!("  To reverse: inspect {} and rename files back.", undo_path.display());
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use crate::models::FileCategory;

    /// Build a minimal FileRecord with a controlled timestamp for testing.
    fn make_record(path: &str, year: i32, month: u32, category: FileCategory) -> FileRecord {
        FileRecord {
            id:           None,
            path:         path.to_string(),
            name:         Path::new(path).file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
            extension:    "jpg".to_string(),
            category,
            mime_type:    "image/jpeg".to_string(),
            size_bytes:   1024,
            content_hash: None,
            modified_at:  Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).unwrap(),
            scanned_at:   Utc::now(),
            is_present:   true,
        }
    }

    #[test]
    fn test_by_type_and_date_destination() {
        let record = make_record("/chaos/IMG_001.jpg", 2024, 3, FileCategory::Image);
        let dest = compute_destination(&record, Path::new("/sorted"), &OrganizeMode::ByTypeAndDate);
        assert_eq!(dest, Path::new("/sorted/Photos/2024/03-March/IMG_001.jpg"));
    }

    #[test]
    fn test_by_date_destination() {
        let record = make_record("/chaos/report.pdf", 2023, 11, FileCategory::Document);
        let dest = compute_destination(&record, Path::new("/sorted"), &OrganizeMode::ByDate);
        assert_eq!(dest, Path::new("/sorted/2023/11-November/report.pdf"));
    }

    #[test]
    fn test_by_type_destination() {
        let record = make_record("/chaos/song.mp3", 2024, 6, FileCategory::Audio);
        let dest = compute_destination(&record, Path::new("/sorted"), &OrganizeMode::ByType);
        assert_eq!(dest, Path::new("/sorted/Music/song.mp3"));
    }

    #[test]
    fn test_month_name_all_months() {
        assert_eq!(month_name(1),  "01-January");
        assert_eq!(month_name(6),  "06-June");
        assert_eq!(month_name(12), "12-December");
    }

    #[test]
    fn test_month_name_clamps_invalid_values() {
        // Months outside 1–12 should clamp gracefully, not panic.
        assert_eq!(month_name(0),  "01-January");  // 0 clamps to 1
        assert_eq!(month_name(13), "12-December"); // 13 clamps to 12
    }

    #[test]
    fn test_category_folder_names() {
        assert_eq!(category_folder(&FileCategory::Image),    "Photos");
        assert_eq!(category_folder(&FileCategory::Video),    "Videos");
        assert_eq!(category_folder(&FileCategory::Audio),    "Music");
        assert_eq!(category_folder(&FileCategory::Document), "Documents");
        assert_eq!(category_folder(&FileCategory::Archive),  "Archives");
        assert_eq!(category_folder(&FileCategory::Code),     "Code");
        assert_eq!(category_folder(&FileCategory::Other),    "Other");
    }

    #[test]
    fn test_move_file_creates_parent_dirs() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Create a source file.
        let src = dir.path().join("source.txt");
        std::fs::File::create(&src).unwrap()
            .write_all(b"hello").unwrap();

        // Destination is in a deeply nested directory that doesn't exist yet.
        let dest = dir.path().join("deep/nested/dir/dest.txt");

        // move_file should create all parent directories.
        move_file(&src, &dest).unwrap();

        assert!(dest.exists(),  "destination should exist after move");
        assert!(!src.exists(),  "source should be gone after move");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "hello");
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY read from the database instead of walking the filesystem directly?
//    The database is the single source of truth. By the time the user runs
//    `organize`, the scanner has already classified every file. Reading from
//    the database means the organizer gets category, MIME type, and timestamps
//    without having to re-read every file on disk — much faster and consistent.
//
// 2. WHY fs::rename before fs::copy?
//    `rename` is a single atomic syscall on the same filesystem — it moves the
//    file's directory entry without copying any data. It's instantaneous even
//    for large files. `copy` reads every byte and writes it again — for a
//    100 GB video this takes minutes. We only fall back to copy+delete when
//    the source and destination are on different mount points (EXDEV error).
//
// 3. WHY zero-padded month numbers ("03-March" not "March")?
//    File managers sort folder names alphabetically. "April" sorts before
//    "March" alphabetically, breaking chronological order. "03-March" and
//    "04-April" sort correctly because the numbers come first. This is a
//    common convention for date-based folder structures.
//
// 4. WHY an undo log?
//    Moving thousands of files is hard to reverse manually. The undo log
//    records every source→destination pair as JSON. A future `undo` command
//    (or a simple script) can read it and rename everything back. This turns
//    a potentially destructive operation into a recoverable one.
//
// 5. WHY append to an existing undo log instead of overwriting?
//    The user might run organize multiple times with different source dirs.
//    Overwriting the undo log on each run would erase the history of previous
//    runs, making older moves impossible to undo. Appending preserves the
//    complete history of all organize operations.
//
// 6. WHY clamp month values in month_name?
//    `chrono::Datelike::month()` always returns 1–12, so this can't happen
//    in production. But tests and future callers might pass unexpected values.
//    Clamping instead of panicking makes the function robust — it degrades
//    gracefully rather than crashing.

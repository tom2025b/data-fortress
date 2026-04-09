//! # dedup/mod.rs
//!
//! Duplicate file detection and removal for Data Fortress.
//!
//! Deduplication works in two steps:
//!
//! 1. **Hash step** — any files without a `content_hash` in the database are
//!    hashed with BLAKE3 (delegated to `hasher.rs`). Files already hashed by
//!    a previous scan are skipped.
//!
//! 2. **Group step** — the database is queried for all `content_hash` values
//!    that appear on more than one file. Each such group is a set of confirmed
//!    duplicates. The groups are returned ordered by wasted bytes descending
//!    so the biggest wins appear first in reports and the dashboard.
//!
//! ## Deletion strategy
//!
//! When the user passes `--delete`, we keep exactly one file per group
//! according to the configured `KeepStrategy` and delete the rest.
//! A dry-run mode prints what would be deleted without touching the filesystem.

pub mod hasher;

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tracing::{info, warn};

use crate::cli::KeepStrategy;
use crate::db;
use crate::models::{DuplicateGroup, FileRecord};
use crate::dedup::hasher::{hash_files_parallel, HashResult};

// =============================================================================
// Public entry point
// =============================================================================

/// Options controlling a deduplication run.
#[derive(Debug, Clone)]
pub struct DedupOptions {
    /// Hash files without a content hash before searching for duplicates.
    pub hash_first: bool,

    /// Only report groups where each copy is at least this many bytes.
    /// Set to 0 to include all duplicates regardless of size.
    pub min_size: u64,

    /// Delete all but one copy of each duplicate group.
    pub delete: bool,

    /// Which copy to keep when deleting.
    pub keep: KeepStrategy,

    /// Print what would happen without touching the filesystem or database.
    pub dry_run: bool,
}

/// Result returned to `main.rs` after a dedup run.
#[derive(Debug, Default)]
pub struct DedupReport {
    /// Total number of duplicate groups found.
    pub groups_found: usize,

    /// Total bytes that could be (or were) reclaimed.
    pub wasted_bytes: u64,

    /// Number of files deleted (0 if dry_run or delete was not requested).
    pub files_deleted: usize,

    /// Paths that could not be deleted (permission errors, etc.).
    pub delete_errors: Vec<String>,

    /// The full list of duplicate groups, for display in the dashboard/CLI.
    pub groups: Vec<DuplicateGroup>,
}

/// Run duplicate detection and return a `DedupReport`.
///
/// Called from `main.rs` after parsing CLI args into `DedupOptions`.
pub fn run(conn: &Connection, opts: &DedupOptions) -> Result<DedupReport> {
    // Step 1: hash any files that don't have a content hash yet.
    if opts.hash_first {
        info!("Hashing files without a content hash…");
        hash_pending(conn, u64::MAX)?; // u64::MAX = no size limit during explicit dedup
    }

    // Step 2: query the database for duplicate groups.
    info!("Searching for duplicate groups…");
    let mut groups = db::get_duplicate_groups(conn)
        .context("could not fetch duplicate groups from database")?;

    // Apply the minimum size filter if the user specified --min-size.
    if opts.min_size > 0 {
        // Keep only groups where files are at least min_size bytes each.
        // `files[0].size_bytes` is representative — all files in a group share
        // the same content, so they all have the same size.
        groups.retain(|g| {
            g.files.first().map_or(false, |f| f.size_bytes >= opts.min_size)
        });
    }

    let groups_found  = groups.len();
    let wasted_bytes  = groups.iter().map(|g| g.wasted_bytes).sum();

    let mut report = DedupReport {
        groups_found,
        wasted_bytes,
        groups: groups.clone(),
        ..Default::default()
    };

    // Step 3: delete duplicates if requested.
    if opts.delete {
        let (deleted, errors) = delete_duplicates(&groups, &opts.keep, opts.dry_run)?;
        report.files_deleted  = deleted;
        report.delete_errors  = errors;
    }

    info!(
        "Dedup complete: {} groups, {} wasted, {} deleted",
        report.groups_found,
        report.wasted_bytes,
        report.files_deleted,
    );

    Ok(report)
}

// =============================================================================
// Hashing
// =============================================================================

/// Hash all files in the database that do not yet have a content hash,
/// up to `max_size` bytes, and write the results back to the database.
pub fn hash_pending(conn: &Connection, max_size: u64) -> Result<()> {
    // Fetch paths of all files that still need hashing.
    let files = db::get_unhashed_files(conn, max_size)
        .context("could not fetch unhashed files")?;

    if files.is_empty() {
        info!("All files already have a content hash — nothing to hash.");
        return Ok(());
    }

    info!("Hashing {} files in parallel…", files.len());

    // Extract just the path strings — that's all the hasher needs.
    let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();

    // Hash all files in parallel across rayon's thread pool.
    let results = hash_files_parallel(&paths);

    // Write successful hashes back to the database in a single transaction.
    // One transaction for all writes is orders of magnitude faster than one
    // transaction per write (avoids repeated disk flushes).
    let tx = conn.unchecked_transaction()
        .context("could not begin hash write transaction")?;

    let mut written = 0usize;
    let mut failed  = 0usize;

    for result in results {
        match result {
            HashResult::Ok { path, hash } => {
                db::set_content_hash(&tx, &path, &hash)
                    .with_context(|| format!("could not write hash for {}", path))?;
                written += 1;
            }
            HashResult::Err { path, reason } => {
                warn!("Hash failed for {}: {}", path, reason);
                failed += 1;
            }
        }
    }

    tx.commit().context("could not commit hash writes")?;

    info!("Hashed {} files successfully, {} failed.", written, failed);
    Ok(())
}

// =============================================================================
// Duplicate detection helpers
// =============================================================================

/// Given a list of duplicate groups, select which file to keep per group.
///
/// Returns a reference to the `FileRecord` that should be retained.
/// All other files in the group are candidates for deletion.
pub fn select_keeper<'a>(group: &'a DuplicateGroup, strategy: &KeepStrategy) -> &'a FileRecord {
    // `group.files` is always non-empty (a group needs at least 2 files).
    // We panic here only as a programmer error guard, not a user-facing error.
    assert!(!group.files.is_empty(), "DuplicateGroup must have at least one file");

    match strategy {
        // Keep the file modified longest ago — most likely to be the "original".
        KeepStrategy::Oldest => group
            .files
            .iter()
            .min_by_key(|f| f.modified_at)
            .unwrap(), // safe: we checked files is non-empty above

        // Keep the most recently modified file.
        KeepStrategy::Newest => group
            .files
            .iter()
            .max_by_key(|f| f.modified_at)
            .unwrap(),

        // Keep the file whose path sorts first alphabetically.
        // Useful when you want to keep files in a specific directory structure.
        KeepStrategy::FirstAlpha => group
            .files
            .iter()
            .min_by(|a, b| a.path.cmp(&b.path))
            .unwrap(),

        // Keep the file with the shortest absolute path — tends to be the
        // one closest to the filesystem root, i.e. most "organised".
        KeepStrategy::ShortestPath => group
            .files
            .iter()
            .min_by_key(|f| f.path.len())
            .unwrap(),
    }
}

// =============================================================================
// Deletion
// =============================================================================

/// Delete all but one file from each duplicate group.
///
/// Returns `(files_deleted, error_paths)`.
/// Errors on individual files are collected rather than aborting — one
/// permission error shouldn't prevent all other deletions from proceeding.
fn delete_duplicates(
    groups: &[DuplicateGroup],
    strategy: &KeepStrategy,
    dry_run: bool,
) -> Result<(usize, Vec<String>)> {
    let mut deleted = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for group in groups {
        // Determine which file to keep.
        let keeper = select_keeper(group, strategy);

        // Delete all other files in the group.
        for file in &group.files {
            // Skip the file we decided to keep.
            if file.path == keeper.path {
                continue;
            }

            if dry_run {
                // In dry-run mode, just print what would be deleted.
                println!(
                    "[dry-run] Would delete: {}\n          Keeping:  {}",
                    file.path, keeper.path
                );
                deleted += 1; // Count in dry-run too, for accurate reporting
                continue;
            }

            // Attempt the actual deletion.
            match std::fs::remove_file(Path::new(&file.path)) {
                Ok(_) => {
                    info!("Deleted duplicate: {}", file.path);
                    deleted += 1;
                }
                Err(e) => {
                    // Log the error and record the path, but keep going.
                    warn!("Could not delete {}: {}", file.path, e);
                    errors.push(format!("{}: {}", file.path, e));
                }
            }
        }
    }

    Ok((deleted, errors))
}

// =============================================================================
// Formatting helpers
// =============================================================================

/// Format a byte count as a human-readable string.
///
/// Used by main.rs when printing the dedup report to the terminal.
/// Examples: 1_048_576 → "1.00 MiB", 2_305_843_009 → "2.15 GiB"
pub fn format_bytes(bytes: u64) -> String {
    // `bytesize::ByteSize` wraps a u64 and implements Display with unit labels.
    bytesize::ByteSize(bytes).to_string()
}

/// Print a human-readable dedup report to stdout.
///
/// Called by `main.rs` when `--json` is not set. The dashboard uses JSON
/// output instead and never calls this function.
pub fn print_report(report: &DedupReport) {
    if report.groups_found == 0 {
        println!("No duplicates found.");
        return;
    }

    println!(
        "\nFound {} duplicate group(s) — {} wasted\n",
        report.groups_found,
        format_bytes(report.wasted_bytes),
    );

    for (i, group) in report.groups.iter().enumerate() {
        println!(
            "Group {} — {} ({} copies, {} wasted)",
            i + 1,
            &group.content_hash[..12], // Show only first 12 chars of the hash
            group.files.len(),
            format_bytes(group.wasted_bytes),
        );

        for file in &group.files {
            println!(
                "  {} ({})",
                file.path,
                format_bytes(file.size_bytes),
            );
        }
        println!();
    }

    if report.files_deleted > 0 {
        println!("Deleted {} file(s).", report.files_deleted);
    }

    if !report.delete_errors.is_empty() {
        println!("\nErrors during deletion:");
        for err in &report.delete_errors {
            println!("  ✗ {}", err);
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crate::models::FileCategory;

    /// Helper: build a minimal FileRecord for testing.
    fn make_record(path: &str, modified_offset_secs: i64, size: u64) -> FileRecord {
        FileRecord {
            id:           None,
            path:         path.to_string(),
            name:         path.split('/').last().unwrap_or("").to_string(),
            extension:    "txt".to_string(),
            category:     FileCategory::Document,
            mime_type:    "text/plain".to_string(),
            size_bytes:   size,
            content_hash: Some("abc123".to_string()),
            // Use a fixed base time plus an offset so we can control ordering.
            modified_at:  Utc::now() + chrono::Duration::seconds(modified_offset_secs),
            scanned_at:   Utc::now(),
            is_present:   true,
        }
    }

    fn make_group(files: Vec<FileRecord>) -> DuplicateGroup {
        let size = files.first().map_or(0, |f| f.size_bytes);
        let wasted = size * (files.len() as u64 - 1);
        DuplicateGroup {
            content_hash: "abc123".to_string(),
            wasted_bytes: wasted,
            files,
        }
    }

    #[test]
    fn test_keep_oldest_selects_earliest_mtime() {
        let group = make_group(vec![
            make_record("/a/new.txt",  100, 100), // newest
            make_record("/b/old.txt", -100, 100), // oldest ← should be kept
            make_record("/c/mid.txt",    0, 100),
        ]);
        let keeper = select_keeper(&group, &KeepStrategy::Oldest);
        assert_eq!(keeper.path, "/b/old.txt");
    }

    #[test]
    fn test_keep_newest_selects_latest_mtime() {
        let group = make_group(vec![
            make_record("/a/new.txt",  100, 100), // newest ← should be kept
            make_record("/b/old.txt", -100, 100),
            make_record("/c/mid.txt",    0, 100),
        ]);
        let keeper = select_keeper(&group, &KeepStrategy::Newest);
        assert_eq!(keeper.path, "/a/new.txt");
    }

    #[test]
    fn test_keep_first_alpha_selects_alphabetically_first() {
        let group = make_group(vec![
            make_record("/z/file.txt", 0, 100),
            make_record("/a/file.txt", 0, 100), // ← alphabetically first
            make_record("/m/file.txt", 0, 100),
        ]);
        let keeper = select_keeper(&group, &KeepStrategy::FirstAlpha);
        assert_eq!(keeper.path, "/a/file.txt");
    }

    #[test]
    fn test_keep_shortest_path_selects_shortest() {
        let group = make_group(vec![
            make_record("/very/long/path/to/file.txt", 0, 100),
            make_record("/short/file.txt",              0, 100), // ← shortest
            make_record("/medium/path/file.txt",        0, 100),
        ]);
        let keeper = select_keeper(&group, &KeepStrategy::ShortestPath);
        assert_eq!(keeper.path, "/short/file.txt");
    }

    #[test]
    fn test_format_bytes_readable() {
        // Verify format_bytes produces a non-empty string with a unit suffix.
        // We don't pin exact formatting — bytesize's thresholds are its own.
        let zero = format_bytes(0);
        let kb   = format_bytes(1_024);
        let big  = format_bytes(1_000_000_000);

        assert!(!zero.is_empty());
        assert!(!kb.is_empty());
        assert!(!big.is_empty());

        // Zero bytes must say "B" (bytes), not a larger unit.
        assert!(zero.contains('B'));
        // 1 GB range should contain a 'B' (part of GB/KB/MB/etc.)
        assert!(big.contains('B'));
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY query the database for duplicates instead of comparing files in memory?
//    With hundreds of thousands of files, loading all FileRecords into memory
//    would use gigabytes of RAM. SQLite's GROUP BY + HAVING query runs in O(n)
//    time with an index on content_hash and uses only O(k) memory where k is
//    the number of duplicate groups — typically a tiny fraction of all files.
//
// 2. WHY collect all groups before deleting?
//    We separate "find duplicates" from "delete files" so the user can review
//    the report before committing to deletions. The dry-run flag takes advantage
//    of this separation: the same deletion code path runs, but with fs::remove_file
//    replaced by a println. This guarantees dry-run output matches real output.
//
// 3. WHY `assert!` instead of returning an error in select_keeper?
//    `select_keeper` can only receive a DuplicateGroup that came from the
//    database query, which only returns groups with ≥ 2 files by definition.
//    An empty group is a bug in our own code, not a user error or I/O failure.
//    Panicking on programmer errors (assert!) and returning Err for runtime
//    errors is idiomatic Rust — it makes the distinction explicit.
//
// 4. WHY collect delete errors instead of returning the first one?
//    File deletions can fail for many reasons: permission denied on one file,
//    file moved by another process, etc. Stopping at the first error would
//    leave the other duplicates intact and waste the work already done.
//    Collecting errors lets the user see all failures at once and fix them.
//
// 5. WHY `group.files[0].size_bytes` to represent the group's file size?
//    All files in a DuplicateGroup have identical content (same BLAKE3 hash),
//    so they all have the same size. Using the first file's size is not an
//    approximation — it is exact for all members of the group.

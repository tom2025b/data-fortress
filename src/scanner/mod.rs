//! # scanner/mod.rs
//!
//! Recursive file system scanner for Data Fortress.
//!
//! The scanner walks one or more root directories, classifies every file it
//! finds, and writes `FileRecord`s into the SQLite database. It handles:
//!
//! - Exclusion of configured directories and extensions
//! - Updating existing records on re-scan (upsert)
//! - Marking files that have disappeared as `is_present = false`
//! - Real-time progress reporting via `indicatif`
//! - Structured statistics returned as `ScanStats`
//!
//! ## Design
//!
//! The scan runs in two phases:
//!   1. **Walk phase** — `walkdir` traverses the directory tree. For each file
//!      we classify it and upsert a `FileRecord`. This is I/O-bound so we run
//!      it on the current thread (walkdir is not parallel by design).
//!   2. **Hash phase** (optional) — After the walk, if `--hash` was requested,
//!      we fetch all unhashed files from the DB and hash them in parallel with
//!      `rayon`. This is CPU+I/O-bound and benefits greatly from parallelism.

pub mod classifier;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use indicatif::{ProgressBar, ProgressStyle};
use rusqlite::Connection;
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::config::Config;
use crate::db;
use crate::models::{FileRecord, ScanStats};
use crate::scanner::classifier::classify;

// =============================================================================
// Public entry point
// =============================================================================

/// Options controlling a single scan run.
///
/// Built by `main.rs` from the parsed CLI arguments and config.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Directories to scan. Must be non-empty.
    pub directories: Vec<PathBuf>,

    /// Whether to compute BLAKE3 content hashes after the walk phase.
    pub hash: bool,

    /// Maximum file size to hash in bytes. Files larger than this are skipped.
    pub max_hash_size: u64,

    /// If true, print what would be scanned but do not write to the database.
    pub dry_run: bool,
}

impl ScanOptions {
    /// Build `ScanOptions` from CLI scan args and the loaded config.
    pub fn from_args(
        directories: Vec<PathBuf>,
        hash: bool,
        max_hash_size: Option<u64>,
        dry_run: bool,
        config: &Config,
    ) -> Self {
        // Use directories from CLI if provided; fall back to config watch_dirs.
        let dirs = if directories.is_empty() {
            config.watch_dirs.clone()
        } else {
            directories
        };

        ScanOptions {
            directories: dirs,
            hash,
            // CLI overrides config; config provides the default.
            max_hash_size: max_hash_size.unwrap_or(config.max_hash_size_bytes),
            dry_run,
        }
    }
}

/// Run a full scan and return aggregate statistics.
///
/// This is the main entry point called from `main.rs`. It orchestrates both
/// the walk phase and the optional hash phase.
pub fn run(conn: &Connection, config: &Config, opts: &ScanOptions) -> Result<ScanStats> {
    // Reject early if no directories are configured — nothing to scan.
    if opts.directories.is_empty() {
        anyhow::bail!(
            "No directories to scan. Add one with: data-fortress config add-dir <PATH>"
        );
    }

    // Record the wall-clock start time so we can compute duration_ms.
    let start = Instant::now();

    // Accumulate statistics across all scanned directories.
    let mut stats = ScanStats::default();

    // Scan each root directory in sequence.
    for dir in &opts.directories {
        info!("Scanning directory: {}", dir.display());
        let dir_stats = scan_directory(conn, config, dir, opts)?;

        // Merge per-directory stats into the overall totals.
        stats.files_found   += dir_stats.files_found;
        stats.files_new     += dir_stats.files_new;
        stats.files_skipped += dir_stats.files_skipped;
        stats.total_bytes   += dir_stats.total_bytes;
    }

    // Phase 2: hash all files that don't have a content hash yet.
    if opts.hash && !opts.dry_run {
        info!("Starting hash phase…");
        hash_pending_files(conn, opts.max_hash_size)?;
    }

    // Record total elapsed time in milliseconds.
    stats.duration_ms = start.elapsed().as_millis() as u64;

    info!(
        "Scan complete: {} files found, {} new, {} skipped in {}ms",
        stats.files_found, stats.files_new, stats.files_skipped, stats.duration_ms
    );

    Ok(stats)
}

// =============================================================================
// Walk phase
// =============================================================================

/// Scan a single root directory and upsert all discovered files into the DB.
///
/// Before walking, marks all files under this root as absent. Any file that
/// is visited during the walk gets marked present again. Files not visited
/// (deleted since last scan) remain absent.
fn scan_directory(
    conn: &Connection,
    config: &Config,
    root: &Path,
    opts: &ScanOptions,
) -> Result<ScanStats> {
    let mut stats = ScanStats::default();

    // Canonicalize the path so we always store absolute, resolved paths.
    // Returns an error if the directory doesn't exist — caught below.
    let root = root.canonicalize()
        .with_context(|| format!("directory not found: {}", root.display()))?;

    // Mark all previously known files under this root as absent.
    // After the walk, anything still absent was deleted since the last scan.
    if !opts.dry_run {
        db::mark_all_absent(conn, root.to_str().unwrap_or(""))
            .context("could not mark files as absent before scan")?;
    }

    // Set up a progress spinner. indicatif renders this in-place on the
    // terminal so the user sees live feedback during long scans.
    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} [{elapsed_precise}] {msg}")
            // unwrap is safe — the template string above is a compile-time constant
            .unwrap(),
    );
    progress.set_message(format!("Scanning {}", root.display()));

    // `WalkDir` recursively traverses the directory tree.
    // `follow_links(false)` prevents infinite loops from circular symlinks.
    // `same_file_system(true)` keeps the walker on the same mount point —
    // important when /mnt/drive2 is mounted inside /home.
    let walker = WalkDir::new(&root)
        .follow_links(false)
        .same_file_system(true)
        .into_iter();

    // `filter_entry` prunes entire subtrees: if we return false for a
    // directory, walkdir does not descend into it at all (more efficient
    // than letting it recurse and skipping files inside).
    for entry in walker.filter_entry(|e| !should_skip_entry(e, config)) {
        // Handle errors on individual entries (e.g. permission denied on a
        // subdirectory). We log a warning and continue rather than aborting.
        let entry = match entry {
            Ok(e)  => e,
            Err(e) => {
                warn!("Skipping inaccessible entry: {}", e);
                stats.files_skipped += 1;
                continue;
            }
        };

        // Skip directories themselves — we only record files.
        if entry.file_type().is_dir() {
            continue;
        }

        // Skip symlinks to avoid recording the same content twice.
        if entry.file_type().is_symlink() {
            debug!("Skipping symlink: {}", entry.path().display());
            stats.files_skipped += 1;
            continue;
        }

        // Process this file entry.
        match process_file(conn, entry.path(), config, opts, &mut stats) {
            Ok(_)  => {}
            Err(e) => {
                // Log errors per-file but keep scanning. One bad file should
                // not abort a scan of hundreds of thousands of files.
                warn!("Error processing {}: {}", entry.path().display(), e);
                stats.files_skipped += 1;
            }
        }

        // Update the spinner message every file so the user knows we're alive.
        progress.set_message(format!(
            "Scanning {} | {} files found",
            root.display(),
            stats.files_found
        ));
        progress.tick();
    }

    progress.finish_with_message(format!(
        "Done: {} ({} files, {} new)",
        root.display(),
        stats.files_found,
        stats.files_new,
    ));

    Ok(stats)
}

/// Returns `true` if this directory entry should be skipped entirely.
///
/// Called by `filter_entry` for every entry (file or directory) before
/// walkdir decides whether to descend. Returning true prunes the subtree.
fn should_skip_entry(entry: &walkdir::DirEntry, config: &Config) -> bool {
    // Get the bare file/directory name (not the full path).
    let name = entry.file_name().to_string_lossy();

    // Always skip hidden directories (starting with '.'), except '.' itself.
    // This skips .git, .cache, .local, .config, etc. without listing them all.
    if entry.file_type().is_dir() && name.starts_with('.') && name != "." {
        return true;
    }

    // Skip any directory explicitly listed in config.exclude_dirs.
    if entry.file_type().is_dir() && config.should_exclude_dir(&name) {
        debug!("Excluding directory: {}", entry.path().display());
        return true;
    }

    // For files, skip if the extension is in config.exclude_extensions.
    if entry.file_type().is_file() {
        let ext = entry.path()
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        if config.should_exclude_extension(&ext) {
            return true;
        }
    }

    // Not skipped — walkdir should process this entry.
    false
}

/// Process a single file: classify it, read its metadata, and upsert to DB.
fn process_file(
    conn: &Connection,
    path: &Path,
    _config: &Config,
    opts: &ScanOptions,
    stats: &mut ScanStats,
) -> Result<()> {
    // Read filesystem metadata (size, timestamps) without opening the file.
    // `metadata()` makes one syscall; much cheaper than opening the file.
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("could not read metadata for {}", path.display()))?;

    // Extract the file size. `metadata.len()` returns bytes as u64.
    let size_bytes = metadata.len();

    // Extract the last-modified timestamp.
    // `modified()` can fail on some filesystems (FAT32, some FUSE mounts).
    let modified_at = metadata.modified()
        .map(|t| chrono::DateTime::<Utc>::from(t))
        .unwrap_or_else(|_| Utc::now()); // Fall back to now if unavailable.

    // Classify the file to get its MIME type and category.
    // `classify()` never fails — returns Other if type cannot be determined.
    let classification = classify(path);

    // Extract file name components.
    let name = path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let extension = path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // Build the FileRecord. `id` is None — db::upsert_file will assign it.
    let record = FileRecord {
        id:           None,
        path:         path.to_string_lossy().to_string(),
        name,
        extension,
        category:     classification.category,
        mime_type:    classification.mime_type,
        size_bytes,
        content_hash: None, // Hashing happens in phase 2
        modified_at,
        scanned_at:   Utc::now(),
        is_present:   true,
    };

    // Update stats.
    stats.files_found += 1;
    stats.total_bytes += size_bytes;

    if opts.dry_run {
        // In dry-run mode, print what we would record without touching the DB.
        println!("[dry-run] Would record: {}", record.path);
        return Ok(());
    }

    // Check if this path already exists in the DB to track new-vs-updated.
    let is_new = db::get_file_by_path(conn, &record.path)?.is_none();
    if is_new {
        stats.files_new += 1;
    }

    // Upsert: insert if new, update if the path already exists.
    db::upsert_file(conn, &record)
        .with_context(|| format!("could not upsert record for {}", record.path))?;

    // Mark this file as present (undoes the mark_all_absent from earlier).
    db::mark_present(conn, &record.path)
        .with_context(|| format!("could not mark present: {}", record.path))?;

    debug!("Recorded: {} ({} bytes)", record.path, size_bytes);
    Ok(())
}

// =============================================================================
// Hash phase
// =============================================================================

/// Hash all files in the database that don't have a content hash yet.
///
/// Fetches the list of unhashed files from SQLite, then uses rayon to hash
/// them in parallel across all available CPU cores. Each hash is written back
/// to the database individually as it completes.
fn hash_pending_files(conn: &Connection, max_hash_size: u64) -> Result<()> {
    use crate::dedup::hasher::hash_file;
    use rayon::prelude::*;

    // Fetch all files that still need hashing, filtered by size limit.
    let files = db::get_unhashed_files(conn, max_hash_size)
        .context("could not fetch unhashed files")?;

    if files.is_empty() {
        info!("All files already hashed.");
        return Ok(());
    }

    info!("Hashing {} files…", files.len());

    // Set up a progress bar that counts down to zero.
    let progress = ProgressBar::new(files.len() as u64);
    progress.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.cyan} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} files hashed")
            .unwrap()
            .progress_chars("=>-"),
    );

    // Collect (path, hash) pairs. rayon's par_iter() distributes the hashing
    // work across all CPU cores automatically — each file is hashed on
    // whichever thread picks it up next from the work-stealing queue.
    //
    // Note: we cannot write to `conn` from multiple threads (rusqlite
    // Connection is not Send). Instead we compute hashes in parallel and
    // collect the results, then write them serially below.
    let results: Vec<(String, Option<String>)> = files
        .par_iter()
        .map(|file| {
            let hash = match hash_file(std::path::Path::new(&file.path)) {
                Ok(h)  => Some(h),
                Err(e) => {
                    warn!("Could not hash {}: {}", file.path, e);
                    None
                }
            };
            progress.inc(1);
            (file.path.clone(), hash)
        })
        .collect();

    progress.finish_with_message("Hashing complete.");

    // Write computed hashes back to the database serially.
    // SQLite performs best with batched writes inside a transaction.
    let tx = conn.unchecked_transaction()
        .context("could not begin hash write transaction")?;

    for (path, hash) in results {
        if let Some(h) = hash {
            db::set_content_hash(&tx, &path, &h)
                .with_context(|| format!("could not save hash for {}", path))?;
        }
    }

    // Commit all hash writes in a single transaction — much faster than one
    // transaction per file, which would force a disk flush after each write.
    tx.commit().context("could not commit hash writes")?;

    Ok(())
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY mark_all_absent before scanning?
//    Without this two-phase approach, we'd have no way to detect deleted files.
//    By marking everything absent first, then marking each visited file present,
//    any file still absent at the end was not seen during the scan — meaning it
//    was deleted, moved, or is on a drive that wasn't mounted.
//
// 2. WHY walkdir instead of std::fs::read_dir?
//    `std::fs::read_dir` only lists one directory level. To recurse, you'd
//    need to write the recursion yourself, handle errors, and manage the stack.
//    `walkdir` does all of this correctly, including handling deeply nested
//    trees without stack overflow (it uses its own internal stack).
//
// 3. WHY filter_entry instead of filtering after the fact?
//    `filter_entry` prunes entire subtrees. If we skip `node_modules/` via
//    `filter_entry`, walkdir never reads its contents — potentially millions
//    of files never touched. Filtering after the fact would still recurse into
//    the directory and check every file, wasting I/O.
//
// 4. WHY collect hashes then write serially?
//    `rusqlite::Connection` does not implement `Send` — it cannot be shared
//    across threads safely. rayon distributes work across threads, so we
//    can't write to the DB from inside the `par_iter` closure. The solution:
//    compute in parallel (no shared state), collect results, write serially.
//    The write is fast compared to the hashing, so this is fine in practice.
//
// 5. WHY wrap the serial writes in a transaction?
//    Each `UPDATE` without an explicit transaction is auto-committed, meaning
//    SQLite flushes to disk after every single write. With 100,000 files, that
//    is 100,000 disk flushes. Wrapping in one transaction reduces this to a
//    single flush at commit time — orders of magnitude faster.
//
// 6. WHY `same_file_system(true)` on walkdir?
//    Without this, walkdir would follow mount points and recurse into any
//    filesystem mounted inside the scan root. A scan of /home would descend
//    into /home/tom/mnt/drive2 if it were mounted there, potentially scanning
//    the same drive twice. `same_file_system` prevents this.

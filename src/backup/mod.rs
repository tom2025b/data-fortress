//! # backup/mod.rs
//!
//! Versioned backup system for Data Fortress.
//!
//! Creates compressed archives of files recorded in the database, writes a
//! manifest listing every included file, and records the backup in SQLite so
//! the dashboard can display backup history.
//!
//! ## Archive format
//!
//! Each backup produces two files in `<backup_dir>/`:
//!
//! ```text
//! <backup_dir>/
//! ├── backup-2025-04-08-a1b2c3d4.tar.zst   ← compressed archive
//! └── backup-2025-04-08-a1b2c3d4.json      ← manifest (file list + metadata)
//! ```
//!
//! The archive is a TAR stream compressed with Zstandard (zstd). TAR is used
//! because it preserves file paths and can store many files without nesting
//! them — the archive can be extracted with standard tools (`tar -xf`).
//!
//! ## Manifest
//!
//! The JSON manifest records: label, created_at, compression level, and a
//! list of every included file (path, size, hash). The manifest makes it
//! possible to verify archive integrity and restore individual files without
//! extracting the entire archive.
//!
//! ## Compression
//!
//! zstd level 3 is the default — roughly 2–4× faster than gzip at similar
//! or better compression ratios. Level 1 maximises speed; level 19+ maximises
//! compression (useful for cold-storage archives you write once and rarely read).

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use indicatif::{ProgressBar, ProgressStyle};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use uuid::Uuid;

use crate::cli::{BackupCreateArgs, SearchCategory};
use crate::config::Config;
use crate::db;
use crate::models::{BackupRecord, FileCategory, FileRecord};

// =============================================================================
// Manifest types
// =============================================================================

/// The JSON manifest written alongside each backup archive.
///
/// Stored as `<archive_name>.json`. Contains enough information to verify
/// the archive and restore individual files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    /// Unique identifier for this backup. Same UUID as in the archive filename.
    pub id: String,

    /// Human-readable label (e.g. "weekly-2025-04-08").
    pub label: String,

    /// When this backup was created (UTC ISO 8601).
    pub created_at: String,

    /// Zstd compression level used (1–22).
    pub compression_level: i32,

    /// Total uncompressed size of all included files, in bytes.
    pub original_bytes: u64,

    /// Size of the compressed archive on disk, in bytes.
    pub compressed_bytes: u64,

    /// One entry per file included in the archive.
    pub files: Vec<ManifestEntry>,
}

/// A single file entry in the backup manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Absolute path of the file on the source system.
    pub path: String,

    /// File size in bytes.
    pub size_bytes: u64,

    /// BLAKE3 content hash at the time of backup (None if not yet hashed).
    pub content_hash: Option<String>,
}

// =============================================================================
// Report
// =============================================================================

/// Result of a backup run, returned to `main.rs`.
#[derive(Debug, Default)]
pub struct BackupReport {
    /// Path to the created archive file.
    pub archive_path: PathBuf,

    /// Path to the JSON manifest file.
    pub manifest_path: PathBuf,

    /// Number of files included in the backup.
    pub files_included: usize,

    /// Total uncompressed size.
    pub original_bytes: u64,

    /// Compressed archive size.
    pub compressed_bytes: u64,

    /// How long the backup took, in milliseconds.
    pub duration_ms: u64,

    /// Files that could not be read and were skipped.
    pub skipped: Vec<String>,
}

// =============================================================================
// Public entry point — create
// =============================================================================

/// Create a new versioned backup archive and return a report.
///
/// Called from `main.rs` for the `backup create` subcommand.
pub fn create(conn: &Connection, config: &Config, args: &BackupCreateArgs) -> Result<BackupReport> {
    let start = Instant::now();

    // Generate a unique ID for this backup — used in filenames to avoid conflicts.
    let backup_id = Uuid::new_v4().to_string();
    let short_id  = &backup_id[..8]; // First 8 chars for the filename

    // Build the label: use --label if provided, otherwise "backup-YYYY-MM-DD".
    let label = args.label.clone().unwrap_or_else(|| {
        format!("backup-{}", Utc::now().format("%Y-%m-%d"))
    });

    // Construct archive and manifest paths.
    let archive_name  = format!("{}-{}.tar.zst", label, short_id);
    let manifest_name = format!("{}-{}.json",    label, short_id);
    let archive_path  = config.backup_dir.join(&archive_name);
    let manifest_path = config.backup_dir.join(&manifest_name);

    // Ensure the backup directory exists.
    fs::create_dir_all(&config.backup_dir)
        .with_context(|| format!("could not create backup dir: {}", config.backup_dir.display()))?;

    // Load files to back up from the database.
    let files = select_files(conn, args)?;

    if files.is_empty() {
        anyhow::bail!("No files to back up. Run `scan` first, or check your --category filter.");
    }

    if args.dry_run {
        return dry_run_report(&files, &archive_path, &manifest_path);
    }

    info!(
        "Starting backup '{}': {} files → {}",
        label,
        files.len(),
        archive_path.display()
    );

    // Write the compressed archive.
    let (original_bytes, compressed_bytes, skipped) =
        write_archive(&archive_path, &files, args.compression_level)?;

    // Build and write the JSON manifest.
    let manifest = BackupManifest {
        id:                backup_id.clone(),
        label:             label.clone(),
        created_at:        Utc::now().to_rfc3339(),
        compression_level: args.compression_level,
        original_bytes,
        compressed_bytes,
        files: files.iter().map(|f| ManifestEntry {
            path:         f.path.clone(),
            size_bytes:   f.size_bytes,
            content_hash: f.content_hash.clone(),
        }).collect(),
    };

    write_manifest(&manifest_path, &manifest)?;

    // Record the backup in the SQLite database.
    let record = BackupRecord {
        id:               None,
        label:            label.clone(),
        archive_path:     archive_path.to_string_lossy().to_string(),
        original_bytes,
        compressed_bytes,
        algorithm:        "zstd".to_string(),
        created_at:       Utc::now(),
    };
    db::insert_backup(conn, &record)
        .context("could not record backup in database")?;

    let duration_ms = start.elapsed().as_millis() as u64;

    info!(
        "Backup complete: {} files, {} → {} ({:.1}% ratio) in {}ms",
        files.len() - skipped.len(),
        bytesize::ByteSize(original_bytes),
        bytesize::ByteSize(compressed_bytes),
        compression_ratio(original_bytes, compressed_bytes),
        duration_ms,
    );

    Ok(BackupReport {
        archive_path,
        manifest_path,
        files_included: files.len() - skipped.len(),
        original_bytes,
        compressed_bytes,
        duration_ms,
        skipped,
    })
}

// =============================================================================
// Public entry point — list
// =============================================================================

/// Return all backup records from the database, newest first.
pub fn list(conn: &Connection) -> Result<Vec<BackupRecord>> {
    db::get_all_backups(conn).context("could not fetch backup list")
}

/// Print the backup list to stdout.
pub fn print_list(backups: &[BackupRecord]) {
    if backups.is_empty() {
        println!("No backups found. Run `data-fortress backup create` to create one.");
        return;
    }

    println!("\n{} backup(s):\n", backups.len());

    for b in backups {
        println!(
            "  [{}] {}",
            b.created_at.format("%Y-%m-%d %H:%M"),
            b.label,
        );
        println!(
            "       {} → {} ({:.1}% ratio)",
            bytesize::ByteSize(b.original_bytes),
            bytesize::ByteSize(b.compressed_bytes),
            compression_ratio(b.original_bytes, b.compressed_bytes),
        );
        println!("       {}", b.archive_path);
        println!();
    }
}

// =============================================================================
// Archive writing
// =============================================================================

/// Write a `.tar.zst` archive containing all the given files.
///
/// Returns `(original_bytes, compressed_bytes, skipped_paths)`.
///
/// The archive is written as:
///   raw file bytes
///     → tar builder (adds headers, paths, padding)
///       → zstd encoder (compresses the stream)
///         → output file on disk
///
/// Each layer is a `Write` implementor wrapping the next — the data flows
/// through without needing intermediate buffers for the whole content.
fn write_archive(
    archive_path: &Path,
    files: &[FileRecord],
    compression_level: i32,
) -> Result<(u64, u64, Vec<String>)> {
    // Open the output file for writing.
    let out_file = File::create(archive_path)
        .with_context(|| format!("could not create archive at {}", archive_path.display()))?;

    // BufWriter reduces the number of write syscalls to the OS by buffering
    // data in userspace and flushing in larger chunks.
    let buf_writer = BufWriter::new(out_file);

    // Wrap the buffered writer in a zstd encoder. All bytes written to
    // `tar_builder` will be compressed and written to `buf_writer`.
    let zstd_encoder = zstd::Encoder::new(buf_writer, compression_level)
        .context("could not create zstd encoder")?;

    // Wrap the zstd encoder in a TAR builder. The builder adds TAR headers
    // and padding around each file's content.
    let mut tar_builder = tar::Builder::new(zstd_encoder);

    let progress = ProgressBar::new(files.len() as u64);
    progress.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.cyan} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} files")
            .unwrap()
            .progress_chars("=>-"),
    );

    let mut original_bytes = 0u64;
    let mut skipped: Vec<String> = Vec::new();

    for file in files {
        let path = Path::new(&file.path);

        // Try to open the source file for reading.
        match File::open(path) {
            Ok(mut src_file) => {
                // `append_file` reads from `src_file` and writes it into the
                // TAR archive with the given archive path (relative path inside
                // the archive). We strip the leading `/` to make paths relative.
                let archive_entry_path = file.path.trim_start_matches('/');

                if let Err(e) = tar_builder.append_file(archive_entry_path, &mut src_file) {
                    warn!("Could not add {} to archive: {}", file.path, e);
                    skipped.push(file.path.clone());
                } else {
                    original_bytes += file.size_bytes;
                }
            }
            Err(e) => {
                warn!("Could not open {} for backup: {}", file.path, e);
                skipped.push(file.path.clone());
            }
        }

        progress.inc(1);
    }

    progress.finish_with_message("Archive written.");

    // `finish()` flushes the TAR end-of-archive marker and returns the
    // underlying zstd encoder so we can finalize it.
    let zstd_encoder = tar_builder
        .into_inner()
        .context("could not finalize TAR stream")?;

    // `finish()` on the zstd encoder flushes all buffered compressed data
    // and writes the zstd frame footer. Without this call, the archive would
    // be corrupt — zstd requires the footer to verify the frame.
    let buf_writer = zstd_encoder
        .finish()
        .context("could not finalize zstd stream")?;

    // Flush and drop the BufWriter to ensure all data reaches the OS.
    buf_writer
        .into_inner()
        .map_err(|e| anyhow::anyhow!("could not flush archive buffer: {}", e))?
        .sync_all()
        .context("could not sync archive to disk")?;

    // Read the final compressed size from the archive file on disk.
    let compressed_bytes = fs::metadata(archive_path)
        .map(|m| m.len())
        .unwrap_or(0);

    Ok((original_bytes, compressed_bytes, skipped))
}

// =============================================================================
// Manifest writing
// =============================================================================

/// Write the JSON manifest to disk.
fn write_manifest(path: &Path, manifest: &BackupManifest) -> Result<()> {
    let json = serde_json::to_string_pretty(manifest)
        .context("could not serialize backup manifest")?;

    fs::write(path, json)
        .with_context(|| format!("could not write manifest at {}", path.display()))?;

    info!("Manifest written: {}", path.display());
    Ok(())
}

// =============================================================================
// File selection
// =============================================================================

/// Select which files to include in the backup from the database.
///
/// Applies the optional category filter from `BackupCreateArgs`.
fn select_files(conn: &Connection, args: &BackupCreateArgs) -> Result<Vec<FileRecord>> {
    match &args.category {
        // No category filter — back up everything.
        None => db::get_all_present_files(conn)
            .context("could not load files for backup"),

        // Category filter — fetch only files in the specified category.
        Some(cat) => {
            let fc = search_category_to_file_category(cat);
            db::search_files_by_category(conn, &fc)
                .context("could not load files by category for backup")
        }
    }
}

/// Convert CLI `SearchCategory` to model `FileCategory`.
fn search_category_to_file_category(cat: &SearchCategory) -> FileCategory {
    match cat {
        SearchCategory::Image    => FileCategory::Image,
        SearchCategory::Video    => FileCategory::Video,
        SearchCategory::Audio    => FileCategory::Audio,
        SearchCategory::Document => FileCategory::Document,
        SearchCategory::Archive  => FileCategory::Archive,
        SearchCategory::Code     => FileCategory::Code,
        SearchCategory::Other    => FileCategory::Other,
    }
}

// =============================================================================
// Dry run
// =============================================================================

/// Produce a report without creating any files, for `--dry-run` mode.
fn dry_run_report(
    files: &[FileRecord],
    archive_path: &Path,
    manifest_path: &Path,
) -> Result<BackupReport> {
    let original_bytes: u64 = files.iter().map(|f| f.size_bytes).sum();

    println!("[dry-run] Would back up {} file(s):", files.len());
    println!("[dry-run] Archive:  {}", archive_path.display());
    println!("[dry-run] Manifest: {}", manifest_path.display());
    println!(
        "[dry-run] Total uncompressed size: {}",
        bytesize::ByteSize(original_bytes)
    );

    Ok(BackupReport {
        archive_path:   archive_path.to_path_buf(),
        manifest_path:  manifest_path.to_path_buf(),
        files_included: files.len(),
        original_bytes,
        compressed_bytes: 0, // Unknown without actually compressing
        duration_ms: 0,
        skipped: Vec::new(),
    })
}

// =============================================================================
// Helpers
// =============================================================================

/// Compute the compression ratio as a percentage saved.
///
/// Returns 0.0 if original_bytes is zero (avoids division by zero).
/// A ratio of 60.0 means the archive is 40% of the original size (60% saved).
fn compression_ratio(original: u64, compressed: u64) -> f64 {
    if original == 0 {
        return 0.0;
    }
    // Percentage saved: 100 * (1 - compressed/original)
    let ratio = 100.0 * (1.0 - (compressed as f64 / original as f64));
    // Clamp to [0, 100] — ratio can be slightly negative if headers add overhead.
    ratio.clamp(0.0, 100.0)
}

/// Print a backup report to stdout.
pub fn print_report(report: &BackupReport, dry_run: bool) {
    if dry_run {
        return; // dry_run_report already printed its own output
    }

    println!("\nBackup complete:");
    println!("  Archive:   {}", report.archive_path.display());
    println!("  Manifest:  {}", report.manifest_path.display());
    println!("  Files:     {}", report.files_included);
    println!(
        "  Size:      {} → {} ({:.1}% saved)",
        bytesize::ByteSize(report.original_bytes),
        bytesize::ByteSize(report.compressed_bytes),
        compression_ratio(report.original_bytes, report.compressed_bytes),
    );
    println!("  Duration:  {}ms", report.duration_ms);

    if !report.skipped.is_empty() {
        println!("\n  Skipped {} file(s) due to read errors:", report.skipped.len());
        for path in &report.skipped {
            println!("    ✗ {}", path);
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_compression_ratio_zero_original() {
        // Dividing by zero should return 0.0, not panic.
        assert_eq!(compression_ratio(0, 0), 0.0);
    }

    #[test]
    fn test_compression_ratio_no_compression() {
        // If compressed == original, 0% saved.
        assert_eq!(compression_ratio(1000, 1000), 0.0);
    }

    #[test]
    fn test_compression_ratio_half_size() {
        // If compressed is half of original, 50% saved.
        let ratio = compression_ratio(1000, 500);
        assert!((ratio - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_compression_ratio_clamped() {
        // Compressed > original (headers added overhead) → clamp to 0.
        assert_eq!(compression_ratio(100, 200), 0.0);
    }

    #[test]
    fn test_write_and_verify_archive() {
        let dir = TempDir::new().unwrap();

        // Create two source files with known content.
        let file1 = dir.path().join("hello.txt");
        let file2 = dir.path().join("world.txt");
        fs::write(&file1, b"Hello, backup world!").unwrap();
        fs::write(&file2, b"Second file content.").unwrap();

        // Build minimal FileRecords for these files.
        let records = vec![
            make_record(file1.to_str().unwrap(), 20),
            make_record(file2.to_str().unwrap(), 20),
        ];

        // Write a real archive.
        let archive_path = dir.path().join("test.tar.zst");
        let (original, compressed, skipped) = write_archive(&archive_path, &records, 3).unwrap();

        assert!(archive_path.exists(), "archive file must be created");
        assert_eq!(skipped.len(), 0, "no files should be skipped");
        assert_eq!(original, 40, "original bytes = 20 + 20");
        assert!(compressed > 0,  "compressed size must be non-zero");
    }

    #[test]
    fn test_write_manifest() {
        let dir = TempDir::new().unwrap();
        let manifest_path = dir.path().join("manifest.json");

        let manifest = BackupManifest {
            id:                "test-id".to_string(),
            label:             "test-backup".to_string(),
            created_at:        "2025-04-08T00:00:00Z".to_string(),
            compression_level: 3,
            original_bytes:    1024,
            compressed_bytes:  512,
            files: vec![ManifestEntry {
                path:         "/home/tom/file.txt".to_string(),
                size_bytes:   1024,
                content_hash: Some("abc123".to_string()),
            }],
        };

        write_manifest(&manifest_path, &manifest).unwrap();

        // Read back and verify it's valid JSON with expected fields.
        let contents = fs::read_to_string(&manifest_path).unwrap();
        let parsed: BackupManifest = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed.label, "test-backup");
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.original_bytes, 1024);
    }

    /// Build a minimal FileRecord pointing at a real file on disk.
    fn make_record(path: &str, size: u64) -> FileRecord {
        use crate::models::FileCategory;
        FileRecord {
            id:           None,
            path:         path.to_string(),
            name:         Path::new(path).file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
            extension:    "txt".to_string(),
            category:     FileCategory::Document,
            mime_type:    "text/plain".to_string(),
            size_bytes:   size,
            content_hash: None,
            modified_at:  Utc::now(),
            scanned_at:   Utc::now(),
            is_present:   true,
        }
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY TAR + zstd instead of ZIP?
//    ZIP compresses each file independently; TAR compresses the whole stream.
//    TAR allows zstd to find repetition across files (e.g. two similar photos),
//    achieving better ratios on collections of similar content. ZIP's per-file
//    compression can't exploit cross-file redundancy. TAR archives are also
//    trivially streamable — you don't need to seek to a central directory.
//
// 2. WHY a streaming pipeline (File → BufWriter → zstd → TAR)?
//    Each layer wraps the previous as a Write implementor. Data flows through
//    without needing to buffer the entire archive in memory. A backup of 1 TB
//    of files uses only a few MB of RAM for the pipeline buffers.
//
// 3. WHY call zstd_encoder.finish()?
//    zstd frames have a footer that includes a checksum and size information.
//    If we just drop the encoder, Rust calls Drop which flushes but does NOT
//    write the footer. `finish()` explicitly closes the frame. An archive
//    without its footer is considered corrupt by all zstd decompressors.
//
// 4. WHY a separate JSON manifest instead of just the archive?
//    The archive is opaque until decompressed. The manifest lets the dashboard
//    show what's in each backup without reading the (potentially large) archive.
//    It also enables targeted restore: find the file in the manifest, extract
//    only that entry from the TAR archive.
//
// 5. WHY UUID in the archive filename?
//    If the user runs two backups on the same date with the same label, we want
//    unique filenames so the second doesn't silently overwrite the first. The
//    first 8 characters of a UUID provide enough uniqueness (1 in 4 billion
//    chance of collision) without making the filename unwieldy.
//
// 6. WHY strip the leading `/` from paths inside the archive?
//    TAR archives with absolute paths (starting with `/`) are considered
//    dangerous — extracting them could overwrite system files. Relative paths
//    inside the archive are the safe convention. We strip the leading `/`
//    so `tar -xf backup.tar.zst` extracts into the current directory, not
//    into `/` on the filesystem.

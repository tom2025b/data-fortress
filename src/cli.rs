//! # cli.rs
//!
//! Command-line interface definitions for Data Fortress.
//!
//! All Clap structs and enums live here. `main.rs` imports `Cli` and matches
//! on `Cli::parse().command` to dispatch to the correct module.
//!
//! ## Subcommand layout
//!
//! ```text
//! data-fortress
//! ├── scan      — walk directories and record files into the database
//! ├── dedup     — find and report duplicate files by content hash
//! ├── organize  — move files into a clean directory structure
//! ├── search    — query files by name, content, or metadata
//! ├── backup    — create and list versioned compressed backups
//! └── config    — manage configuration (show, set, add-dir, remove-dir)
//! ```

use std::path::PathBuf;

// `Parser` is the trait that provides `.parse()` on our Cli struct.
// `Subcommand` marks an enum whose variants become subcommands.
// `Args` marks a struct whose fields become flags/arguments for a subcommand.
// `ValueEnum` marks an enum whose variants become valid flag values.
use clap::{Args, Parser, Subcommand, ValueEnum};

// =============================================================================
// Root CLI struct
// =============================================================================

/// Data Fortress — personal file management system.
///
/// The text in this doc comment becomes the top-level --help description.
#[derive(Parser, Debug)]
#[command(
    name    = "data-fortress",
    version,                        // auto-filled from Cargo.toml version field
    about   = "Scan, deduplicate, organize, search, and backup your files.",
    long_about = None,
    propagate_version = true,       // show version in all subcommand --help outputs
)]
pub struct Cli {
    /// Path to the config file.
    ///
    /// Defaults to ~/.config/data-fortress/config.json if not provided.
    #[arg(
        short = 'c',
        long  = "config",
        global = true,              // available on every subcommand
        env   = "FORTRESS_CONFIG",  // can also be set via environment variable
    )]
    pub config_path: Option<PathBuf>,

    /// Increase verbosity. Pass multiple times for more detail (-v, -vv, -vvv).
    ///
    /// Controls the tracing log level:
    ///   -v   = INFO  (progress updates)
    ///   -vv  = DEBUG (per-file details)
    ///   -vvv = TRACE (everything)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    pub verbosity: u8,

    /// Output results as JSON instead of human-readable text.
    ///
    /// The Streamlit dashboard uses this flag when shelling out to the binary.
    #[arg(long = "json", global = true)]
    pub json: bool,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Commands,
}

// =============================================================================
// Top-level subcommands
// =============================================================================

/// All available subcommands.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Scan directories and record file metadata into the database.
    Scan(ScanArgs),

    /// Find duplicate files by content hash (BLAKE3).
    Dedup(DedupArgs),

    /// Automatically organize files into a clean directory structure.
    Organize(OrganizeArgs),

    /// Search for files by name, content, or metadata.
    Search(SearchArgs),

    /// Create and manage versioned compressed backups.
    Backup(BackupArgs),

    /// Show and modify Data Fortress configuration.
    Config(ConfigArgs),
}

// =============================================================================
// scan
// =============================================================================

/// Arguments for the `scan` subcommand.
///
/// Example usage:
///   data-fortress scan /home/tom/Photos /mnt/drive2
///   data-fortress scan --hash --json /mnt/data
#[derive(Args, Debug)]
pub struct ScanArgs {
    /// One or more directories to scan.
    ///
    /// If omitted, scans all directories listed in the config file.
    /// At least one directory must be provided here or in the config.
    #[arg(value_name = "DIR")]
    pub directories: Vec<PathBuf>,

    /// Also compute BLAKE3 content hashes during this scan.
    ///
    /// Hashing is the slow part of a full scan. Without this flag, the scanner
    /// records metadata only (fast). Hashes are needed for deduplication.
    /// You can run `data-fortress dedup --hash` later to hash separately.
    #[arg(short = 'H', long = "hash")]
    pub hash: bool,

    /// Skip files larger than this many bytes during hashing.
    ///
    /// Defaults to the value in config (usually 4 GiB).
    /// Only relevant when --hash is also passed.
    #[arg(long = "max-hash-size", value_name = "BYTES")]
    pub max_hash_size: Option<u64>,

    /// Perform a dry run: print what would be scanned without writing to the DB.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
}

// =============================================================================
// dedup
// =============================================================================

/// Arguments for the `dedup` subcommand.
///
/// Example usage:
///   data-fortress dedup
///   data-fortress dedup --hash --min-size 1048576
///   data-fortress dedup --delete --keep newest
#[derive(Args, Debug)]
pub struct DedupArgs {
    /// Hash any files that don't have a content hash yet before deduplicating.
    ///
    /// Without this flag, only files already hashed by a previous scan are
    /// compared. Use this flag for a fresh deduplication pass on new files.
    #[arg(short = 'H', long = "hash")]
    pub hash: bool,

    /// Only report duplicates where each copy is larger than this size in bytes.
    ///
    /// Useful for ignoring tiny duplicate config or lock files and focusing on
    /// large space-wasting duplicates (photos, videos, archives).
    /// Default: 0 (report all duplicates regardless of size).
    #[arg(long = "min-size", value_name = "BYTES", default_value = "0")]
    pub min_size: u64,

    /// Delete duplicate files, keeping one copy according to the --keep strategy.
    ///
    /// WARNING: this permanently deletes files. Always review the report first.
    /// Combine with --dry-run to preview what would be deleted.
    #[arg(long = "delete")]
    pub delete: bool,

    /// Which copy to keep when --delete is used.
    #[arg(long = "keep", value_enum, default_value = "oldest")]
    pub keep: KeepStrategy,

    /// Preview what would be deleted without actually deleting anything.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
}

/// Which file to keep when deleting duplicates.
#[derive(ValueEnum, Debug, Clone)]
pub enum KeepStrategy {
    /// Keep the copy with the oldest modification timestamp (most "original").
    Oldest,
    /// Keep the copy with the newest modification timestamp.
    Newest,
    /// Keep the copy whose path comes first alphabetically.
    FirstAlpha,
    /// Keep the copy with the shortest path (usually closer to root = more organized).
    ShortestPath,
}

// =============================================================================
// organize
// =============================================================================

/// Arguments for the `organize` subcommand.
///
/// Example usage:
///   data-fortress organize /mnt/drive2/chaos --dest /mnt/drive2/sorted
///   data-fortress organize --mode by-date --dry-run /home/tom/Downloads
#[derive(Args, Debug)]
pub struct OrganizeArgs {
    /// The source directory to organize.
    #[arg(value_name = "SOURCE")]
    pub source: PathBuf,

    /// The destination root directory where organized files will be placed.
    ///
    /// Files are moved into subdirectories under this root according to --mode.
    /// Example with --mode by-date: <dest>/Photos/2024/03/IMG_001.jpg
    #[arg(short = 'd', long = "dest", value_name = "DEST")]
    pub dest: PathBuf,

    /// How to organize the files.
    #[arg(short = 'm', long = "mode", value_enum, default_value = "by-type-and-date")]
    pub mode: OrganizeMode,

    /// Preview moves without actually moving any files.
    ///
    /// Prints each planned move as: "SOURCE  →  DEST". Use this to verify the
    /// organization plan before committing to it.
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Overwrite files at the destination if they already exist.
    ///
    /// Without this flag, the organizer skips files where the destination
    /// already exists and reports them as conflicts.
    #[arg(long = "overwrite")]
    pub overwrite: bool,
}

/// Organization strategy controlling how files are sorted into subdirectories.
#[derive(ValueEnum, Debug, Clone)]
pub enum OrganizeMode {
    /// Sort by broad type first, then by year/month.
    /// Structure: <dest>/<Category>/<Year>/<Month>/<file>
    ByTypeAndDate,

    /// Sort by year/month only, regardless of file type.
    /// Structure: <dest>/<Year>/<Month>/<file>
    ByDate,

    /// Sort by file category only (no date subdirectory).
    /// Structure: <dest>/<Category>/<file>
    ByType,
}

// =============================================================================
// search
// =============================================================================

/// Arguments for the `search` subcommand.
///
/// Example usage:
///   data-fortress search "vacation photos 2023"
///   data-fortress search --category image "beach"
///   data-fortress search --content "quarterly report" --limit 20
#[derive(Args, Debug)]
pub struct SearchArgs {
    /// The search query string.
    ///
    /// Matched against file names, paths, and (with --content) extracted text.
    #[arg(value_name = "QUERY")]
    pub query: String,

    /// Restrict results to files of this category.
    #[arg(short = 'C', long = "category", value_enum)]
    pub category: Option<SearchCategory>,

    /// Also search inside file content (PDFs, documents, spreadsheets).
    ///
    /// Slower than name-only search because it reads and parses file content.
    #[arg(long = "content")]
    pub content: bool,

    /// Maximum number of results to return.
    #[arg(short = 'n', long = "limit", default_value = "50")]
    pub limit: usize,

    /// Sort results by this field.
    #[arg(long = "sort", value_enum, default_value = "relevance")]
    pub sort: SearchSort,
}

/// File category filter for search.
///
/// Mirrors `FileCategory` but implemented as a separate enum here so Clap
/// can derive `ValueEnum` for it without touching the models module.
#[derive(ValueEnum, Debug, Clone)]
pub enum SearchCategory {
    Image,
    Video,
    Audio,
    Document,
    Archive,
    Code,
    Other,
}

/// How to sort search results.
#[derive(ValueEnum, Debug, Clone)]
pub enum SearchSort {
    /// Most relevant results first (default).
    Relevance,
    /// Most recently modified files first.
    Newest,
    /// Largest files first.
    Largest,
    /// Alphabetical by file name.
    Name,
}

// =============================================================================
// backup
// =============================================================================

/// Arguments for the `backup` subcommand.
///
/// This is a subcommand group — it has its own sub-subcommands: create / list.
///
/// Example usage:
///   data-fortress backup create --label "weekly"
///   data-fortress backup list
#[derive(Args, Debug)]
pub struct BackupArgs {
    #[command(subcommand)]
    pub action: BackupAction,
}

/// Actions available under the `backup` subcommand.
#[derive(Subcommand, Debug)]
pub enum BackupAction {
    /// Create a new versioned backup archive.
    Create(BackupCreateArgs),

    /// List all recorded backup archives.
    List,
}

/// Arguments for `backup create`.
#[derive(Args, Debug)]
pub struct BackupCreateArgs {
    /// Human-readable label for this backup.
    ///
    /// Defaults to the current date ("backup-YYYY-MM-DD") if not provided.
    #[arg(short = 'l', long = "label", value_name = "LABEL")]
    pub label: Option<String>,

    /// Only back up files in this category.
    ///
    /// If omitted, all present files in the database are included.
    #[arg(short = 'C', long = "category", value_enum)]
    pub category: Option<SearchCategory>, // reuse SearchCategory for DRY

    /// zstd compression level (1 = fastest, 22 = best ratio).
    ///
    /// Level 3 is the default — a good balance of speed and size.
    #[arg(long = "compression", default_value = "3", value_parser = clap::value_parser!(i32).range(1..=22))]
    pub compression_level: i32,

    /// Preview what would be backed up without writing the archive.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
}

// =============================================================================
// config
// =============================================================================

/// Arguments for the `config` subcommand group.
///
/// Example usage:
///   data-fortress config show
///   data-fortress config add-dir /mnt/drive2
///   data-fortress config remove-dir /mnt/drive2
///   data-fortress config set threads 4
#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: ConfigAction,
}

/// Actions available under the `config` subcommand.
#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print the current configuration as JSON.
    Show,

    /// Add a directory to the watch list.
    AddDir {
        /// The directory path to add.
        #[arg(value_name = "DIR")]
        path: PathBuf,
    },

    /// Remove a directory from the watch list.
    RemoveDir {
        /// The directory path to remove.
        #[arg(value_name = "DIR")]
        path: PathBuf,
    },

    /// Set a configuration value by key.
    ///
    /// Example: data-fortress config set threads 4
    Set {
        /// The config key to update (e.g. "threads", "max_hash_size_bytes").
        #[arg(value_name = "KEY")]
        key: String,

        /// The new value for the key.
        #[arg(value_name = "VALUE")]
        value: String,
    },
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY derive-based Clap instead of the builder API?
//    Clap supports two styles: derive (structs + enums) and builder (method
//    chains). Derive keeps command definitions as plain data — easy to read,
//    easy to extend, and the compiler checks types for you. Builder style is
//    more flexible but verbose. For a project this size, derive is the right
//    choice.
//
// 2. WHY `#[arg(global = true)]` on --config, --verbose, --json?
//    Without `global = true`, flags defined on the root `Cli` struct are only
//    available before the subcommand. With it, users can write either:
//      data-fortress --json scan /mnt/data
//      data-fortress scan /mnt/data --json
//    Both work because Clap propagates the flag to all subcommands.
//
// 3. WHY `#[arg(env = "FORTRESS_CONFIG")]`?
//    The `env` attribute makes a flag also readable from an environment
//    variable. This is a best practice for CLI tools: users can set
//    FORTRESS_CONFIG once in their shell profile rather than typing
//    --config /path/to/config on every invocation.
//
// 4. WHY a separate `SearchCategory` enum instead of reusing `FileCategory`?
//    `FileCategory` (in models.rs) derives serde for JSON. `ValueEnum` (for
//    Clap) is a different trait. We could add both derives to `FileCategory`,
//    but that would couple the data model to the CLI layer — a design smell.
//    A separate enum that mirrors the values keeps the layers independent.
//
// 5. WHY nested subcommands (backup create / backup list)?
//    Some commands naturally have sub-actions. Grouping them under `backup`
//    keeps the top-level command list short and groups related functionality.
//    Clap supports arbitrary nesting via `#[command(subcommand)]` on a field
//    inside an `Args` struct.
//
// 6. WHY `ArgAction::Count` for --verbose?
//    `Count` makes the flag additive: `-v` sets verbosity to 1, `-vv` to 2,
//    `-vvv` to 3. We map these counts to tracing log levels in main.rs.
//    This is the standard Unix convention for verbosity flags.

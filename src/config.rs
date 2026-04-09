//! # config.rs
//!
//! Runtime configuration for Data Fortress.
//!
//! The `Config` struct holds all user-tunable settings: which directories to
//! scan, where to store the database, what to exclude, and how many threads
//! to use. It serializes to/from JSON so users can persist their settings in
//! `~/.config/data-fortress/config.json`.
//!
//! At startup, `main.rs` calls `Config::load()` which either reads the saved
//! config or falls back to `Config::default_config()` for first-run defaults.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// =============================================================================
// Config
// =============================================================================

/// All user-tunable runtime settings for Data Fortress.
///
/// Serialized to JSON and stored at `~/.config/data-fortress/config.json`.
/// Every field has a sensible default so first-time users need zero setup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Absolute path to the SQLite database file.
    ///
    /// PathBuf is the owned, heap-allocated path type in Rust — think of it
    /// as String for file paths. We use PathBuf (not String) so the standard
    /// library's file APIs accept it directly without conversions.
    ///
    /// Default: `~/.local/share/data-fortress/fortress.db`
    pub db_path: PathBuf,

    /// Root directories that Data Fortress will scan for files.
    ///
    /// Users add directories with `data-fortress config add-dir /mnt/drive2`.
    /// The scanner walks each entry recursively.
    pub watch_dirs: Vec<PathBuf>,

    /// Directory where versioned backup archives are stored.
    ///
    /// Default: `~/.local/share/data-fortress/backups/`
    pub backup_dir: PathBuf,

    /// Directory names to skip entirely during scanning.
    ///
    /// Matching is done against the directory name only (not the full path),
    /// so ".git" skips every `.git/` folder anywhere in the tree.
    ///
    /// Default list covers common noise directories on Linux.
    pub exclude_dirs: Vec<String>,

    /// File extensions to skip, lowercase without a leading dot.
    ///
    /// For example, "tmp" skips all `.tmp` files. The scanner compares the
    /// lowercase extension of each file against this list.
    pub exclude_extensions: Vec<String>,

    /// Files larger than this many bytes are skipped during the content-hash
    /// pass and flagged as "not hashed" in the database.
    ///
    /// Hashing a 100 GB video file takes minutes; this limit lets users choose
    /// to skip very large files and hash them manually later.
    ///
    /// Default: 4 GiB (4 * 1024 * 1024 * 1024)
    pub max_hash_size_bytes: u64,

    /// Number of rayon worker threads to use for parallel operations.
    ///
    /// `0` means "use all available logical CPU cores" — rayon detects this
    /// automatically. Set to a lower number to leave cores free for other work
    /// while a scan runs in the background.
    ///
    /// Default: 0 (all cores)
    pub threads: usize,
}

// =============================================================================
// impl Config
// =============================================================================

impl Config {
    /// Build a `Config` with sensible defaults for a first-time Linux user.
    ///
    /// Uses the `dirs` crate to locate the correct XDG base directories:
    /// - Data:   `~/.local/share/`  (database, backups)
    /// - Config: `~/.config/`       (config.json)
    ///
    /// Falls back to `/tmp/data-fortress/` if XDG dirs are unavailable
    /// (e.g. running as a system service with no home directory).
    pub fn default_config() -> Self {
        // `dirs::data_dir()` returns `~/.local/share` on Linux.
        // We append our app name to get our private data directory.
        let data_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("data-fortress");

        Config {
            // Database lives inside our data directory.
            db_path: data_dir.join("fortress.db"),

            // No directories watched by default — user adds them explicitly.
            watch_dirs: Vec::new(),

            // Backups go into a subdirectory of our data directory.
            backup_dir: data_dir.join("backups"),

            // Common directories that are never useful to scan.
            exclude_dirs: vec![
                ".git".into(),
                ".svn".into(),
                ".hg".into(),
                "node_modules".into(),
                ".cache".into(),
                "target".into(),          // Rust build output
                "__pycache__".into(),     // Python bytecode
                ".Trash".into(),
                ".local".into(),
                "lost+found".into(),      // Linux filesystem recovery directory
            ],

            // File extensions that are temporary, lock, or editor-swap files.
            exclude_extensions: vec![
                "tmp".into(),
                "temp".into(),
                "lock".into(),
                "swp".into(),   // Vim swap files
                "swo".into(),   // Vim swap files (overflow)
                "bak".into(),
            ],

            // 4 GiB: files larger than this skip the hash pass.
            // 4 * 1024^3 computed explicitly so the intent is clear.
            max_hash_size_bytes: 4 * 1024 * 1024 * 1024,

            // 0 = let rayon decide (uses all logical CPU cores).
            threads: 0,
        }
    }

    /// Returns the default path for the config file itself.
    ///
    /// `dirs::config_dir()` returns `~/.config` on Linux (XDG spec).
    /// The config file lives at `~/.config/data-fortress/config.json`.
    pub fn default_config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("data-fortress")
            .join("config.json")
    }

    /// Load a `Config` from a JSON file at `path`.
    ///
    /// If the file does not exist, returns `Config::default_config()` instead
    /// of erroring — this handles the first-run case gracefully.
    ///
    /// Returns an error only if the file exists but cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self> {
        // Check if the config file exists before trying to open it.
        // `path.exists()` returns false for both "not found" and permission
        // errors; we distinguish them below if needed.
        if !path.exists() {
            // First run: no config file yet. Use defaults silently.
            return Ok(Self::default_config());
        }

        // Read the entire file into a String.
        // `.with_context(...)` attaches a human-readable message to any error,
        // so the user sees "could not read config file at /path" not just "I/O error".
        let contents = fs::read_to_string(path)
            .with_context(|| format!("could not read config file at {}", path.display()))?;

        // Parse the JSON string into a Config struct.
        // serde_json uses the #[derive(Deserialize)] we added to Config.
        let config: Config = serde_json::from_str(&contents)
            .with_context(|| format!("config file at {} is not valid JSON", path.display()))?;

        Ok(config)
    }

    /// Save this `Config` as a JSON file at `path`.
    ///
    /// Creates any missing parent directories automatically (e.g. on first run
    /// `~/.config/data-fortress/` may not exist yet).
    pub fn save(&self, path: &Path) -> Result<()> {
        // `path.parent()` gives us the directory containing the config file.
        // We create it (and any missing parents) before writing.
        if let Some(parent) = path.parent() {
            // `create_dir_all` is like `mkdir -p` — does nothing if the
            // directory already exists, creates all missing parents if not.
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create config directory at {}", parent.display()))?;
        }

        // Serialize the Config to a pretty-printed JSON string.
        // `serde_json::to_string_pretty` adds indentation so the file is
        // human-readable when the user opens it in a text editor.
        let json = serde_json::to_string_pretty(self)
            .context("could not serialize config to JSON")?;

        // Write the JSON string to the file, overwriting any previous version.
        fs::write(path, json)
            .with_context(|| format!("could not write config file at {}", path.display()))?;

        Ok(())
    }

    /// Ensure the directories referenced in this config exist on disk.
    ///
    /// Call this once at startup, before opening the database or starting a
    /// scan. If a required directory is missing, we create it rather than
    /// failing later with a confusing "no such file or directory" error.
    pub fn ensure_dirs(&self) -> Result<()> {
        // Create the directory that will hold the SQLite database file.
        // The database file itself is created by rusqlite on first open;
        // we only need to ensure the parent directory exists.
        if let Some(db_dir) = self.db_path.parent() {
            fs::create_dir_all(db_dir)
                .with_context(|| format!("could not create database directory at {}", db_dir.display()))?;
        }

        // Create the backup output directory.
        fs::create_dir_all(&self.backup_dir)
            .with_context(|| format!("could not create backup directory at {}", self.backup_dir.display()))?;

        Ok(())
    }

    /// Returns `true` if the given directory name should be skipped during scanning.
    ///
    /// Compares the bare directory name (not the full path) against the
    /// `exclude_dirs` list. Called by the scanner for every directory entry.
    pub fn should_exclude_dir(&self, dir_name: &str) -> bool {
        // `iter().any(...)` returns true if at least one element matches.
        self.exclude_dirs.iter().any(|excluded| excluded == dir_name)
    }

    /// Returns `true` if the given file extension should be skipped.
    ///
    /// The extension should already be lowercase and without a leading dot,
    /// matching how FileRecord stores it.
    pub fn should_exclude_extension(&self, ext: &str) -> bool {
        self.exclude_extensions.iter().any(|excluded| excluded == ext)
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY PathBuf instead of String for paths?
//    PathBuf is Rust's owned path type. Unlike a raw String, PathBuf knows
//    how to join components (`path.join("subdir")`), get the parent directory
//    (`path.parent()`), and extract the file name (`path.file_name()`). Using
//    PathBuf also makes functions that accept &Path work directly, with no
//    conversion. Always prefer PathBuf/Path over String for file paths.
//
// 2. WHY serde for config?
//    Adding #[derive(Serialize, Deserialize)] means we get JSON read/write for
//    free. No manual parsing, no format mismatches. If we add a new field to
//    Config later, serde handles it gracefully (missing keys get the field's
//    Default value when deserializing older config files).
//
// 3. WHY `.with_context(|| ...)`?
//    anyhow's `.with_context()` wraps an existing error with an extra message.
//    Without it, a user might see "No such file or directory (os error 2)".
//    With it, they see "could not read config file at /home/tom/.config/
//    data-fortress/config.json: No such file or directory". Much more useful.
//    The closure `|| ...` is lazy — the string is only allocated if there's
//    actually an error.
//
// 4. WHY `dirs::data_dir()` instead of hardcoding `~/.local/share`?
//    The `dirs` crate reads the XDG environment variables
//    (XDG_DATA_HOME, XDG_CONFIG_HOME) that users and desktop environments set
//    to redirect standard directories. Hardcoding the path would break for
//    users who have customized their XDG directories.
//
// 5. WHY `ensure_dirs()` at startup?
//    Rust (and Linux) do not create parent directories automatically when you
//    open a file. If ~/.local/share/data-fortress/ doesn't exist and rusqlite
//    tries to create the database there, it will fail. `ensure_dirs()` handles
//    this once, cleanly, before anything else runs.

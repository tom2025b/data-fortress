// tests/test_scanner.rs
// ----------------------
// Integration tests for the `data-fortress scan` subcommand.
//
// These are BLACK-BOX tests: they invoke the compiled binary as a child process
// (using the `assert_cmd` crate) and verify its exit code, stdout, and the
// SQLite database it produces. They do NOT import any internal Rust modules.
//
// Why black-box integration tests instead of more unit tests?
//   Unit tests (in src/) verify individual functions in isolation.
//   Integration tests verify that the whole pipeline — CLI parsing → scanning
//   → SQLite writes — works end-to-end. A bug in the wiring between modules
//   would be invisible to unit tests but caught here.
//
// How the config works:
//   Data Fortress reads its db_path from a JSON config file. Each test writes
//   a minimal config into a temp directory so tests are fully isolated and
//   don't accidentally share the developer's real database.

// ── Dependencies ──────────────────────────────────────────────────────────────
use assert_cmd::Command;
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ── TestEnv: temp directory + isolated config ─────────────────────────────────
// Groups the temporary directory handle and derived paths so tests can't
// accidentally let the TempDir be dropped (and deleted) too early.
struct TestEnv {
    /// The TempDir handle — must stay alive for the test's duration.
    _dir:        TempDir,
    /// Absolute path to the config JSON file the binary will read.
    config_path: PathBuf,
    /// Absolute path to the SQLite database the binary will write.
    db_path:     PathBuf,
    /// Absolute path to the directory we'll scan (may have files written into it).
    scan_dir:    PathBuf,
}

impl TestEnv {
    /// Create a new, empty test environment.
    fn new() -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let db_path     = dir.path().join("test.db");
        let scan_dir    = dir.path().join("scan");
        let config_path = dir.path().join("config.json");
        let backup_dir  = dir.path().join("backups");

        // Create the scan subdirectory so tests can write files into it.
        fs::create_dir_all(&scan_dir).expect("failed to create scan dir");

        // Write a minimal config JSON pointing at our temp paths.
        // serde_json is not available here (integration tests don't import src/),
        // so we build the JSON string manually. The structure must match Config in src/config.rs.
        let config_json = format!(
            r#"{{
  "db_path": {db_path:?},
  "watch_dirs": [],
  "backup_dir": {backup_dir:?},
  "exclude_dirs": [".git", "node_modules", "target"],
  "exclude_extensions": ["tmp", "lock", "swp"],
  "max_hash_size_bytes": 4294967296,
  "threads": 0
}}"#,
            db_path  = db_path.to_str().unwrap(),
            backup_dir = backup_dir.to_str().unwrap(),
        );
        fs::write(&config_path, config_json).expect("failed to write config");

        Self { _dir: dir, config_path, db_path, scan_dir }
    }

    /// Open a read-only connection to the test database.
    fn open_db(&self) -> Connection {
        Connection::open(&self.db_path).expect("failed to open test database")
    }

    /// Return a pre-configured `Command` targeting our test config.
    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("data-fortress").expect("binary not found");
        c.args(["--config", self.config_path.to_str().unwrap()]);
        c
    }

    /// Run `scan` against `scan_dir`.
    fn scan(&self, extra: &[&str]) {
        let mut args = vec!["scan", self.scan_dir.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.cmd().args(&args).assert().success();
    }
}

// ── Helper: write a tiny valid PNG into a directory ───────────────────────────
// The first 8 bytes are the PNG magic signature that the `infer` crate reads.
fn write_png(dir: &Path, name: &str) {
    let magic: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
                          0x00, 0x00, 0x00, 0x0D];
    fs::write(dir.join(name), magic).expect("failed to write PNG fixture");
}

// ── Test 1: basic scan exits zero ─────────────────────────────────────────────
#[test]
fn test_scan_exits_zero() {
    let env = TestEnv::new();
    fs::write(env.scan_dir.join("hello.txt"), "hello world").unwrap();

    env.cmd()
        .args(["scan", env.scan_dir.to_str().unwrap()])
        .assert()
        .success();
}

// ── Test 2: scan indexes the correct number of files ─────────────────────────
#[test]
fn test_scan_indexes_files() {
    let env = TestEnv::new();

    // Write 3 files of different types.
    fs::write(env.scan_dir.join("note.txt"), "sample text").unwrap();
    fs::write(env.scan_dir.join("main.rs"),  "fn main() {}").unwrap();
    write_png(&env.scan_dir, "photo.png");

    env.scan(&[]);

    let conn  = env.open_db();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM files WHERE is_present = 1", [], |r| r.get(0))
        .expect("count query failed");

    assert_eq!(count, 3, "expected 3 indexed files, got {count}");
}

// ── Test 3: scan classifies file categories correctly ─────────────────────────
#[test]
fn test_scan_classifies_categories() {
    let env = TestEnv::new();

    fs::write(env.scan_dir.join("note.txt"), "sample text").unwrap();
    fs::write(env.scan_dir.join("main.rs"),  "fn main() {}").unwrap();
    write_png(&env.scan_dir, "photo.png");

    env.scan(&[]);

    let conn = env.open_db();

    // Closure: look up a file's category by its name.
    let category_of = |name: &str| -> String {
        conn.query_row(
            "SELECT category FROM files WHERE name = ?1",
            [name],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| panic!("file '{name}' not found in database"))
    };

    assert_eq!(category_of("note.txt"), "document");
    assert_eq!(category_of("main.rs"),  "code");
    assert_eq!(category_of("photo.png"), "image");
}

// ── Test 4: scan with --json outputs valid JSON ───────────────────────────────
#[test]
fn test_scan_json_output() {
    let env = TestEnv::new();
    fs::write(env.scan_dir.join("note.txt"), "hello").unwrap();

    let output = env.cmd()
        .args(["--json", "scan", env.scan_dir.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("stdout not UTF-8");
    // `serde_json` is not imported in integration tests, so we do a simple string check.
    assert!(
        stdout.contains("files_found"),
        "JSON output should contain 'files_found'. Got: {stdout}"
    );
    // Valid JSON must start with '{' (we expect an object, not an array or number).
    assert!(
        stdout.trim_start().starts_with('{'),
        "JSON output should start with '{{'. Got: {stdout}"
    );
}

// ── Test 5: scanning a missing directory exits non-zero ───────────────────────
#[test]
fn test_scan_missing_dir_fails() {
    let env = TestEnv::new();

    env.cmd()
        .args(["scan", "/this/path/absolutely/does/not/exist"])
        .assert()
        // `failure()` checks that the exit code is non-zero.
        .failure();
}

// ── Test 6: rescan marks deleted files as absent ─────────────────────────────
// Tests the two-phase scan: mark_all_absent → walk → mark_present.
// Files that disappear between scans get is_present = 0.
#[test]
fn test_scan_marks_deleted_files_absent() {
    let env = TestEnv::new();

    fs::write(env.scan_dir.join("note.txt"), "hello").unwrap();
    fs::write(env.scan_dir.join("keep.txt"), "keep me").unwrap();

    // First scan: index both files.
    env.scan(&[]);

    // Delete one file.
    fs::remove_file(env.scan_dir.join("note.txt")).expect("failed to remove note.txt");

    // Second scan: note.txt is gone.
    env.scan(&[]);

    let conn = env.open_db();

    // note.txt should remain in the DB with is_present = 0.
    let is_present: i64 = conn
        .query_row(
            "SELECT is_present FROM files WHERE name = 'note.txt'",
            [],
            |row| row.get(0),
        )
        .expect("note.txt should still exist in DB with is_present = 0");

    assert_eq!(is_present, 0, "deleted file should have is_present=0");

    // keep.txt should still be present.
    let kept: i64 = conn
        .query_row(
            "SELECT is_present FROM files WHERE name = 'keep.txt'",
            [],
            |row| row.get(0),
        )
        .expect("keep.txt not found");

    assert_eq!(kept, 1, "surviving file should have is_present=1");
}

// ── Test 7: scan --hash populates 64-char BLAKE3 hashes ──────────────────────
#[test]
fn test_scan_with_hash_flag() {
    let env = TestEnv::new();
    fs::write(env.scan_dir.join("note.txt"), "content to hash").unwrap();

    env.scan(&["--hash"]);

    let conn = env.open_db();

    let hash: String = conn
        .query_row(
            "SELECT content_hash FROM files WHERE content_hash IS NOT NULL LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("no hashed file found after --hash scan");

    // BLAKE3 produces a 256-bit digest = 32 bytes = 64 hex characters.
    assert_eq!(
        hash.len(), 64,
        "BLAKE3 hex hash should be 64 chars, got {}: {hash}", hash.len()
    );

    // Must be lowercase hex only.
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "hash should contain only hex characters, got: {hash}"
    );
}

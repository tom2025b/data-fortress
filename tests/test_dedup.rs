// tests/test_dedup.rs
// --------------------
// Integration tests for the `data-fortress dedup` subcommand.
//
// Strategy:
//   1. Create a temp environment with an isolated config + database.
//   2. Write pairs of identical files (same content → same BLAKE3 hash).
//   3. Run `scan --hash` to index and hash everything.
//   4. Run `dedup` (dry-run and real) and verify results.
//
// Black-box tests: we only invoke the binary and inspect the filesystem + DB.

use assert_cmd::Command;
use rusqlite::Connection;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

// ── TestEnv: isolated config + temp directory ─────────────────────────────────
struct TestEnv {
    _dir:        TempDir,
    config_path: PathBuf,
    db_path:     PathBuf,
    scan_dir:    PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let dir        = TempDir::new().expect("failed to create temp dir");
        let db_path    = dir.path().join("test.db");
        let scan_dir   = dir.path().join("scan");
        let config_path = dir.path().join("config.json");
        let backup_dir = dir.path().join("backups");

        fs::create_dir_all(&scan_dir).expect("failed to create scan dir");

        // Write a minimal config that routes the db to our temp path.
        let config_json = format!(
            r#"{{
  "db_path": {db:?},
  "watch_dirs": [],
  "backup_dir": {bak:?},
  "exclude_dirs": [".git", "node_modules", "target"],
  "exclude_extensions": ["tmp", "lock", "swp"],
  "max_hash_size_bytes": 4294967296,
  "threads": 0
}}"#,
            db  = db_path.to_str().unwrap(),
            bak = backup_dir.to_str().unwrap(),
        );
        fs::write(&config_path, config_json).expect("failed to write config");

        Self { _dir: dir, config_path, db_path, scan_dir }
    }

    /// Return a Command pre-loaded with --config pointing at our temp config.
    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("data-fortress").expect("binary not found");
        c.args(["--config", self.config_path.to_str().unwrap()]);
        c
    }

    /// Open the test database.
    fn open_db(&self) -> Connection {
        Connection::open(&self.db_path).expect("failed to open test database")
    }

    /// Run `scan [--hash]` against scan_dir.
    fn scan(&self, hash: bool) {
        let mut args = vec!["scan", self.scan_dir.to_str().unwrap()];
        if hash { args.push("--hash"); }
        self.cmd().args(&args).assert().success();
    }

    /// Populate scan_dir with 3 identical files + 1 unique file.
    ///
    /// After `scan --hash`, original/copy1/copy2 form one duplicate group.
    fn write_duplicates(&self) {
        let dup  = b"duplicate content alpha — same BLAKE3 hash";
        let uniq = b"unique content xyz — different hash 987654";

        fs::write(self.scan_dir.join("original.txt"), dup).unwrap();
        fs::write(self.scan_dir.join("copy1.txt"),    dup).unwrap();
        fs::write(self.scan_dir.join("copy2.txt"),    dup).unwrap();
        fs::write(self.scan_dir.join("unique.txt"),   uniq).unwrap();
    }
}

// ── Test 1: dedup dry-run exits zero ──────────────────────────────────────────
#[test]
fn test_dedup_dry_run_exits_zero() {
    let env = TestEnv::new();
    env.write_duplicates();
    env.scan(true);

    env.cmd()
        .args(["dedup", "--dry-run"])
        .assert()
        .success();
}

// ── Test 2: dry-run leaves all files on disk ──────────────────────────────────
#[test]
fn test_dedup_dry_run_no_deletion() {
    let env = TestEnv::new();
    env.write_duplicates();
    env.scan(true);

    env.cmd()
        .args(["dedup", "--dry-run"])
        .assert()
        .success();

    // Every file must still exist — dry-run must never touch the filesystem.
    for name in &["original.txt", "copy1.txt", "copy2.txt", "unique.txt"] {
        assert!(
            env.scan_dir.join(name).exists(),
            "dry-run must not delete {name}"
        );
    }
}

// ── Test 3: --json output reports the correct group count ─────────────────────
#[test]
fn test_dedup_json_group_count() {
    let env = TestEnv::new();
    env.write_duplicates();
    env.scan(true);

    let output = env.cmd()
        .args(["--json", "dedup", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("stdout not UTF-8");

    // The report JSON contains `"groups_found": N`.
    // We extract it with a simple string search rather than pulling in serde_json.
    assert!(
        stdout.contains("\"groups_found\""),
        "JSON output missing 'groups_found'. Got: {stdout}"
    );
    assert!(
        stdout.contains("\"groups_found\": 1") || stdout.contains("\"groups_found\":1"),
        "expected groups_found = 1. Got: {stdout}"
    );
}

// ── Test 4: real dedup deletes copies, leaves one survivor ────────────────────
#[test]
fn test_dedup_deletes_copies() {
    let env = TestEnv::new();
    env.write_duplicates();
    env.scan(true);

    // Run real dedup (no --dry-run). --delete enables file removal.
    env.cmd()
        .args(["dedup", "--delete"])
        .assert()
        .success();

    // Exactly ONE of the three identical files should remain.
    let dup_names = ["original.txt", "copy1.txt", "copy2.txt"];
    let survivors: Vec<&str> = dup_names
        .iter()
        .filter(|&&n| env.scan_dir.join(n).exists())
        .copied()
        .collect();

    assert_eq!(
        survivors.len(), 1,
        "expected exactly 1 copy to survive, got: {survivors:?}"
    );

    // The unique file must not be touched.
    assert!(
        env.scan_dir.join("unique.txt").exists(),
        "unique.txt should never be deleted by dedup"
    );
}

// ── Test 5: --min-size excludes tiny files ────────────────────────────────────
#[test]
fn test_dedup_min_size_skips_small_files() {
    let env = TestEnv::new();
    env.write_duplicates();
    env.scan(true);

    // Our duplicate files are ~42 bytes. A 1 MiB minimum should exclude them.
    let output = env.cmd()
        .args(["--json", "dedup", "--dry-run", "--min-size", "1048576"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("stdout not UTF-8");

    // With the filter, no groups should be reported.
    assert!(
        stdout.contains("\"groups_found\": 0") || stdout.contains("\"groups_found\":0"),
        "--min-size 1MiB should skip tiny files. Got: {stdout}"
    );
}

// ── Test 6: dedup --hash hashes un-hashed files on the fly ───────────────────
// Scanning WITHOUT --hash leaves content_hash = NULL.
// `dedup --hash` should compute hashes before deduplicating.
#[test]
fn test_dedup_hashes_on_the_fly() {
    let env = TestEnv::new();
    env.write_duplicates();

    // Scan WITHOUT --hash so hashes remain NULL.
    env.scan(false);

    // Verify hashes are NULL.
    let conn = env.open_db();
    let null_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE content_hash IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(null_count > 0, "expected NULL hashes after plain scan");
    drop(conn);

    // Run dedup --hash — should hash and then find the duplicate group.
    let output = env.cmd()
        .args(["--json", "dedup", "--hash", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("stdout not UTF-8");

    // After on-the-fly hashing, our duplicate group must be found.
    assert!(
        !stdout.contains("\"groups_found\": 0") && !stdout.contains("\"groups_found\":0"),
        "dedup --hash should find duplicates; got: {stdout}"
    );
    assert!(
        stdout.contains("\"groups_found\": 1") || stdout.contains("\"groups_found\":1"),
        "expected exactly 1 group; got: {stdout}"
    );
}

// ── Test 7: dedup on zero-file database reports zero groups ───────────────────
#[test]
fn test_dedup_empty_database() {
    let env = TestEnv::new();

    // Scan an empty directory so the database exists but has no files.
    env.scan(true);

    let output = env.cmd()
        .args(["--json", "dedup", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("stdout not UTF-8");

    assert!(
        stdout.contains("\"groups_found\": 0") || stdout.contains("\"groups_found\":0"),
        "empty database should report 0 groups. Got: {stdout}"
    );
}

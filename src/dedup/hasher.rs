//! # dedup/hasher.rs
//!
//! BLAKE3 content hashing for deduplication.
//!
//! This module provides streaming file hashing using the BLAKE3 algorithm.
//! Files are read in chunks so we never load an entire large file into memory.
//!
//! ## Why BLAKE3?
//!
//! - Faster than SHA-256 and SHA-512 on modern hardware (often 2–5x faster)
//! - Cryptographically secure — collision resistance means two different files
//!   will never produce the same hash in practice
//! - Not broken like MD5 or SHA-1, which have known collision attacks
//! - Natively parallelisable for very large files (we use the streaming API
//!   here, which is fast enough for files up to several GB)
//!
//! ## Usage
//!
//! The scanner calls `hash_file(path)` to get a hex-encoded hash string.
//! The dedup module calls `hash_files_parallel(paths)` to hash many files
//! at once using rayon's work-stealing thread pool.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result};
use rayon::prelude::*;
use tracing::{debug, warn};

// The size of each chunk we read from disk at a time.
// 1 MiB is a good balance: large enough to amortise syscall overhead,
// small enough to fit comfortably in L3 cache on most systems.
const CHUNK_SIZE: usize = 1024 * 1024; // 1 MiB

// =============================================================================
// Single-file hashing
// =============================================================================

/// Compute the BLAKE3 hash of a file and return it as a lowercase hex string.
///
/// Reads the file in `CHUNK_SIZE` chunks so arbitrarily large files can be
/// hashed without loading them into memory. A 10 GB file uses only ~1 MB of
/// RAM during hashing.
///
/// Returns an error if the file cannot be opened or read, but does NOT error
/// on empty files — an empty file has a valid (and deterministic) BLAKE3 hash.
pub fn hash_file(path: &Path) -> Result<String> {
    // Open the file for reading. `File::open` returns Err if the file does not
    // exist, is a directory, or permission is denied.
    let file = File::open(path)
        .with_context(|| format!("could not open file for hashing: {}", path.display()))?;

    // `BufReader` wraps the file in a userspace buffer. Without it, each
    // `read()` call would be a separate syscall. BufReader batches small reads
    // into larger ones, reducing syscall overhead — especially important when
    // hashing millions of small files.
    let mut reader = BufReader::with_capacity(CHUNK_SIZE, file);

    // `blake3::Hasher` is an incremental hasher. We feed it data in chunks
    // and call `finalize()` at the end to get the final hash. This is the
    // streaming API — no need to hold the entire file in memory.
    let mut hasher = blake3::Hasher::new();

    // Reusable buffer — allocated once per file, reused across chunks.
    // Avoids repeated heap allocations inside the read loop.
    let mut buffer = vec![0u8; CHUNK_SIZE];

    loop {
        // Read up to CHUNK_SIZE bytes from the file into the buffer.
        // Returns the number of bytes actually read (may be less than CHUNK_SIZE
        // at the end of the file, or exactly 0 when EOF is reached).
        let bytes_read = reader.read(&mut buffer)
            .with_context(|| format!("error reading file during hashing: {}", path.display()))?;

        // `bytes_read == 0` means we have reached the end of the file.
        if bytes_read == 0 {
            break;
        }

        // Feed this chunk to the hasher. `&buffer[..bytes_read]` is a slice
        // of only the bytes that were actually read (not the full buffer).
        hasher.update(&buffer[..bytes_read]);
    }

    // `finalize()` produces the final BLAKE3 hash from all the chunks we fed.
    // `to_hex()` converts the 32-byte hash into a 64-character hex string.
    // `.to_string()` converts the fixed-size hex buffer into an owned String.
    let hash = hasher.finalize().to_hex().to_string();

    debug!("Hashed {} → {}", path.display(), &hash[..8]); // Log first 8 chars only

    Ok(hash)
}

// =============================================================================
// Parallel batch hashing
// =============================================================================

/// Hash a collection of files in parallel using rayon.
///
/// Returns a `Vec<HashResult>` — one entry per input path, in the same order.
/// Errors on individual files are captured in `HashResult::Err` rather than
/// aborting the entire batch. The caller decides what to do with failures.
///
/// This is the function the dedup module calls when it needs to hash many
/// files that were not hashed during the scan phase.
pub fn hash_files_parallel(paths: &[String]) -> Vec<HashResult> {
    // `par_iter()` distributes the paths across rayon's thread pool.
    // Each thread picks up a path, hashes it, and stores the result.
    // The `.collect()` at the end gathers all results back into a Vec,
    // preserving the original order (rayon guarantees order-preserving collect).
    paths
        .par_iter()
        .map(|path_str| {
            let path = Path::new(path_str);
            match hash_file(path) {
                Ok(hash) => {
                    HashResult::Ok {
                        path: path_str.clone(),
                        hash,
                    }
                }
                Err(e) => {
                    // Log the failure but don't abort — one bad file shouldn't
                    // stop all other files from being hashed.
                    warn!("Failed to hash {}: {:#}", path_str, e);
                    HashResult::Err {
                        path: path_str.clone(),
                        reason: e.to_string(),
                    }
                }
            }
        })
        .collect()
}

/// The result of hashing a single file.
///
/// Using an enum instead of `Result<(String, String), String>` makes the
/// success and failure cases explicit and self-documenting at call sites.
#[derive(Debug, Clone)]
pub enum HashResult {
    /// The file was hashed successfully.
    Ok {
        /// The absolute path that was hashed.
        path: String,
        /// The BLAKE3 hash as a 64-character lowercase hex string.
        hash: String,
    },
    /// Hashing failed for this file.
    Err {
        /// The path that could not be hashed.
        path: String,
        /// Human-readable description of what went wrong.
        reason: String,
    },
}

impl HashResult {
    /// Returns `true` if this result represents a successful hash.
    pub fn is_ok(&self) -> bool {
        matches!(self, HashResult::Ok { .. })
    }

    /// Extract the (path, hash) pair if this result is Ok, or None if Err.
    pub fn into_ok(self) -> Option<(String, String)> {
        match self {
            HashResult::Ok { path, hash } => Some((path, hash)),
            HashResult::Err { .. }        => None,
        }
    }
}

// =============================================================================
// Verification
// =============================================================================

/// Verify that a file still matches its stored hash.
///
/// Used by the backup module to confirm a file has not been corrupted or
/// modified since it was last scanned. Returns `true` if the hash matches.
pub fn verify_file(path: &Path, expected_hash: &str) -> Result<bool> {
    let actual = hash_file(path)?;
    // Constant-time comparison would be ideal for security-critical code,
    // but for file integrity checking, a simple equality check is fine.
    Ok(actual == expected_hash)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Helper: create a temp file with known content, return its path.
    fn temp_file_with(content: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f
    }

    #[test]
    fn test_hash_file_produces_64_char_hex() {
        let f = temp_file_with(b"hello, data fortress");
        let hash = hash_file(f.path()).unwrap();
        // BLAKE3 produces a 32-byte hash → 64 hex characters.
        assert_eq!(hash.len(), 64);
        // All characters must be valid hex digits.
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_same_content_produces_same_hash() {
        let f1 = temp_file_with(b"duplicate content");
        let f2 = temp_file_with(b"duplicate content");
        let h1 = hash_file(f1.path()).unwrap();
        let h2 = hash_file(f2.path()).unwrap();
        // Two files with identical content must produce identical hashes.
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_different_content_produces_different_hash() {
        let f1 = temp_file_with(b"file one");
        let f2 = temp_file_with(b"file two");
        let h1 = hash_file(f1.path()).unwrap();
        let h2 = hash_file(f2.path()).unwrap();
        // Different content must never produce the same hash.
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_empty_file_has_valid_hash() {
        let f = temp_file_with(b"");
        let hash = hash_file(f.path()).unwrap();
        // Empty files are valid — BLAKE3 produces a deterministic hash for them.
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn test_verify_file_matches_its_own_hash() {
        let f = temp_file_with(b"verify me");
        let hash = hash_file(f.path()).unwrap();
        assert!(verify_file(f.path(), &hash).unwrap());
    }

    #[test]
    fn test_verify_file_detects_wrong_hash() {
        let f = temp_file_with(b"original content");
        // Provide a hash that does not match the file.
        let wrong_hash = "a".repeat(64);
        assert!(!verify_file(f.path(), &wrong_hash).unwrap());
    }

    #[test]
    fn test_parallel_hashing_matches_sequential() {
        let f1 = temp_file_with(b"parallel file one");
        let f2 = temp_file_with(b"parallel file two");
        let f3 = temp_file_with(b"parallel file three");

        let paths = vec![
            f1.path().to_string_lossy().to_string(),
            f2.path().to_string_lossy().to_string(),
            f3.path().to_string_lossy().to_string(),
        ];

        // Hash sequentially for reference.
        let seq: Vec<String> = paths.iter()
            .map(|p| hash_file(Path::new(p)).unwrap())
            .collect();

        // Hash in parallel — results must match and be in the same order.
        let par: Vec<String> = hash_files_parallel(&paths)
            .into_iter()
            .map(|r| r.into_ok().unwrap().1)
            .collect();

        assert_eq!(seq, par);
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY streaming (chunked) hashing instead of read_to_end?
//    `fs::read(path)` loads the entire file into a Vec<u8>. For a 10 GB video
//    file this would require 10 GB of RAM. Streaming reads chunks of 1 MiB at
//    a time, so memory usage is constant regardless of file size. The hash
//    result is identical — BLAKE3 is designed for incremental updates.
//
// 2. WHY BufReader?
//    Every call to `File::read()` is a syscall. Without buffering, hashing
//    a file with tiny reads would flood the kernel with syscall overhead.
//    BufReader reads ahead in large blocks and serves small reads from its
//    internal buffer, dramatically reducing syscall count.
//
// 3. WHY rayon par_iter() for batch hashing?
//    Hashing is CPU-bound (BLAKE3 is compute-intensive) and I/O-bound
//    (reading from disk). On a multi-core machine with NVMe storage, hashing
//    files sequentially leaves most CPU cores idle. par_iter() saturates all
//    cores with minimal code change — just `.par_iter()` instead of `.iter()`.
//
// 4. WHY HashResult enum instead of Result?
//    `hash_files_parallel` processes a batch. If we used `Result<Vec<...>>`,
//    one failing file would abort the entire batch. With `Vec<HashResult>`,
//    failures are captured per-file and the caller can decide: skip? retry?
//    log and continue? This is more resilient for production use.
//
// 5. WHY 64 hex characters for a BLAKE3 hash?
//    BLAKE3 produces a 256-bit (32-byte) hash. Each byte encodes as 2 hex
//    characters, so 32 × 2 = 64 characters. This is the same output size as
//    SHA-256 and provides 128 bits of collision resistance — far more than
//    needed for file deduplication.

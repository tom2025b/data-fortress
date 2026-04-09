//! # search/mod.rs
//!
//! Content-based search engine for Data Fortress.
//!
//! ## Search pipeline
//!
//! ```text
//! User query string
//!      │
//!      ▼
//! tokenize()          — split query into lowercase tokens
//!      │
//!      ▼
//! db::search_files_by_name()   — fast metadata-only pass (SQLite LIKE)
//!      │
//!      ▼  (if --content flag set)
//! content_search()    — read + extract text, score against tokens
//!      │
//!      ▼
//! score_record()      — compute relevance score for each candidate
//!      │
//!      ▼
//! sort + limit        — order by score desc, cap at `limit`
//!      │
//!      ▼
//! Vec<SearchResult>   — returned to main.rs
//! ```
//!
//! ## Scoring
//!
//! Each file gets a `f64` relevance score. Higher = better match.
//! The score accumulates points for:
//! - Filename contains the full query string (highest weight)
//! - Filename contains individual query tokens
//! - File path contains tokens
//! - EXIF searchable_text contains tokens (images)
//! - Extracted content contains tokens (documents, with --content)
//!
//! This is not a full-text search engine (no inverted index, no TF-IDF).
//! For a personal file collection of hundreds of thousands of files, a
//! scored linear scan with SQLite pre-filtering is fast enough and far
//! simpler to maintain.

pub mod extractor;
pub mod exif;

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tracing::{debug, info};

use crate::cli::{SearchArgs, SearchCategory, SearchSort};
use crate::db;
use crate::models::{FileCategory, FileRecord, SearchResult};
use crate::search::extractor::{extract, make_snippet};
use crate::search::exif::extract_metadata;

// =============================================================================
// Score weights
// =============================================================================

// These constants control how much each type of match contributes to the score.
// Weights are additive — a file can score points from multiple sources.

/// Full query string found in the file name (exact phrase match).
const WEIGHT_NAME_EXACT:   f64 = 10.0;
/// Individual query token found in the file name.
const WEIGHT_NAME_TOKEN:   f64 = 4.0;
/// Individual query token found anywhere in the full path.
const WEIGHT_PATH_TOKEN:   f64 = 1.5;
/// Token found in EXIF/media metadata searchable text.
const WEIGHT_EXIF_TOKEN:   f64 = 2.0;
/// Token found in extracted document content.
const WEIGHT_CONTENT_TOKEN: f64 = 3.0;

// =============================================================================
// Public entry point
// =============================================================================

/// Run a search and return ranked results.
///
/// Called from `main.rs` after parsing the `search` subcommand arguments.
pub fn run(conn: &Connection, args: &SearchArgs) -> Result<Vec<SearchResult>> {
    let query = args.query.trim();

    if query.is_empty() {
        return Ok(Vec::new());
    }

    info!("Searching for: {:?}", query);

    // Step 1: tokenize the query into lowercase words.
    let tokens = tokenize(query);
    debug!("Query tokens: {:?}", tokens);

    // Step 2: fast metadata pre-filter using SQLite LIKE.
    // This narrows the candidate set before the expensive content scan.
    let mut candidates = db::search_files_by_name(conn, query)
        .context("database name search failed")?;

    // If a category filter was specified, also fetch all files in that category
    // and merge them into candidates (they might match on content even if the
    // name doesn't match the query).
    if let Some(ref cat) = args.category {
        let fc = search_category_to_file_category(cat);
        let category_files = db::search_files_by_category(conn, &fc)
            .context("database category search failed")?;

        // Merge: add category files not already in candidates.
        // Collect existing paths into an owned HashSet first so we don't
        // hold an immutable borrow on `candidates` while pushing to it.
        let existing_paths: std::collections::HashSet<String> =
            candidates.iter().map(|f| f.path.clone()).collect();

        let new_files: Vec<FileRecord> = category_files
            .into_iter()
            .filter(|f| !existing_paths.contains(&f.path))
            .collect();

        candidates.extend(new_files);
    }

    info!("Candidates after metadata filter: {}", candidates.len());

    // Step 3: score every candidate.
    let mut results: Vec<SearchResult> = candidates
        .into_iter()
        .filter_map(|file| score_file(file, query, &tokens, args.content))
        .filter(|r| r.score > 0.0) // Drop files that scored nothing
        .collect();

    // Step 4: sort by score descending (best match first).
    results.sort_by(|a, b| {
        match &args.sort {
            SearchSort::Relevance => b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal),
            SearchSort::Newest    => b.file.modified_at.cmp(&a.file.modified_at),
            SearchSort::Largest   => b.file.size_bytes.cmp(&a.file.size_bytes),
            SearchSort::Name      => a.file.name.cmp(&b.file.name),
        }
    });

    // Step 5: apply the result limit.
    results.truncate(args.limit);

    info!("Search returned {} result(s)", results.len());
    Ok(results)
}

// =============================================================================
// Scoring
// =============================================================================

/// Compute a relevance score for one file against the query.
///
/// Returns `None` if the file scores zero on all criteria — it will be
/// filtered out of results. Returns `Some(SearchResult)` otherwise.
fn score_file(
    file: FileRecord,
    query: &str,
    tokens: &[String],
    do_content_search: bool,
) -> Option<SearchResult> {
    let mut score = 0.0_f64;
    let mut snippet: Option<String> = None;

    let name_lower = file.name.to_lowercase();
    let path_lower = file.path.to_lowercase();
    let query_lower = query.to_lowercase();

    // ── Name scoring ──────────────────────────────────────────────────────────

    // Highest score: the full query appears verbatim in the file name.
    if name_lower.contains(&query_lower) {
        score += WEIGHT_NAME_EXACT;
    }

    // Additional points for each individual token found in the name.
    for token in tokens {
        if name_lower.contains(token.as_str()) {
            score += WEIGHT_NAME_TOKEN;
        }
    }

    // ── Path scoring ──────────────────────────────────────────────────────────

    // Smaller bonus for tokens found anywhere in the full path (parent dirs).
    // This lets a search for "vacation" find files in a "Vacations/" folder
    // even if the filename itself doesn't contain the word.
    for token in tokens {
        if path_lower.contains(token.as_str()) {
            score += WEIGHT_PATH_TOKEN;
        }
    }

    // ── EXIF / media metadata scoring ─────────────────────────────────────────

    // For image files, extract EXIF metadata and score against it.
    // We only do this if the file is an image to avoid wasting I/O on text files.
    match file.category {
        FileCategory::Image | FileCategory::Video => {
            if let Some(meta) = extract_metadata(Path::new(&file.path)) {
                let meta_lower = meta.searchable_text.to_lowercase();
                for token in tokens {
                    if meta_lower.contains(token.as_str()) {
                        score += WEIGHT_EXIF_TOKEN;
                    }
                }
                // Use the EXIF date/camera info as the snippet for image results.
                if snippet.is_none() && !meta.searchable_text.is_empty() {
                    snippet = Some(meta.searchable_text.clone());
                }
            }
        }
        _ => {}
    }

    // ── Content scoring ───────────────────────────────────────────────────────

    // If the user passed --content, extract and score document text.
    // This is the expensive path — we read and parse every candidate file.
    if do_content_search {
        if let Some(text) = extract(Path::new(&file.path)) {
            let text_lower = text.to_lowercase();
            let mut content_score = 0.0;

            for token in tokens {
                if text_lower.contains(token.as_str()) {
                    content_score += WEIGHT_CONTENT_TOKEN;
                }
            }

            if content_score > 0.0 {
                score += content_score;
                // Generate a snippet centered on the first query token found.
                if snippet.is_none() {
                    let first_token = tokens.first().map(|s| s.as_str()).unwrap_or(query);
                    snippet = Some(make_snippet(&text, first_token, 200));
                }
            }
        }
    }

    if score == 0.0 {
        return None;
    }

    Some(SearchResult { file, score, snippet })
}

// =============================================================================
// Tokenization
// =============================================================================

/// Split a query string into lowercase tokens, filtering out short words.
///
/// Tokens are split on whitespace and punctuation. Words shorter than 2
/// characters are dropped to avoid matching every file with single-letter
/// tokens like "a" or "I".
pub fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric()) // Split on non-alphanumeric chars
        .map(|s| s.to_lowercase())
        .filter(|s| s.len() >= 2) // Drop very short tokens
        .collect()
}

// =============================================================================
// Category conversion
// =============================================================================

/// Convert a CLI `SearchCategory` into the database's `FileCategory`.
///
/// These are two separate enums because CLI and model layers are deliberately
/// decoupled. This function is the bridge between them.
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
// Output formatting
// =============================================================================

/// Print search results to stdout in human-readable format.
///
/// Called by `main.rs` when `--json` is not set.
pub fn print_results(results: &[SearchResult]) {
    if results.is_empty() {
        println!("No results found.");
        return;
    }

    println!("\n{} result(s):\n", results.len());

    for (i, result) in results.iter().enumerate() {
        println!(
            "{}. {} (score: {:.1})",
            i + 1,
            result.file.path,
            result.score,
        );
        println!(
            "   {} | {} | {}",
            result.file.category,
            bytesize::ByteSize(result.file.size_bytes),
            result.file.modified_at.format("%Y-%m-%d"),
        );
        if let Some(ref snippet) = result.snippet {
            // Truncate very long snippets for terminal display.
            let display = if snippet.len() > 120 {
                format!("{}…", &snippet[..120])
            } else {
                snippet.clone()
            };
            println!("   \"{}\"", display);
        }
        println!();
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_splits_on_spaces() {
        let tokens = tokenize("vacation photos 2024");
        assert_eq!(tokens, vec!["vacation", "photos", "2024"]);
    }

    #[test]
    fn test_tokenize_splits_on_punctuation() {
        let tokens = tokenize("file_name-test.rs");
        // Underscores and hyphens and dots are split points.
        assert!(tokens.contains(&"file".to_string()));
        assert!(tokens.contains(&"name".to_string()));
        assert!(tokens.contains(&"test".to_string()));
        assert!(tokens.contains(&"rs".to_string()));
    }

    #[test]
    fn test_tokenize_drops_short_words() {
        let tokens = tokenize("a the it go");
        // "a" and "it" are too short (< 2 chars); "go" is exactly 2.
        assert!(!tokens.contains(&"a".to_string()));
        assert!(tokens.contains(&"go".to_string()));
    }

    #[test]
    fn test_tokenize_lowercases() {
        let tokens = tokenize("Vacation PHOTOS");
        assert!(tokens.contains(&"vacation".to_string()));
        assert!(tokens.contains(&"photos".to_string()));
    }

    #[test]
    fn test_tokenize_empty_query() {
        let tokens = tokenize("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_search_category_to_file_category_mapping() {
        assert_eq!(search_category_to_file_category(&SearchCategory::Image),    FileCategory::Image);
        assert_eq!(search_category_to_file_category(&SearchCategory::Document), FileCategory::Document);
        assert_eq!(search_category_to_file_category(&SearchCategory::Video),    FileCategory::Video);
        assert_eq!(search_category_to_file_category(&SearchCategory::Code),     FileCategory::Code);
    }

    #[test]
    fn test_score_weights_are_ordered() {
        // Sanity check: exact name match should outweigh a token match,
        // which should outweigh a path match.
        assert!(WEIGHT_NAME_EXACT > WEIGHT_NAME_TOKEN);
        assert!(WEIGHT_NAME_TOKEN > WEIGHT_PATH_TOKEN);
        assert!(WEIGHT_CONTENT_TOKEN > WEIGHT_PATH_TOKEN);
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY a two-phase search (SQLite first, then score)?
//    Running a full content scan on every file in the database is O(n) reads
//    of potentially millions of files — too slow for interactive use. SQLite's
//    LIKE query with an index pre-filters the candidate set to files whose
//    name or path contains the query, typically reducing candidates by 99%.
//    The expensive content extraction only runs on this small set.
//
// 2. WHY not build an inverted index?
//    An inverted index (like Elasticsearch or SQLite FTS5 uses) maps tokens
//    to file lists and enables sub-millisecond full-text search at any scale.
//    It requires: index build time after every scan, schema migration when
//    the index changes, and significantly more code. For a personal tool with
//    hundreds of thousands of files, a scored linear scan with pre-filtering
//    takes < 1 second and is far simpler. We can add FTS5 later if needed.
//
// 3. WHY are score weights f64 constants instead of config options?
//    Tuning search weights is a rabbit hole. For a personal tool, fixed weights
//    that "feel right" are good enough. If the user's use case is unusual (all
//    their files are named "scan_001.pdf"), they'd need per-collection tuning
//    that would require a full IR research project. Keep it simple.
//
// 4. WHY separate SearchCategory from FileCategory?
//    The CLI layer (cli.rs) and the model layer (models.rs) should not depend
//    on each other. If we added `ValueEnum` to FileCategory, we'd couple the
//    data model to Clap's derive macros. The conversion function here is the
//    explicit bridge — easy to find, easy to change.
//
// 5. WHY filter score == 0.0 after scoring?
//    The category filter can add files that match the category but not the
//    query at all. Without the zero-score filter, searching for "vacation"
//    with --category image would return every image in the database, not just
//    vacation photos. The filter ensures every result has some relevance signal.

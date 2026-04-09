//! # search/extractor.rs
//!
//! Plain-text extraction from document files for full-text search indexing.
//!
//! Each file format requires a different extraction strategy:
//!
//! | Format        | Crate         | Strategy                              |
//! |---------------|---------------|---------------------------------------|
//! | PDF           | pdf-extract   | Parse PDF content streams             |
//! | DOCX / PPTX   | zip + quick-xml | Unzip the archive, parse XML nodes  |
//! | XLSX / XLS    | calamine      | Iterate cell values                   |
//! | TXT / MD / etc.| std::fs      | Read directly as UTF-8                |
//! | Everything else| —            | Return None (no text to extract)      |
//!
//! ## Design
//!
//! `extract(path)` is the single public function. It dispatches to the correct
//! format handler based on the file extension. All handlers return
//! `Option<String>` — `None` means "no text available", not an error.
//! Errors inside handlers are logged as warnings and treated as `None`.
//!
//! This keeps the search engine resilient: one unreadable PDF never crashes
//! the indexing of thousands of other documents.

use std::fs;
use std::io::Read;
use std::path::Path;

use tracing::warn;

// =============================================================================
// Public entry point
// =============================================================================

/// Extract plain text from a file, returning `None` if extraction is not
/// possible or fails (binary files, unsupported formats, corrupt files, etc.)
///
/// The returned string is raw extracted text — not HTML, not formatted.
/// The search engine tokenizes it downstream.
pub fn extract(path: &Path) -> Option<String> {
    // Dispatch on the lowercase file extension.
    // We use extension-based dispatch here because by the time we're indexing
    // for search, the scanner has already validated the file type via magic
    // bytes. Extension dispatch is fast and correct for known document formats.
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        // PDF — use the pdf-extract crate
        "pdf" => extract_pdf(path),

        // Microsoft Word (modern XML-based format)
        "docx" => extract_docx(path),

        // Microsoft PowerPoint (modern XML-based format)
        "pptx" => extract_pptx(path),

        // Microsoft Excel (modern XML-based format)
        "xlsx" | "xls" => extract_xlsx(path),

        // Plain text variants — read directly
        "txt" | "md" | "markdown" | "rst" | "csv" | "log" => extract_text(path),

        // Source code files — treat as plain text for search purposes
        "rs" | "py" | "js" | "ts" | "go" | "c" | "cpp" | "h" | "hpp"
        | "java" | "rb" | "php" | "sh" | "bash" | "zsh" | "fish"
        | "toml" | "yaml" | "yml" | "json" | "xml" | "html" | "css" => extract_text(path),

        // Everything else (images, videos, archives, executables) → no text
        _ => None,
    }
}

// =============================================================================
// PDF extraction
// =============================================================================

/// Extract plain text from a PDF file using the `pdf-extract` crate.
///
/// PDF text extraction is best-effort — some PDFs store text as paths/images
/// (scanned documents) rather than actual text objects, in which case we
/// return None rather than garbage characters.
fn extract_pdf(path: &Path) -> Option<String> {
    // `pdf_extract::extract_text` reads the PDF and returns all text content
    // as a single String. It handles multi-page PDFs automatically.
    match pdf_extract::extract_text(path) {
        Ok(text) => {
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                // PDF parsed but contained no extractable text — likely a
                // scanned/image-only PDF. Return None to signal "no text".
                None
            } else {
                Some(trimmed)
            }
        }
        Err(e) => {
            // Log as a warning but don't propagate the error. One bad PDF
            // shouldn't stop the indexer from processing thousands of others.
            warn!("PDF extraction failed for {}: {}", path.display(), e);
            None
        }
    }
}

// =============================================================================
// DOCX extraction
// =============================================================================

/// Extract plain text from a .docx file.
///
/// DOCX is a ZIP archive containing XML files. The main document text lives
/// in `word/document.xml`. We open the ZIP, locate that file, and pull text
/// nodes out of the XML using quick-xml's event parser.
fn extract_docx(path: &Path) -> Option<String> {
    extract_office_xml(path, "word/document.xml")
}

/// Extract plain text from a .pptx file.
///
/// PPTX is structured similarly to DOCX. Slide text is spread across multiple
/// `ppt/slides/slide1.xml`, `slide2.xml`, etc. We extract all of them.
fn extract_pptx(path: &Path) -> Option<String> {
    // Open the ZIP archive.
    let file = fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    let mut all_text = String::new();

    // Iterate over all files in the archive and extract text from slide XMLs.
    // We collect the names first to avoid borrowing issues with the archive.
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| {
            let entry = archive.by_index(i).ok()?;
            let name = entry.name().to_string();
            // Match slide XML files: ppt/slides/slide1.xml, slide2.xml, etc.
            if name.starts_with("ppt/slides/slide") && name.ends_with(".xml") {
                Some(name)
            } else {
                None
            }
        })
        .collect();

    for name in names {
        if let Ok(mut entry) = archive.by_name(&name) {
            let mut xml_content = String::new();
            if entry.read_to_string(&mut xml_content).is_ok() {
                // Extract text nodes from this slide's XML.
                if let Some(slide_text) = parse_xml_text(&xml_content) {
                    if !all_text.is_empty() {
                        all_text.push('\n');
                    }
                    all_text.push_str(&slide_text);
                }
            }
        }
    }

    if all_text.is_empty() { None } else { Some(all_text) }
}

/// Extract text from a specific XML entry inside an Office ZIP archive.
///
/// Used for DOCX (word/document.xml) and as a helper for PPTX slides.
fn extract_office_xml(path: &Path, xml_entry: &str) -> Option<String> {
    // Open the file as a ZIP archive.
    let file = fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    // Locate the target XML entry inside the archive.
    let mut entry = archive.by_name(xml_entry).ok()?;

    // Read the entire XML into a string.
    let mut xml_content = String::new();
    entry.read_to_string(&mut xml_content).ok()?;

    // Parse text nodes out of the XML.
    parse_xml_text(&xml_content)
}

/// Parse all text content (`<w:t>` and `<a:t>` nodes) from an Office XML string.
///
/// Office XML stores visible text inside `<w:t>` elements (Word) or `<a:t>`
/// elements (PowerPoint). We use quick-xml's event-based parser to walk
/// the XML and collect the text content of those elements.
fn parse_xml_text(xml: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    // quick-xml trims whitespace from text events by default; we want it raw.
    reader.config_mut().trim_text(false);

    let mut output = String::new();
    let mut inside_text_element = false;

    loop {
        match reader.read_event() {
            // A Start or Empty element — check if it's a text-carrying tag.
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                // `<w:t>` = Word text run, `<a:t>` = PowerPoint text run.
                // We check the local name (after the namespace prefix colon).
                let local_name = e.local_name();
                inside_text_element = local_name.as_ref() == b"t";
            }

            // A text node — capture it if we're inside a <w:t> or <a:t>.
            Ok(Event::Text(e)) if inside_text_element => {
                if let Ok(text) = e.unescape() {
                    // Push a space between text runs to avoid words merging.
                    if !output.is_empty() && !output.ends_with(' ') {
                        output.push(' ');
                    }
                    output.push_str(text.trim());
                }
                inside_text_element = false;
            }

            // End element — reset the text-element flag.
            Ok(Event::End(_)) => {
                inside_text_element = false;
            }

            // End of file — we're done parsing.
            Ok(Event::Eof) => break,

            // Errors and other events (comments, processing instructions) — skip.
            Err(e) => {
                warn!("XML parse error: {}", e);
                break;
            }
            _ => {}
        }
    }

    let trimmed = output.trim().to_string();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

// =============================================================================
// XLSX extraction
// =============================================================================

/// Extract all cell text from an Excel spreadsheet (.xlsx or .xls).
///
/// Uses the `calamine` crate which handles both modern XLSX (ZIP+XML) and
/// legacy XLS (binary) formats. We concatenate all cell values across all
/// sheets, separated by spaces.
fn extract_xlsx(path: &Path) -> Option<String> {
    use calamine::{Reader, open_workbook_auto};

    // `open_workbook_auto` detects the format (XLSX vs XLS) from the file.
    let mut workbook = open_workbook_auto(path).ok()?;

    let mut all_text = String::new();

    // Iterate over all sheet names in the workbook.
    // `sheet_names()` returns a Vec<String>, so we clone to avoid borrow issues.
    let sheet_names: Vec<String> = workbook.sheet_names().to_vec();

    for sheet_name in sheet_names {
        // `worksheet_range` returns a 2D grid of `DataType` values.
        if let Ok(range) = workbook.worksheet_range(&sheet_name) {
            // Iterate over every row and cell in the sheet.
            for row in range.rows() {
                for cell in row {
                    // Convert each cell to its string representation.
                    // `DataType` can be Empty, Int, Float, Bool, String, Error.
                    let cell_str = cell.to_string();
                    let trimmed = cell_str.trim();
                    if !trimmed.is_empty() {
                        if !all_text.is_empty() {
                            // Separate cells with a space for tokenization.
                            all_text.push(' ');
                        }
                        all_text.push_str(trimmed);
                    }
                }
            }
        }
    }

    if all_text.is_empty() { None } else { Some(all_text) }
}

// =============================================================================
// Plain text extraction
// =============================================================================

/// Read a plain text file as UTF-8.
///
/// Handles files that are not valid UTF-8 by using lossy conversion —
/// invalid byte sequences are replaced with U+FFFD (the replacement character).
/// This is preferable to failing: a source code file with one non-UTF-8 comment
/// should still be searchable by its other contents.
fn extract_text(path: &Path) -> Option<String> {
    // `fs::read` gives us the raw bytes without assuming any encoding.
    let bytes = fs::read(path).ok()?;

    // `String::from_utf8_lossy` converts bytes to a string, replacing any
    // invalid UTF-8 sequences with U+FFFD. The `.into_owned()` call converts
    // the `Cow<str>` into an owned `String`.
    let text = String::from_utf8_lossy(&bytes).into_owned();

    let trimmed = text.trim().to_string();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

// =============================================================================
// Snippet generation
// =============================================================================

/// Extract a short snippet from a larger text containing a query term.
///
/// Used by the search engine to show context around matched content.
/// Returns a window of `window` characters centered on the first occurrence
/// of `query` (case-insensitive). Falls back to the first `window` characters
/// if the query is not found.
pub fn make_snippet(text: &str, query: &str, window: usize) -> String {
    // Find the first case-insensitive match of the query in the text.
    let lower_text  = text.to_lowercase();
    let lower_query = query.to_lowercase();

    let center = lower_text.find(&lower_query).unwrap_or(0);

    // Compute the start of the window, clamped so we don't go negative.
    let start = center.saturating_sub(window / 2);

    // Compute the end of the window, clamped to the text length.
    let end = (start + window).min(text.len());

    // Ensure start and end fall on valid UTF-8 character boundaries.
    // `text.is_char_boundary()` returns true if the byte index is valid.
    // We scan forward/backward until we find a boundary.
    let start = (0..=start).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    let end   = (end..=text.len()).find(|&i| text.is_char_boundary(i)).unwrap_or(text.len());

    let mut snippet = text[start..end].trim().to_string();

    // Add ellipsis markers to indicate the snippet is a fragment.
    if start > 0 { snippet = format!("…{}", snippet); }
    if end < text.len() { snippet.push('…'); }

    snippet
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_extract_plain_text_file() {
        let mut f = NamedTempFile::with_suffix(".txt").unwrap();
        f.write_all(b"Hello, Data Fortress!").unwrap();
        let text = extract(f.path()).unwrap();
        assert_eq!(text, "Hello, Data Fortress!");
    }

    #[test]
    fn test_extract_rust_source_file() {
        let mut f = NamedTempFile::with_suffix(".rs").unwrap();
        f.write_all(b"fn main() { println!(\"hello\"); }").unwrap();
        let text = extract(f.path()).unwrap();
        assert!(text.contains("fn main"));
    }

    #[test]
    fn test_extract_empty_file_returns_none() {
        let f = NamedTempFile::with_suffix(".txt").unwrap();
        // Empty file should return None, not Some("").
        assert!(extract(f.path()).is_none());
    }

    #[test]
    fn test_extract_binary_file_returns_none() {
        // A .jpg extension → the extractor returns None immediately.
        let f = NamedTempFile::with_suffix(".jpg").unwrap();
        assert!(extract(f.path()).is_none());
    }

    #[test]
    fn test_make_snippet_centered_on_query() {
        let text = "The quick brown fox jumps over the lazy dog";
        let snippet = make_snippet(text, "fox", 20);
        // The snippet should contain the query term.
        assert!(snippet.to_lowercase().contains("fox"));
    }

    #[test]
    fn test_make_snippet_falls_back_to_start() {
        let text = "Hello world this is a test";
        // Query not present → snippet starts from beginning.
        let snippet = make_snippet(text, "notfound", 10);
        assert!(!snippet.is_empty());
    }

    #[test]
    fn test_make_snippet_adds_ellipsis() {
        let text = "a".repeat(200);
        let snippet = make_snippet(&text, "a", 20);
        // A snippet from the middle of a long string should have ellipsis markers.
        assert!(snippet.contains('…'));
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY return Option<String> instead of Result<String>?
//    Extraction failure is common and expected: images have no text, some PDFs
//    are scanned, some files are corrupt. Using `Option` makes "no text" a
//    first-class outcome rather than an error. The search engine uses None to
//    mean "this file has no indexable text" and moves on — no error handling
//    needed at the call site.
//
// 2. WHY extension-based dispatch instead of MIME-type dispatch?
//    By the time we're extracting text, we already know the extension from
//    the FileRecord. Extension dispatch is fast, readable, and precise for
//    document formats. We use magic bytes in the scanner for initial
//    classification; here we use extension for targeted dispatch.
//
// 3. WHY is DOCX a ZIP file?
//    The Open Document XML format (used by .docx, .xlsx, .pptx) stores content
//    as XML files inside a ZIP archive. This makes the formats inspectable with
//    any ZIP tool — `unzip -l document.docx` shows you the internal structure.
//    We exploit this by using Rust's `zip` crate to open the archive and
//    `quick-xml` to parse the XML inside.
//
// 4. WHY quick-xml events instead of parsing the full DOM?
//    Loading the entire XML document into a DOM tree (like a browser does)
//    requires holding the whole document in memory at once. quick-xml's event
//    API streams through the document, emitting events (Start, Text, End) as
//    it reads. This is much more memory-efficient for large documents.
//
// 5. WHY String::from_utf8_lossy for plain text?
//    Not all "text" files are valid UTF-8. Config files, legacy source code,
//    and CSV exports often contain Latin-1 or Windows-1252 characters. Rather
//    than failing to index them, we replace invalid bytes with the replacement
//    character (U+FFFD). The file is still mostly readable and searchable.
//
// 6. WHY is_char_boundary in make_snippet?
//    Rust strings are UTF-8. Slicing a String at an arbitrary byte offset can
//    panic if that offset falls in the middle of a multi-byte character.
//    `is_char_boundary()` checks that the offset is at a valid character start,
//    preventing panics when the snippet window lands on a multi-byte codepoint.

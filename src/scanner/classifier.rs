//! # scanner/classifier.rs
//!
//! File type classification for the scanner.
//!
//! Given a file path, this module determines:
//!   1. The MIME type — detected from magic bytes first, extension as fallback.
//!   2. The `FileCategory` — a broad grouping (Image, Video, Document, etc.)
//!
//! ## Why magic bytes?
//!
//! File extensions can be wrong, missing, or misleading. A file named
//! `document.pdf` might actually be a JPEG. Reading the first few bytes of the
//! file (the "magic bytes" or file signature) gives us the true format.
//! We use the `infer` crate for magic-byte detection and fall back to
//! `mime_guess` (extension-based) when magic bytes are inconclusive.

use std::path::Path;

use crate::models::FileCategory;

// =============================================================================
// Public API
// =============================================================================

/// The result of classifying a single file.
///
/// Returned by `classify()` and used by the scanner to populate a FileRecord.
#[derive(Debug, Clone)]
pub struct Classification {
    /// MIME type string, e.g. "image/jpeg", "application/pdf", "text/plain".
    pub mime_type: String,

    /// Broad category derived from the MIME type.
    pub category: FileCategory,
}

/// Classify a file by detecting its MIME type and broad category.
///
/// Reads up to 16 bytes from the start of the file to check magic bytes.
/// Falls back to extension-based guessing if magic-byte detection fails or
/// returns an inconclusive result (e.g. plain text files have no magic bytes).
///
/// Never fails — returns `FileCategory::Other` with `"application/octet-stream"`
/// if the type cannot be determined at all.
pub fn classify(path: &Path) -> Classification {
    // Attempt magic-byte detection first — it's the most reliable method.
    let mime_from_magic = detect_from_magic(path);

    // Use magic-byte result if we got one; otherwise fall back to extension.
    let mime_type = mime_from_magic
        .or_else(|| detect_from_extension(path))
        .unwrap_or_else(|| "application/octet-stream".to_string());

    // Derive the broad FileCategory from the resolved MIME type string.
    let category = category_from_mime(&mime_type);

    Classification { mime_type, category }
}

// =============================================================================
// Magic-byte detection
// =============================================================================

/// Try to detect the MIME type by reading the file's magic bytes.
///
/// The `infer` crate maintains a list of known file signatures (e.g. JPEG
/// files start with `FF D8 FF`, PNG files start with `89 50 4E 47`).
/// We read up to 16 bytes — enough for all signatures `infer` checks.
///
/// Returns `None` if the file cannot be read or the signature is not recognised.
fn detect_from_magic(path: &Path) -> Option<String> {
    use std::fs::File;
    use std::io::Read;

    // Open the file for reading. If it fails (e.g. permission denied), return
    // None so the caller can fall back to extension-based detection.
    let mut file = File::open(path).ok()?;

    // Read the first 16 bytes into a fixed-size buffer.
    // 16 bytes is sufficient for all signatures the `infer` crate uses.
    let mut buf = [0u8; 16];
    let bytes_read = file.read(&mut buf).ok()?;

    // Guard against empty files — no bytes means no signature to check.
    if bytes_read == 0 {
        return None;
    }

    // `infer::get` checks the byte slice against its internal signature table.
    // Returns Some(Type) with a MIME type string, or None if unrecognised.
    let inferred = infer::get(&buf[..bytes_read])?;

    // `inferred.mime_type()` returns a &str like "image/jpeg".
    // We convert to String so the caller owns it.
    Some(inferred.mime_type().to_string())
}

// =============================================================================
// Extension-based detection
// =============================================================================

/// Try to detect the MIME type from the file's extension.
///
/// Uses the `mime_guess` crate, which contains a large mapping of extensions
/// to MIME types. Less reliable than magic bytes but works for files we cannot
/// read (e.g. very large files where we skip reading) or for text formats that
/// have no magic bytes (plain .txt, .rs, .py, etc.).
///
/// Returns `None` if the file has no extension or the extension is unknown.
fn detect_from_extension(path: &Path) -> Option<String> {
    // `mime_guess::from_path` reads the extension from the path and looks it up.
    // `.first()` returns the most likely MIME type for that extension.
    let guess = mime_guess::from_path(path).first()?;

    // `Mime` implements `Display`, so `.to_string()` gives us "image/jpeg" etc.
    Some(guess.to_string())
}

// =============================================================================
// Category derivation
// =============================================================================

/// Map a MIME type string to a broad `FileCategory`.
///
/// We only look at the MIME type prefix (e.g. "image/" covers all image
/// subtypes: image/jpeg, image/png, image/webp, etc.). Specific subtypes
/// are checked where needed (e.g. "application/pdf" → Document).
pub fn category_from_mime(mime: &str) -> FileCategory {
    // Split the MIME type on '/' to get the top-level type and subtype.
    // e.g. "image/jpeg" → type_part = "image", subtype = "jpeg"
    let type_part = mime.split('/').next().unwrap_or("");
    let subtype   = mime.split('/').nth(1).unwrap_or("");

    match type_part {
        // All image/* types → Image
        "image" => FileCategory::Image,

        // All video/* types → Video
        "video" => FileCategory::Video,

        // All audio/* types → Audio
        "audio" => FileCategory::Audio,

        // text/* is mostly documents, but source code is a special case
        "text" => classify_text_subtype(subtype),

        // application/* covers a huge range — we need to look at the subtype
        "application" => classify_application_subtype(subtype),

        // font/*, model/*, chemical/*, etc. → Other
        _ => FileCategory::Other,
    }
}

/// Classify text/* subtypes into Document or Code.
fn classify_text_subtype(subtype: &str) -> FileCategory {
    // These subtypes are programming languages or data formats → Code
    const CODE_SUBTYPES: &[&str] = &[
        "x-rust", "x-python", "x-c", "x-c++", "x-java", "x-javascript",
        "javascript", "typescript", "x-typescript", "x-sh", "x-shellscript",
        "x-ruby", "x-go", "x-swift", "x-kotlin", "css", "html", "xml",
        "x-yaml", "yaml", "x-toml", "x-makefile", "x-asm",
    ];

    // `iter().any()` returns true if any element in the slice matches subtype.
    if CODE_SUBTYPES.iter().any(|&s| s == subtype) {
        FileCategory::Code
    } else {
        // plain text, markdown, csv, etc. → Document
        FileCategory::Document
    }
}

/// Classify application/* subtypes.
fn classify_application_subtype(subtype: &str) -> FileCategory {
    // Document formats
    const DOCUMENT_SUBTYPES: &[&str] = &[
        "pdf",
        "msword",
        "vnd.openxmlformats-officedocument.wordprocessingml.document", // .docx
        "vnd.oasis.opendocument.text",                                  // .odt
        "vnd.ms-excel",
        "vnd.openxmlformats-officedocument.spreadsheetml.sheet",        // .xlsx
        "vnd.oasis.opendocument.spreadsheet",                           // .ods
        "vnd.ms-powerpoint",
        "vnd.openxmlformats-officedocument.presentationml.presentation", // .pptx
        "rtf",
        "epub+zip",
        "vnd.amazon.ebook",
    ];

    // Archive / compressed formats
    const ARCHIVE_SUBTYPES: &[&str] = &[
        "zip",
        "x-tar",
        "gzip",
        "x-gzip",
        "x-bzip2",
        "x-7z-compressed",
        "x-rar-compressed",
        "vnd.rar",
        "x-xz",
        "x-lzma",
        "zstd",
        "x-zstd",
        "x-iso9660-image",
    ];

    // Source code or data formats delivered as application/*
    const CODE_SUBTYPES: &[&str] = &[
        "json",
        "xml",
        "x-yaml",
        "javascript",
        "ecmascript",
        "x-httpd-php",
        "x-ruby",
        "x-perl",
        "x-python",
        "wasm",
        "graphql",
        "ld+json",
        "schema+json",
    ];

    if DOCUMENT_SUBTYPES.iter().any(|&s| s == subtype) {
        FileCategory::Document
    } else if ARCHIVE_SUBTYPES.iter().any(|&s| s == subtype) {
        FileCategory::Archive
    } else if CODE_SUBTYPES.iter().any(|&s| s == subtype) {
        FileCategory::Code
    } else {
        // application/octet-stream, executables, unknown binaries → Other
        FileCategory::Other
    }
}

// =============================================================================
// Extension → category fast path
// =============================================================================

/// Classify a file by extension only, without reading the file.
///
/// Used when we need a quick category (e.g. in the scanner's first pass)
/// without the overhead of opening files for magic-byte detection.
/// Less accurate than `classify()` but very fast.
pub fn category_from_extension(ext: &str) -> FileCategory {
    // Build the fake path "file.ext" so mime_guess can do its extension lookup.
    // We never use this path to access the filesystem — it's just a trick to
    // reuse mime_guess's extension-to-MIME mapping without touching disk.
    let fake_path = format!("file.{}", ext);
    match detect_from_extension(Path::new(&fake_path)) {
        Some(mime) => category_from_mime(&mime),
        None       => FileCategory::Other,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Tests live inside the module they test (Rust convention).
    // `#[cfg(test)]` means this block is compiled only during `cargo test`.

    #[test]
    fn test_image_mime_maps_to_image_category() {
        assert_eq!(category_from_mime("image/jpeg"), FileCategory::Image);
        assert_eq!(category_from_mime("image/png"),  FileCategory::Image);
        assert_eq!(category_from_mime("image/webp"), FileCategory::Image);
    }

    #[test]
    fn test_video_mime_maps_to_video_category() {
        assert_eq!(category_from_mime("video/mp4"),  FileCategory::Video);
        assert_eq!(category_from_mime("video/x-matroska"), FileCategory::Video);
    }

    #[test]
    fn test_document_subtypes() {
        assert_eq!(category_from_mime("application/pdf"),   FileCategory::Document);
        assert_eq!(category_from_mime("application/msword"), FileCategory::Document);
    }

    #[test]
    fn test_archive_subtypes() {
        assert_eq!(category_from_mime("application/zip"),       FileCategory::Archive);
        assert_eq!(category_from_mime("application/x-tar"),     FileCategory::Archive);
        assert_eq!(category_from_mime("application/x-7z-compressed"), FileCategory::Archive);
    }

    #[test]
    fn test_code_subtypes() {
        assert_eq!(category_from_mime("application/json"),       FileCategory::Code);
        assert_eq!(category_from_mime("text/x-rust"),            FileCategory::Code);
        assert_eq!(category_from_mime("text/x-python"),          FileCategory::Code);
    }

    #[test]
    fn test_unknown_falls_back_to_other() {
        assert_eq!(category_from_mime("application/octet-stream"), FileCategory::Other);
        assert_eq!(category_from_mime("model/gltf+json"),          FileCategory::Other);
    }

    #[test]
    fn test_category_from_extension() {
        assert_eq!(category_from_extension("jpg"),  FileCategory::Image);
        assert_eq!(category_from_extension("mp4"),  FileCategory::Video);
        assert_eq!(category_from_extension("pdf"),  FileCategory::Document);
        assert_eq!(category_from_extension("zip"),  FileCategory::Archive);
        assert_eq!(category_from_extension("rs"),   FileCategory::Code);
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY magic bytes over file extensions?
//    File extensions are user-controlled and often wrong. Someone can rename
//    a JPEG to ".txt" and the extension is now misleading. Magic bytes are
//    embedded in the file itself by the program that created it and cannot
//    be changed by renaming. For a deduplication/organization tool, accuracy
//    here matters: misclassifying a photo as a document would put it in the
//    wrong folder during organization.
//
// 2. WHY read only 16 bytes?
//    All magic byte signatures the `infer` crate checks are ≤ 16 bytes long.
//    Reading the minimum necessary avoids loading large files and keeps the
//    scanner fast. A 4 GB video file is classified from its first 16 bytes.
//
// 3. WHY does `classify()` never fail?
//    The scanner runs over potentially millions of files. If classification
//    raised an error on an unknown type, one unusual file would abort the
//    entire scan. Instead we return `Other` / `application/octet-stream` and
//    let the user deal with it manually. Resilience over strictness.
//
// 4. WHY `const CODE_SUBTYPES: &[&str]`?
//    A `const` slice of string literals is stored in the read-only data
//    section of the binary — no heap allocation at runtime. Using `const`
//    instead of `let` makes it clear this is a compile-time constant list,
//    not something that changes at runtime.
//
// 5. WHY separate `category_from_extension` fast path?
//    The scanner's first pass needs to classify millions of files quickly.
//    Magic-byte detection requires opening every file. By using extension
//    classification in the first pass and magic bytes only on uncertain cases,
//    we trade a little accuracy for a lot of speed on common file types.

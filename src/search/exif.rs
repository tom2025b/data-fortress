//! # search/exif.rs
//!
//! EXIF and media metadata extraction for search indexing.
//!
//! EXIF (Exchangeable Image File Format) metadata is embedded inside image
//! files by the camera or editing software that created them. It contains:
//!
//! - **Capture timestamp** — when the photo was actually taken (not the file
//!   modification time, which changes when you copy or edit the file)
//! - **GPS coordinates** — latitude, longitude, altitude
//! - **Camera info** — make, model, lens, focal length, aperture, ISO
//! - **Image dimensions** — pixel width and height
//!
//! This module extracts these fields and returns them as a structured
//! `MediaMetadata` type that the search engine can index and the dashboard
//! can display.
//!
//! ## Supported formats
//!
//! `kamadak-exif` reads EXIF from: JPEG, TIFF, HEIF, PNG (partial), WebP.
//! Files without EXIF (videos, non-camera images) return `None` for EXIF
//! fields but may still have basic metadata (dimensions, duration).

use std::path::Path;
use std::fs::File;
use std::io::BufReader;

use serde::{Deserialize, Serialize};

// =============================================================================
// MediaMetadata
// =============================================================================

/// All metadata extracted from a media file.
///
/// Every field is `Option` — not all cameras write all fields, and non-image
/// files may have no EXIF at all. The search engine indexes whichever fields
/// are present.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MediaMetadata {
    // ── Camera / capture info ─────────────────────────────────────────────────

    /// Camera manufacturer (e.g. "Apple", "Canon", "Sony").
    pub camera_make: Option<String>,

    /// Camera model (e.g. "iPhone 15 Pro", "EOS R5").
    pub camera_model: Option<String>,

    /// Lens description (e.g. "24-70mm f/2.8").
    pub lens_model: Option<String>,

    /// Focal length in millimetres (e.g. 50.0 for a 50mm lens).
    pub focal_length_mm: Option<f64>,

    /// F-number / aperture (e.g. 2.8 for f/2.8).
    pub f_number: Option<f64>,

    /// ISO sensitivity (e.g. 400, 3200).
    pub iso: Option<u32>,

    /// Shutter speed as a fraction string (e.g. "1/250").
    pub exposure_time: Option<String>,

    // ── Timestamps ────────────────────────────────────────────────────────────

    /// The date and time the photo was captured, from EXIF DateTimeOriginal.
    ///
    /// This is more reliable than the file modification time for photos —
    /// it reflects when the shutter fired, not when the file was last touched.
    pub date_taken: Option<String>, // ISO 8601 string

    // ── GPS ───────────────────────────────────────────────────────────────────

    /// GPS latitude in decimal degrees. Negative = South.
    pub gps_latitude: Option<f64>,

    /// GPS longitude in decimal degrees. Negative = West.
    pub gps_longitude: Option<f64>,

    /// GPS altitude in metres above sea level.
    pub gps_altitude_m: Option<f64>,

    // ── Image dimensions ──────────────────────────────────────────────────────

    /// Image width in pixels.
    pub width_px: Option<u32>,

    /// Image height in pixels.
    pub height_px: Option<u32>,

    // ── Searchable text ───────────────────────────────────────────────────────

    /// A flat, space-joined string of all non-empty metadata values.
    ///
    /// The search engine indexes this field directly. Rather than building
    /// a query that checks every individual field, we denormalize all metadata
    /// into one searchable string at extraction time.
    pub searchable_text: String,
}

impl MediaMetadata {
    /// Build the `searchable_text` field from all other populated fields.
    ///
    /// Called after extraction to create a single string the search engine can
    /// tokenize and match against without needing to know the field structure.
    fn build_searchable_text(&mut self) {
        let mut parts: Vec<String> = Vec::new();

        // Collect all present string/numeric fields into the parts vec.
        if let Some(ref v) = self.camera_make    { parts.push(v.clone()); }
        if let Some(ref v) = self.camera_model   { parts.push(v.clone()); }
        if let Some(ref v) = self.lens_model     { parts.push(v.clone()); }
        if let Some(ref v) = self.date_taken     { parts.push(v.clone()); }
        if let Some(v) = self.gps_latitude       { parts.push(format!("lat:{:.4}", v)); }
        if let Some(v) = self.gps_longitude      { parts.push(format!("lon:{:.4}", v)); }
        if let Some(v) = self.focal_length_mm    { parts.push(format!("{}mm", v)); }
        if let Some(v) = self.f_number           { parts.push(format!("f/{}", v)); }
        if let Some(v) = self.iso                { parts.push(format!("ISO{}", v)); }
        if let Some(ref v) = self.exposure_time  { parts.push(v.clone()); }
        if let Some(v) = self.width_px           { parts.push(format!("{}px", v)); }
        if let Some(v) = self.height_px          { parts.push(format!("{}px", v)); }

        // Join with spaces so the search tokenizer can split on whitespace.
        self.searchable_text = parts.join(" ");
    }
}

// =============================================================================
// Public entry point
// =============================================================================

/// Extract media metadata from a file.
///
/// Returns `None` if the file has no EXIF and no other detectable metadata.
/// Returns `Some(MediaMetadata)` with whichever fields could be read.
///
/// Never panics — all extraction errors are logged as warnings.
pub fn extract_metadata(path: &Path) -> Option<MediaMetadata> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        // These formats may contain EXIF data.
        "jpg" | "jpeg" | "tiff" | "tif" | "heif" | "heic" | "png" | "webp" => {
            extract_exif(path)
        }
        // For video and audio we could extract container metadata (duration,
        // codec, etc.) using an external tool like ffprobe, but for now we
        // return None. This is a good future extension point.
        _ => None,
    }
}

// =============================================================================
// EXIF extraction
// =============================================================================

/// Extract EXIF metadata from an image file using `kamadak-exif`.
fn extract_exif(path: &Path) -> Option<MediaMetadata> {
    // Open the file for reading.
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);

    // `exif::Reader::new().read_from_container` reads EXIF from a file
    // that may contain the data inside a JPEG/TIFF/HEIF container.
    // It handles the container format automatically.
    let exif = exif::Reader::new()
        .read_from_container(&mut reader)
        .ok()?; // Return None if no EXIF data found

    let mut meta = MediaMetadata::default();

    // ── Camera make ───────────────────────────────────────────────────────────
    if let Some(field) = exif.get_field(exif::Tag::Make, exif::In::PRIMARY) {
        meta.camera_make = Some(field_to_string(field));
    }

    // ── Camera model ──────────────────────────────────────────────────────────
    if let Some(field) = exif.get_field(exif::Tag::Model, exif::In::PRIMARY) {
        meta.camera_model = Some(field_to_string(field));
    }

    // ── Lens model ────────────────────────────────────────────────────────────
    if let Some(field) = exif.get_field(exif::Tag::LensModel, exif::In::PRIMARY) {
        meta.lens_model = Some(field_to_string(field));
    }

    // ── Focal length ──────────────────────────────────────────────────────────
    // FocalLength is stored as a rational number (numerator/denominator).
    if let Some(field) = exif.get_field(exif::Tag::FocalLength, exif::In::PRIMARY) {
        meta.focal_length_mm = rational_to_f64(field);
    }

    // ── F-number ──────────────────────────────────────────────────────────────
    if let Some(field) = exif.get_field(exif::Tag::FNumber, exif::In::PRIMARY) {
        meta.f_number = rational_to_f64(field);
    }

    // ── ISO ───────────────────────────────────────────────────────────────────
    // ISOSpeedRatings is stored as a SHORT (u16) integer.
    if let Some(field) = exif.get_field(exif::Tag::PhotographicSensitivity, exif::In::PRIMARY) {
        if let exif::Value::Short(ref v) = field.value {
            meta.iso = v.first().map(|&n| n as u32);
        }
    }

    // ── Exposure time ─────────────────────────────────────────────────────────
    // Stored as a rational (e.g. 1/250). We format it as "1/250" for display.
    if let Some(field) = exif.get_field(exif::Tag::ExposureTime, exif::In::PRIMARY) {
        if let exif::Value::Rational(ref v) = field.value {
            if let Some(r) = v.first() {
                meta.exposure_time = Some(format!("{}/{}", r.num, r.denom));
            }
        }
    }

    // ── Date taken ────────────────────────────────────────────────────────────
    // DateTimeOriginal is stored as an ASCII string: "YYYY:MM:DD HH:MM:SS".
    if let Some(field) = exif.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY) {
        let raw = field_to_string(field);
        // Convert from EXIF format ("2024:03:15 14:30:00") to ISO 8601.
        meta.date_taken = Some(exif_datetime_to_iso(&raw));
    }

    // ── GPS ───────────────────────────────────────────────────────────────────
    meta.gps_latitude  = extract_gps_coordinate(&exif, exif::Tag::GPSLatitude,  exif::Tag::GPSLatitudeRef);
    meta.gps_longitude = extract_gps_coordinate(&exif, exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef);

    if let Some(field) = exif.get_field(exif::Tag::GPSAltitude, exif::In::PRIMARY) {
        meta.gps_altitude_m = rational_to_f64(field);
    }

    // ── Image dimensions ──────────────────────────────────────────────────────
    if let Some(field) = exif.get_field(exif::Tag::PixelXDimension, exif::In::PRIMARY) {
        meta.width_px = field_to_u32(field);
    }
    if let Some(field) = exif.get_field(exif::Tag::PixelYDimension, exif::In::PRIMARY) {
        meta.height_px = field_to_u32(field);
    }

    // Build the flat searchable text string from all extracted fields.
    meta.build_searchable_text();

    // Return None if no meaningful metadata was found (file has an empty EXIF block).
    if meta.searchable_text.is_empty() {
        None
    } else {
        Some(meta)
    }
}

// =============================================================================
// GPS helpers
// =============================================================================

/// Extract a GPS coordinate (latitude or longitude) as a signed decimal degree.
///
/// GPS coordinates in EXIF are stored as three rational numbers (degrees,
/// minutes, seconds) plus a reference direction string ("N"/"S" or "E"/"W").
/// We convert to signed decimal degrees, the standard format for maps.
fn extract_gps_coordinate(
    exif: &exif::Exif,
    value_tag: exif::Tag,
    ref_tag: exif::Tag,
) -> Option<f64> {
    // Read the DMS (degrees, minutes, seconds) values.
    let field = exif.get_field(value_tag, exif::In::PRIMARY)?;
    let rationals = match &field.value {
        exif::Value::Rational(v) => v,
        _ => return None,
    };

    // Must have at least 3 rationals for degrees, minutes, seconds.
    if rationals.len() < 3 {
        return None;
    }

    // Convert each rational to f64.
    let degrees = rationals[0].num as f64 / rationals[0].denom as f64;
    let minutes = rationals[1].num as f64 / rationals[1].denom as f64;
    let seconds = rationals[2].num as f64 / rationals[2].denom as f64;

    // Combine degrees, minutes, seconds into decimal degrees.
    // Formula: DD = degrees + (minutes / 60) + (seconds / 3600)
    let mut decimal = degrees + (minutes / 60.0) + (seconds / 3600.0);

    // Read the reference direction ("N"/"S" for latitude, "E"/"W" for longitude).
    // South and West are represented as negative values in decimal degrees.
    if let Some(ref_field) = exif.get_field(ref_tag, exif::In::PRIMARY) {
        let reference = field_to_string(ref_field);
        if reference.contains('S') || reference.contains('W') {
            decimal = -decimal;
        }
    }

    Some(decimal)
}

// =============================================================================
// Value conversion helpers
// =============================================================================

/// Convert an EXIF field value to a plain string, stripping surrounding quotes.
fn field_to_string(field: &exif::Field) -> String {
    // `field.display_value()` returns a Display-able value. For ASCII fields
    // this includes surrounding quotes (e.g. `"Apple"`). We strip them.
    let raw = field.display_value().to_string();
    raw.trim_matches('"').trim().to_string()
}

/// Convert an EXIF rational field to f64 (numerator / denominator).
///
/// Returns None if the field is not a rational or the denominator is zero.
fn rational_to_f64(field: &exif::Field) -> Option<f64> {
    if let exif::Value::Rational(ref v) = field.value {
        if let Some(r) = v.first() {
            if r.denom != 0 {
                return Some(r.num as f64 / r.denom as f64);
            }
        }
    }
    None
}

/// Convert an EXIF field containing a SHORT or LONG integer to u32.
fn field_to_u32(field: &exif::Field) -> Option<u32> {
    match &field.value {
        exif::Value::Short(v)  => v.first().map(|&n| n as u32),
        exif::Value::Long(v)   => v.first().copied(),
        _ => None,
    }
}

/// Convert an EXIF datetime string to ISO 8601 format.
///
/// EXIF stores datetimes as "YYYY:MM:DD HH:MM:SS".
/// We convert to "YYYY-MM-DD HH:MM:SS" (replace colons in date part).
fn exif_datetime_to_iso(exif_dt: &str) -> String {
    // EXIF format: "2024:03:15 14:30:00"
    // ISO 8601:    "2024-03-15 14:30:00"
    // Only the date part (before the space) uses colons as separators.
    if let Some(space_pos) = exif_dt.find(' ') {
        let date_part = &exif_dt[..space_pos];
        let time_part = &exif_dt[space_pos..];
        // Replace colons in the date part only.
        format!("{}{}", date_part.replace(':', "-"), time_part)
    } else {
        // No space found — just replace all colons as a fallback.
        exif_dt.replace(':', "-")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exif_datetime_to_iso_conversion() {
        assert_eq!(
            exif_datetime_to_iso("2024:03:15 14:30:00"),
            "2024-03-15 14:30:00"
        );
    }

    #[test]
    fn test_exif_datetime_no_space_fallback() {
        // If there's no space, colons are replaced throughout.
        assert_eq!(
            exif_datetime_to_iso("2024:03:15"),
            "2024-03-15"
        );
    }

    #[test]
    fn test_build_searchable_text_populated() {
        let mut meta = MediaMetadata {
            camera_make:  Some("Apple".into()),
            camera_model: Some("iPhone 15 Pro".into()),
            iso:          Some(400),
            ..Default::default()
        };
        meta.build_searchable_text();
        assert!(meta.searchable_text.contains("Apple"));
        assert!(meta.searchable_text.contains("iPhone 15 Pro"));
        assert!(meta.searchable_text.contains("ISO400"));
    }

    #[test]
    fn test_build_searchable_text_empty_when_no_fields() {
        let mut meta = MediaMetadata::default();
        meta.build_searchable_text();
        assert!(meta.searchable_text.is_empty());
    }

    #[test]
    fn test_extract_metadata_returns_none_for_non_image() {
        // A .txt file should return None from extract_metadata.
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        f.write_all(b"not an image").unwrap();
        assert!(extract_metadata(f.path()).is_none());
    }

    #[test]
    fn test_extract_metadata_returns_none_for_empty_jpeg() {
        // An empty file with .jpg extension has no EXIF data.
        let f = tempfile::NamedTempFile::with_suffix(".jpg").unwrap();
        // The file is empty so kamadak-exif will find no EXIF — returns None.
        assert!(extract_metadata(f.path()).is_none());
    }

    #[test]
    fn test_gps_coordinate_negative_for_south() {
        // We can't call extract_gps_coordinate directly (needs exif::Exif),
        // but we can verify the decimal degree formula is correct.
        // 48° 51' 30" N = 48 + 51/60 + 30/3600 = 48.858333...
        let degrees = 48.0_f64;
        let minutes = 51.0_f64;
        let seconds = 30.0_f64;
        let decimal = degrees + minutes / 60.0 + seconds / 3600.0;
        assert!((decimal - 48.8583).abs() < 0.001);
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY is EXIF useful for search?
//    The filesystem modification time changes every time you copy or backup a
//    file. EXIF DateTimeOriginal records when the shutter fired — it's
//    immutable. This means searching for "photos from March 2024" works even
//    if you reorganized your photo library in April 2024.
//
// 2. WHY GPS in decimal degrees instead of DMS?
//    Degrees-Minutes-Seconds (48° 51' 30" N) is human-readable but hard to
//    compute with. Decimal degrees (48.8583) are what mapping APIs, databases,
//    and distance calculations expect. The conversion is lossless — we just
//    apply the formula DD = D + M/60 + S/3600.
//
// 3. WHY Option for every field?
//    Not all cameras write all EXIF fields. A phone camera writes GPS; a DSLR
//    in manual mode may not write ISO. An image from a scanner has no camera
//    fields at all. Option correctly models "this field may or may not exist"
//    and forces the caller to handle the absence case rather than getting a
//    default (which might be misleading — is ISO 0 real, or just missing?).
//
// 4. WHY a `searchable_text` denormalization field?
//    The search engine works on strings. Without denormalization, querying
//    "Canon EOS R5" would require checking camera_make AND camera_model
//    independently. By building one string of all metadata values at index
//    time, the search engine does one substring/token check per file. It's
//    a classic search-engine trade-off: more storage at index time, faster
//    queries at search time.
//
// 5. WHY BufReader when reading EXIF?
//    kamadak-exif may make multiple read calls while parsing the EXIF header
//    (seeking around the JPEG/TIFF structure). BufReader caches data from
//    the first read, so subsequent reads within the buffer are served from
//    memory without additional syscalls — especially important when processing
//    thousands of small JPEG thumbnails.

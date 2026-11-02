//! Filename validation, Windows DOS device reserved name checking, and safe demo naming functions.

use crate::MAX_FILENAME_LEN;

const INVALID_FILENAME_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

/// DOS device names reserved by Windows across NTFS, FAT, and ReFS filesystems.
pub const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CLOCK$", "CONIN$", "CONOUT$", "COM1", "COM2", "COM3", "COM4",
    "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
    "LPT8", "LPT9", "COM¹", "COM²", "COM³", "LPT¹", "LPT²", "LPT³",
];

#[inline]
fn is_reserved_name(name: &str) -> bool {
    WINDOWS_RESERVED_NAMES
        .iter()
        .any(|&reserved| reserved.eq_ignore_ascii_case(name))
}

#[inline]
fn is_control_or_format(ch: char) -> bool {
    let code = ch as u32;
    code < 32
        || code == 127
        || matches!(
            ch,
            '\u{00AD}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061C}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

/// Returns the length of `value` when encoded as UTF-16 code units.
#[must_use]
pub fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// Returns the longest prefix of `value` whose UTF-16 length does not exceed `max_units`.
#[must_use]
pub fn truncate_utf16(value: &str, max_units: usize) -> &str {
    let mut units = 0;
    for (idx, ch) in value.char_indices() {
        let len = ch.len_utf16();
        if units + len > max_units {
            return &value[..idx];
        }
        units += len;
    }
    value
}

/// Checks whether `name` is a valid Windows filename ending in `.dem`.
#[must_use]
pub fn is_valid_demo_filename(name: &str) -> bool {
    is_valid_filename(name)
        && name
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("dem"))
}

/// Checks whether `name` is a valid filename component according to Windows NTFS rules.
#[must_use]
pub fn is_valid_filename(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if utf16_len(name) > MAX_FILENAME_LEN {
        return false;
    }
    if name.contains('/') || name.contains('\\') {
        return false;
    }
    if name.chars().any(|ch| INVALID_FILENAME_CHARS.contains(&ch)) {
        return false;
    }
    if name.starts_with(' ') || name.ends_with('.') || name.ends_with(' ') {
        return false;
    }
    if name.chars().any(is_control_or_format) {
        return false;
    }

    let stem = name.split('.').next().unwrap_or("").trim();
    if stem.is_empty() {
        return false;
    }

    !is_reserved_name(stem)
}

/// Checks whether `delim` is a valid filename delimiter component.
#[must_use]
pub fn is_valid_delimiter(delim: &str) -> bool {
    if delim.is_empty() {
        return true;
    }
    if utf16_len(delim) > 32 {
        return false;
    }
    if delim.contains('/') || delim.contains('\\') {
        return false;
    }
    if delim.chars().any(|ch| INVALID_FILENAME_CHARS.contains(&ch)) {
        return false;
    }
    if delim.chars().any(is_control_or_format) {
        return false;
    }
    true
}

/// Sanitizes an arbitrary string into a guaranteed valid demo filename ending in `.dem`.
///
/// Strips directory paths, replaces illegal or reserved Windows characters with underscores,
/// prefixes reserved DOS device names with an underscore (e.g. `CON.dem` becomes `_CON.dem`),
/// ensures a `.dem` extension, and enforces length bounds.
#[must_use]
pub fn sanitize_demo_filename(raw: &str) -> String {
    let trimmed = raw.trim();
    if is_valid_demo_filename(trimmed) {
        return trimmed.to_string();
    }

    // Split directory separators if any leaked in (take the basename)
    let base_start = trimmed.rfind(['\\', '/']).map(|idx| idx + 1).unwrap_or(0);
    let base = &trimmed[base_start..];

    // Determine stem and whether extension is .dem
    let (stem, ext) = if let Some(dot) = base.rfind('.') {
        let ext = &base[dot..];
        if ext.eq_ignore_ascii_case(".dem") {
            (base[..dot].to_string(), ext)
        } else {
            (base.to_string(), ".dem")
        }
    } else {
        (base.to_string(), ".dem")
    };

    // Replace invalid characters in stem
    let mut clean_stem = String::with_capacity(stem.len());
    for ch in stem.chars() {
        if INVALID_FILENAME_CHARS.contains(&ch)
            || ch == '/'
            || ch == '\\'
            || is_control_or_format(ch)
        {
            clean_stem.push('_');
        } else {
            clean_stem.push(ch);
        }
    }

    // Enforce MAX_FILENAME_LEN (255 utf16 units) including ext (".dem" = 4)
    let max_stem_len = MAX_FILENAME_LEN.saturating_sub(utf16_len(ext));
    let truncated =
        truncate_utf16(clean_stem.trim_start(), max_stem_len).trim_end_matches(['.', ' ']);
    let mut stem = if truncated.is_empty() {
        "demo".to_string()
    } else {
        truncated.to_string()
    };

    // Check for Windows reserved names in the primary stem
    let first_part = stem.split('.').next().unwrap_or("").trim();
    if is_reserved_name(first_part) {
        stem.insert(0, '_');
        let re_truncated = truncate_utf16(&stem, max_stem_len).trim_end_matches(['.', ' ']);
        stem = re_truncated.to_string();
    }

    let candidate = format!("{stem}{ext}");
    if is_valid_demo_filename(&candidate) {
        candidate
    } else {
        "demo.dem".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_reserved_name_case_insensitive() {
        assert!(is_reserved_name("con"));
        assert!(is_reserved_name("CON"));
        assert!(is_reserved_name("cOn"));
        assert!(is_reserved_name("prn"));
        assert!(is_reserved_name("aux"));
        assert!(is_reserved_name("nul"));
        assert!(is_reserved_name("com1"));
        assert!(is_reserved_name("COM9"));
        assert!(is_reserved_name("lpt1"));
        assert!(is_reserved_name("LPT9"));
        assert!(is_reserved_name("clock$"));
        assert!(is_reserved_name("conin$"));
        assert!(is_reserved_name("conout$"));
        assert!(is_reserved_name("COM¹"));
        assert!(is_reserved_name("com¹"));

        assert!(!is_reserved_name("con1"));
        assert!(!is_reserved_name("normal"));
        assert!(!is_reserved_name("com10"));
    }

    #[test]
    fn test_truncate_utf16() {
        assert_eq!(truncate_utf16("hello", 10), "hello");
        assert_eq!(truncate_utf16("hello", 5), "hello");
        assert_eq!(truncate_utf16("hello", 3), "hel");
        assert_eq!(truncate_utf16("hello", 0), "");
        assert_eq!(truncate_utf16("", 5), "");

        // Multi-byte UTF-8, single UTF-16 code unit
        assert_eq!(truncate_utf16("café", 4), "café");
        assert_eq!(truncate_utf16("café", 3), "caf");

        // Surrogate pair (🦀 len_utf16 = 2)
        assert_eq!(truncate_utf16("a🦀b", 4), "a🦀b");
        assert_eq!(truncate_utf16("a🦀b", 3), "a🦀");
        assert_eq!(truncate_utf16("a🦀b", 2), "a");
        assert_eq!(truncate_utf16("a🦀b", 1), "a");
        assert_eq!(truncate_utf16("🦀", 1), "");
        assert_eq!(truncate_utf16("🦀", 2), "🦀");
    }

    #[test]
    fn test_sanitize_demo_filename_long_stems() {
        let long_stem = "a".repeat(300);
        let sanitized = sanitize_demo_filename(&format!("{long_stem}.dem"));
        assert_eq!(utf16_len(&sanitized), MAX_FILENAME_LEN);
        assert!(is_valid_demo_filename(&sanitized));
        assert!(sanitized.ends_with(".dem"));

        // Long stem starting with a reserved name
        let con_long = format!("CON.{long_stem}.dem");
        let sanitized_con = sanitize_demo_filename(&con_long);
        assert!(sanitized_con.starts_with("_CON."));
        assert!(utf16_len(&sanitized_con) <= MAX_FILENAME_LEN);
        assert!(is_valid_demo_filename(&sanitized_con));
    }
}

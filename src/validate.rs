use unicode_general_category::{GeneralCategory, get_general_category};

use crate::MAX_FILENAME_LEN;

const INVALID_FILENAME_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM1", "COM2", "COM3", "COM4", "COM5",
    "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
    "LPT9", "COM¹", "COM²", "COM³", "LPT¹", "LPT²", "LPT³",
];

pub fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

pub fn is_valid_demo_filename(name: &str) -> bool {
    is_valid_filename(name)
        && name
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("dem"))
}

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
    for ch in name.chars() {
        let code = ch as u32;
        if code < 32 || code == 127 {
            return false;
        }
        if get_general_category(ch) == GeneralCategory::Format {
            return false;
        }
    }

    let stem = name.split('.').next().unwrap_or("").trim();
    if stem.is_empty() {
        return false;
    }

    let stem = stem.to_uppercase();
    !WINDOWS_RESERVED_NAMES.contains(&stem.as_str())
}

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
    for ch in delim.chars() {
        let code = ch as u32;
        if code < 32 || code == 127 {
            return false;
        }
        if get_general_category(ch) == GeneralCategory::Format {
            return false;
        }
    }
    true
}

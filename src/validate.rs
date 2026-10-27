use std::fmt;
use std::path::Path;
use std::str::FromStr;

use unicode_general_category::{GeneralCategory, get_general_category};

use crate::MAX_FILENAME_LEN;

const INVALID_FILENAME_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

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

    !is_reserved_name(stem)
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

/// An error indicating that a string could not be parsed as a valid [`.dem` filename](DemoFileName).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DemoFileNameError {
    #[error("invalid demo filename: {0}")]
    Invalid(String),
}

/// A validated `.dem` filename that is guaranteed to be safe across all Windows filesystems.
///
/// Cannot be constructed directly with arbitrary strings; must be parsed via [`DemoFileName::parse`]
/// or sanitized via [`DemoFileName::sanitize`]. Guards against reserved Windows DOS device names
/// (`CON`, `PRN`, `AUX`, `NUL`, `COM1-9`, `LPT1-9`, `CLOCK$`, etc.), illegal characters (`:`, `<`, `>`,
/// `"`, `/`, `\`, `|`, `?`, `*`), control characters, formatting characters, and leading/trailing dots or spaces.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DemoFileName(String);

impl DemoFileName {
    /// Parses and validates a raw filename string.
    ///
    /// Trims leading and trailing whitespace. Returns an error if the filename is invalid or reserved.
    pub fn parse(raw: &str) -> Result<Self, DemoFileNameError> {
        let trimmed = raw.trim();
        if is_valid_demo_filename(trimmed) {
            Ok(Self(trimmed.to_string()))
        } else {
            Err(DemoFileNameError::Invalid(raw.to_string()))
        }
    }

    /// Sanitizes an arbitrary string into a guaranteed valid [`DemoFileName`].
    ///
    /// Strips directory paths, replaces illegal or reserved Windows characters with underscores,
    /// prefixes reserved DOS device names with an underscore (e.g. `CON.dem` becomes `_CON.dem`),
    /// ensures a `.dem` extension, and enforces length bounds.
    pub fn sanitize(raw: &str) -> Self {
        let trimmed = raw.trim();
        if is_valid_demo_filename(trimmed) {
            return Self(trimmed.to_string());
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
            let code = ch as u32;
            if INVALID_FILENAME_CHARS.contains(&ch)
                || ch == '/'
                || ch == '\\'
                || code < 32
                || code == 127
                || get_general_category(ch) == GeneralCategory::Format
            {
                clean_stem.push('_');
            } else {
                clean_stem.push(ch);
            }
        }

        // Trim leading spaces, trailing spaces, and trailing dots
        let mut stem = clean_stem.trim().trim_end_matches('.').trim().to_string();

        if stem.is_empty() {
            stem = "demo".to_string();
        }

        // Check for Windows reserved names in the primary stem
        let first_part = stem.split('.').next().unwrap_or("").trim();
        if is_reserved_name(first_part) {
            stem = format!("_{stem}");
        }

        // Enforce MAX_FILENAME_LEN (255 utf16 units) including ext (".dem" = 4)
        let max_stem_len = MAX_FILENAME_LEN.saturating_sub(utf16_len(ext));
        while utf16_len(&stem) > max_stem_len {
            stem.pop();
        }
        stem = stem.trim().trim_end_matches('.').trim().to_string();
        if stem.is_empty() {
            stem = "demo".to_string();
        }

        // Re-check reserved names in case truncation exposed one
        let first_part = stem.split('.').next().unwrap_or("").trim();
        if is_reserved_name(first_part) {
            stem = format!("_{stem}");
            while utf16_len(&stem) > max_stem_len {
                stem.pop();
            }
        }

        let candidate = format!("{stem}{ext}");
        if is_valid_demo_filename(&candidate) {
            Self(candidate)
        } else {
            Self("demo.dem".to_string())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for DemoFileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for DemoFileName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for DemoFileName {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl std::ops::Deref for DemoFileName {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl PartialEq<str> for DemoFileName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for DemoFileName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<DemoFileName> for &str {
    fn eq(&self, other: &DemoFileName) -> bool {
        *self == other.0
    }
}

impl PartialEq<String> for DemoFileName {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

impl PartialEq<DemoFileName> for String {
    fn eq(&self, other: &DemoFileName) -> bool {
        self == &other.0
    }
}

impl FromStr for DemoFileName {
    type Err = DemoFileNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for DemoFileName {
    type Error = DemoFileNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for DemoFileName {
    type Error = DemoFileNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
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
}

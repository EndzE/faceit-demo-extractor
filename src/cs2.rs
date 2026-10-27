//! Counter-Strike 2 installation discovery via Windows Registry keys and filesystem conventions.

use std::path::{Path, PathBuf};

use winreg::RegKey;
use winreg::RegValue;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, RegType};

use crate::win32::expand_env;

/// Registry subkeys and value names checked when querying Counter-Strike 2 install location.
pub const CS2_REG_LOOKUP: &[(&str, &str)] = &[
    (r"SOFTWARE\WOW6432Node\Valve\cs2", "installpath"),
    (
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Steam App 730",
        "InstallLocation",
    ),
    (
        r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Steam App 730",
        "InstallLocation",
    ),
];

/// Relative path components from the CS2 root installation directory to the `csgo` folder.
pub const CS2_CSGO_REL: &[&str] = &["game", "csgo"];

fn decode_reg_string(raw: &RegValue) -> Option<String> {
    match raw.vtype {
        RegType::REG_SZ | RegType::REG_EXPAND_SZ => {}
        _ => return None,
    }

    let iter = raw
        .bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|&unit| unit != 0);

    Some(
        char::decode_utf16(iter)
            .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect(),
    )
}

/// Reads and trims a string registry value under `HKEY_LOCAL_MACHINE` with 64-bit registry redirection.
#[must_use]
pub fn read_reg_string(subkey: &str, value_name: &str) -> Option<String> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm
        .open_subkey_with_flags(subkey, KEY_READ | KEY_WOW64_64KEY)
        .ok()?;
    let raw = key.get_raw_value(value_name).ok()?;
    let mut value = decode_reg_string(&raw)?;

    if raw.vtype == RegType::REG_EXPAND_SZ {
        value = expand_env(&value);
    }

    let cleaned = value.trim().trim_matches('"').trim_end_matches('\0').trim();

    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

fn resolve_csgo_dir(path: &Path) -> PathBuf {
    let nested = path.join("game").join("csgo");
    if nested.is_dir() {
        return nested;
    }

    if path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("csgo"))
    {
        return path.to_path_buf();
    }

    let direct = path.join("csgo");
    if direct.is_dir() {
        return direct;
    }

    nested
}

/// Resolves the CS2 `csgo` folder, evaluating user overrides or probing Windows Registry locations.
///
/// Returns a tuple containing the optional resolved folder path and a diagnostic description of the source.
#[must_use]
pub fn find_cs2_csgo_path(
    override_path: Option<&str>,
    base_dir: &Path,
) -> (Option<PathBuf>, String) {
    if let Some(value) = override_path
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let expanded = expand_env(value.trim_matches('"'));
        let trimmed = Path::new(expanded.trim());
        let resolved = if trimmed.is_absolute() {
            trimmed.to_path_buf()
        } else {
            base_dir.join(trimmed)
        };
        let candidate = resolve_csgo_dir(&resolved);

        if candidate.is_dir() {
            return (
                Some(candidate),
                format!("config.json -> csgo_path = {value}"),
            );
        }

        return (
            None,
            format!(
                "config.json -> csgo_path = {value} points to a missing folder ({})",
                candidate.display()
            ),
        );
    }

    let mut attempts: Vec<String> = Vec::new();

    for &(subkey, value_name) in CS2_REG_LOOKUP {
        let Some(raw) = read_reg_string(subkey, value_name) else {
            attempts.push(format!("HKLM\\{subkey} -> value {value_name} not present"));
            continue;
        };

        let trimmed = raw.trim_end_matches(['\\', '/']);
        let csgo = Path::new(trimmed).join("game").join("csgo");

        if csgo.is_dir() {
            return (Some(csgo), format!("HKLM\\{subkey} -> {value_name}"));
        }

        attempts.push(format!(
            "HKLM\\{subkey} -> {value_name} = {raw} points to a missing folder ({})",
            csgo.display()
        ));
    }

    (None, attempts.join("; "))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    #[test]
    fn test_decode_reg_string() {
        let value = RegValue {
            vtype: RegType::REG_SZ,
            bytes: Cow::Borrowed(&[b'C', 0, b':', 0, b'\\', 0, b'C', 0, b'S', 0, 0, 0]),
        };
        assert_eq!(decode_reg_string(&value), Some(r"C:\CS".to_string()));

        let invalid_type = RegValue {
            vtype: RegType::REG_DWORD,
            bytes: Cow::Borrowed(&[0, 0, 0, 0]),
        };
        assert_eq!(decode_reg_string(&invalid_type), None);

        let empty = RegValue {
            vtype: RegType::REG_SZ,
            bytes: Cow::Borrowed(&[]),
        };
        assert_eq!(decode_reg_string(&empty), Some(String::new()));
    }
}

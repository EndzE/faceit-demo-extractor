use std::path::{Path, PathBuf};

use winreg::RegKey;
use winreg::RegValue;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, RegType};

use crate::output::py_repr_str;
use crate::win32::expand_env;

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

pub const CS2_CSGO_REL: &[&str] = &["game", "csgo"];

fn decode_reg_string(raw: &RegValue) -> Option<String> {
    match raw.vtype {
        RegType::REG_SZ | RegType::REG_EXPAND_SZ => {}
        _ => return None,
    }

    let units: Vec<u16> = raw
        .bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|&unit| unit != 0)
        .collect();

    Some(String::from_utf16_lossy(&units))
}

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

pub fn find_cs2_csgo_path() -> (Option<PathBuf>, String) {
    let mut attempts: Vec<String> = Vec::new();

    for &(subkey, value_name) in CS2_REG_LOOKUP {
        let Some(raw) = read_reg_string(subkey, value_name) else {
            attempts.push(format!(
                "HKLM\\{subkey} -> value {} not present",
                py_repr_str(value_name)
            ));
            continue;
        };

        let trimmed = raw.trim_end_matches(['\\', '/']);
        let csgo = Path::new(trimmed).join("game").join("csgo");

        if csgo.is_dir() {
            return (Some(csgo), format!("HKLM\\{subkey} -> {value_name}"));
        }

        attempts.push(format!(
            "HKLM\\{subkey} -> {} points to a missing folder ({})",
            py_repr_str(&raw),
            csgo.display()
        ));
    }

    (None, attempts.join("; "))
}

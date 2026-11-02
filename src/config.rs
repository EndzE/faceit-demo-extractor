//! Application configuration management, JSON serialization, and default settings.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::output::{self, warn};
use crate::validate::{is_valid_delimiter, is_valid_demo_filename};
use crate::{
    DEFAULT_DELIMITER, DEFAULT_KEEP_ZST, DEFAULT_MOVE_TO_RECYCLE_BIN,
    DEFAULT_NOTIFICATIONS_INSTEAD_OF_CONSOLE, DEFAULT_OVERWRITE_SINGLE_FILE_EXTRACTION,
    DEFAULT_RENAME_DEMO, DEFAULT_TARGET_FILENAME,
};

/// Application configuration settings persisted in `config.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Whether extracted demo files are renamed according to [`Config::target_filename`].
    #[serde(deserialize_with = "de_bool_true")]
    pub rename_demo: bool,
    /// Destination filename when renaming extracted demos (e.g. `1.dem`).
    #[serde(deserialize_with = "de_target_filename")]
    pub target_filename: String,
    /// Delimiter string used when suffixing filenames on conflict (e.g. `_`).
    #[serde(deserialize_with = "de_delimiter")]
    pub delimiter: String,
    /// Whether original archive files (`.zst` or `.bz2`) are kept after extraction.
    #[serde(deserialize_with = "de_bool_false")]
    pub keep_zst: bool,
    /// Whether deleted archives are sent to the Windows Recycle Bin instead of permanently deleted.
    #[serde(deserialize_with = "de_bool_false")]
    pub move_to_recycle_bin: bool,
    /// Whether single-file extraction overwrites an existing destination file without suffixing.
    #[serde(deserialize_with = "de_bool_true")]
    pub overwrite_single_file_extraction: bool,
    /// Whether background workers surface desktop toast notifications instead of console windows.
    #[serde(deserialize_with = "de_bool_false")]
    pub notifications_instead_of_console: bool,
    /// Custom path override for the CS2 `csgo` folder.
    #[serde(deserialize_with = "de_csgo_path")]
    pub csgo_path: Option<String>,
}

fn de_bool_true<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Value>::deserialize(deserializer)?
        .and_then(|v| v.as_bool())
        .unwrap_or(true))
}

fn de_bool_false<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Value>::deserialize(deserializer)?
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

fn de_target_filename<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<Value>::deserialize(deserializer)?;
    let valid = match opt {
        Some(Value::String(name)) => {
            let trimmed = name.trim();
            if is_valid_demo_filename(trimmed) {
                Some(trimmed.to_string())
            } else {
                None
            }
        }
        _ => None,
    };
    Ok(valid.unwrap_or_else(|| DEFAULT_TARGET_FILENAME.to_string()))
}

fn de_delimiter<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<Value>::deserialize(deserializer)?;
    let valid = match opt {
        Some(Value::String(delim)) => {
            let trimmed = delim.trim();
            if is_valid_delimiter(trimmed) {
                Some(trimmed.to_string())
            } else {
                None
            }
        }
        _ => None,
    };
    Ok(valid.unwrap_or_else(|| DEFAULT_DELIMITER.to_string()))
}

fn de_csgo_path<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<Value>::deserialize(deserializer)?;
    Ok(match opt {
        Some(Value::String(path)) if !path.trim().is_empty() => {
            Some(path.trim().trim_matches('"').to_string())
        }
        _ => None,
    })
}

impl Default for Config {
    fn default() -> Self {
        Self::defaults()
    }
}

impl Config {
    /// Returns a new [`Config`] populated with default values.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            rename_demo: DEFAULT_RENAME_DEMO,
            target_filename: DEFAULT_TARGET_FILENAME.to_string(),
            delimiter: DEFAULT_DELIMITER.to_string(),
            keep_zst: DEFAULT_KEEP_ZST,
            move_to_recycle_bin: DEFAULT_MOVE_TO_RECYCLE_BIN,
            overwrite_single_file_extraction: DEFAULT_OVERWRITE_SINGLE_FILE_EXTRACTION,
            notifications_instead_of_console: DEFAULT_NOTIFICATIONS_INSTEAD_OF_CONSOLE,
            csgo_path: None,
        }
    }
}

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

fn parse_config_json(bytes: &[u8]) -> Result<Map<String, Value>, String> {
    match serde_json::from_slice::<Value>(strip_bom(bytes)) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("config.json does not contain a JSON object".to_string()),
        Err(err) => Err(format!("config.json is not valid JSON ({err})")),
    }
}

/// Loads application configuration from `path`, returning default values if the file is missing or invalid.
#[must_use]
pub fn load_config(path: &Path) -> Config {
    let Ok(bytes) = fs::read(path) else {
        return Config::defaults();
    };
    serde_json::from_slice::<Config>(strip_bom(&bytes)).unwrap_or_default()
}

fn preserve_invalid_config(path: &Path) -> Result<PathBuf, String> {
    let mut backup_name = path.as_os_str().to_os_string();
    backup_name.push(".bak");
    let backup = PathBuf::from(backup_name);

    if backup.exists() {
        fs::remove_file(&backup)
            .map_err(|err| format!("could not replace {}: {err}", backup.display()))?;
    }
    fs::rename(path, &backup).map_err(|err| {
        format!(
            "could not rename {} to {}: {err}",
            path.display(),
            backup.display()
        )
    })?;

    Ok(backup)
}

fn write_config_atomic(path: &Path, value: &Value) -> Result<(), String> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".config-")
        .suffix(".tmp")
        .tempfile_in(directory)
        .map_err(|err| err.to_string())?;

    serde_json::to_writer_pretty(&mut temp, value).map_err(|err| err.to_string())?;
    temp.write_all(b"\n").map_err(|err| err.to_string())?;
    temp.flush().map_err(|err| err.to_string())?;
    temp.as_file().sync_all().map_err(|err| err.to_string())?;
    temp.persist(path).map_err(|err| err.error.to_string())?;

    Ok(())
}

/// Merges the provided `updates` into the configuration file at `path`, writing atomically via a temporary file.
///
/// # Errors
///
/// Returns an error message if reading the existing config fails, serialization fails, or writing fails.
pub fn save_config(path: &Path, updates: &[(&str, Value)]) -> Result<(), String> {
    let mut merged = match fs::read(path) {
        Ok(bytes) => parse_config_json(&bytes)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Map::new(),
        Err(err) => return Err(format!("could not read config.json: {err}")),
    };
    for (key, value) in updates {
        merged.insert((*key).to_string(), value.clone());
    }

    write_config_atomic(path, &Value::Object(merged))
}

/// Ensures the configuration file exists at `path`, initializing it with defaults or filling in missing keys.
///
/// # Errors
///
/// Returns an error message if saving default or updated settings fails.
pub fn ensure_config_file(path: &Path, announce: bool) -> Result<(), String> {
    match fs::read(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let defaults =
                serde_json::to_value(Config::defaults()).map_err(|err| err.to_string())?;
            write_config_atomic(path, &defaults)?;
            if announce {
                println!(
                    "{}",
                    output::field(
                        "Config",
                        output::good(format!("created {}", path.display()))
                    )
                );
            }
            Ok(())
        }
        Err(err) => {
            warn(format!(
                "could not read config.json ({err}); using built-in defaults"
            ));
            Ok(())
        }
        Ok(bytes) => match parse_config_json(&bytes) {
            Err(problem) => {
                let backup = preserve_invalid_config(path).map_err(|err| {
                    format!(
                        "{problem}; refusing to overwrite it because a backup could not be created: {err}"
                    )
                })?;
                warn(format!(
                    "{problem}; moved the original to {} and reset it to defaults",
                    backup.display()
                ));
                let defaults =
                    serde_json::to_value(Config::defaults()).map_err(|err| err.to_string())?;
                write_config_atomic(path, &defaults)
            }
            Ok(mut data) => {
                let default_val =
                    serde_json::to_value(Config::defaults()).map_err(|err| err.to_string())?;
                let Value::Object(default_map) = default_val else {
                    unreachable!()
                };

                let missing_keys: Vec<String> = default_map
                    .keys()
                    .filter(|key| !data.contains_key(*key))
                    .cloned()
                    .collect();

                if missing_keys.is_empty() {
                    return Ok(());
                }

                for key in &missing_keys {
                    if let Some(val) = default_map.get(key) {
                        data.insert(key.clone(), val.clone());
                    }
                }

                write_config_atomic(path, &Value::Object(data))?;
                if announce {
                    println!(
                        "{}",
                        output::field(
                            "Config",
                            format!(
                                "added missing setting(s): {}",
                                output::strong(missing_keys.join(", "))
                            )
                        )
                    );
                }
                Ok(())
            }
        },
    }
}

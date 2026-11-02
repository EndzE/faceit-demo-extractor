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
    pub rename_demo: bool,
    /// Destination filename when renaming extracted demos (e.g. `1.dem`).
    pub target_filename: String,
    /// Delimiter string used when suffixing filenames on conflict (e.g. `_`).
    pub delimiter: String,
    /// Whether original archive files (`.zst` or `.bz2`) are kept after extraction.
    pub keep_zst: bool,
    /// Whether deleted archives are sent to the Windows Recycle Bin instead of permanently deleted.
    pub move_to_recycle_bin: bool,
    /// Whether single-file extraction overwrites an existing destination file without suffixing.
    pub overwrite_single_file_extraction: bool,
    /// Whether background workers surface desktop toast notifications instead of console windows.
    pub notifications_instead_of_console: bool,
    /// Custom path override for the CS2 `csgo` folder.
    pub csgo_path: Option<String>,
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

    /// Sanitizes and validates configuration fields, falling back to defaults for invalid values.
    #[must_use]
    pub fn sanitize(mut self) -> Self {
        let name = self.target_filename.trim();
        self.target_filename = if is_valid_demo_filename(name) {
            name.to_string()
        } else {
            DEFAULT_TARGET_FILENAME.to_string()
        };

        let delim = self.delimiter.trim();
        self.delimiter = if is_valid_delimiter(delim) {
            delim.to_string()
        } else {
            DEFAULT_DELIMITER.to_string()
        };

        self.csgo_path = self
            .csgo_path
            .as_deref()
            .map(|p| p.trim().trim_matches('"').trim())
            .filter(|p| !p.is_empty())
            .map(str::to_string);

        self
    }
}

fn sanitize_map(data: &mut Map<String, Value>) {
    let check_bool = |key: &str, default: bool, map: &mut Map<String, Value>| {
        if let Some(val) = map.get(key)
            && !val.is_boolean()
        {
            warn(format!(
                "config.json: invalid value for \"{key}\" (expected boolean, found {val}); using default {default}"
            ));
            map.insert(key.to_string(), Value::Bool(default));
        }
    };

    let check_string = |key: &str, default: &str, map: &mut Map<String, Value>| {
        if let Some(val) = map.get(key)
            && !val.is_string()
        {
            warn(format!(
                "config.json: invalid value for \"{key}\" (expected string, found {val}); using default \"{default}\""
            ));
            map.insert(key.to_string(), Value::String(default.to_string()));
        }
    };

    check_bool("rename_demo", DEFAULT_RENAME_DEMO, data);
    check_string("target_filename", DEFAULT_TARGET_FILENAME, data);
    check_string("delimiter", DEFAULT_DELIMITER, data);
    check_bool("keep_zst", DEFAULT_KEEP_ZST, data);
    check_bool("move_to_recycle_bin", DEFAULT_MOVE_TO_RECYCLE_BIN, data);
    check_bool(
        "overwrite_single_file_extraction",
        DEFAULT_OVERWRITE_SINGLE_FILE_EXTRACTION,
        data,
    );
    check_bool(
        "notifications_instead_of_console",
        DEFAULT_NOTIFICATIONS_INSTEAD_OF_CONSOLE,
        data,
    );

    if let Some(val) = data.get("csgo_path")
        && !val.is_string()
        && !val.is_null()
    {
        warn(format!(
            "config.json: invalid value for \"csgo_path\" (expected string or null, found {val}); ignoring"
        ));
        data.insert("csgo_path".to_string(), Value::Null);
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
    let Ok(mut map) = parse_config_json(&bytes) else {
        return Config::defaults();
    };
    sanitize_map(&mut map);
    serde_json::from_value::<Config>(Value::Object(map))
        .map(Config::sanitize)
        .unwrap_or_default()
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

/// Saves `config` into the configuration file at `path`, merging with existing keys and writing atomically.
///
/// # Errors
///
/// Returns an error message if reading the existing config fails, serialization fails, or writing fails.
pub fn save_config(path: &Path, config: &Config) -> Result<(), String> {
    let mut merged = match fs::read(path) {
        Ok(bytes) => parse_config_json(&bytes)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Map::new(),
        Err(err) => return Err(format!("could not read config.json: {err}")),
    };
    if let Ok(Value::Object(updates)) = serde_json::to_value(config) {
        merged.extend(updates);
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

//! Application configuration management, JSON serialization, and default settings.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::output::{self, warn};
use crate::validate::{DemoFileName, is_valid_delimiter};
use crate::{
    DEFAULT_DELIMITER, DEFAULT_KEEP_ZST, DEFAULT_MOVE_TO_RECYCLE_BIN,
    DEFAULT_NOTIFICATIONS_INSTEAD_OF_CONSOLE, DEFAULT_OVERWRITE_SINGLE_FILE_EXTRACTION,
    DEFAULT_RENAME_DEMO, DEFAULT_TARGET_FILENAME,
};

static CONFIG_WARNINGS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Application configuration settings persisted in `config.json`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, from = "RawConfig")]
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

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    rename_demo: Option<Value>,
    target_filename: Option<Value>,
    delimiter: Option<Value>,
    keep_zst: Option<Value>,
    move_to_recycle_bin: Option<Value>,
    overwrite_single_file_extraction: Option<Value>,
    notifications_instead_of_console: Option<Value>,
    csgo_path: Option<Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self::defaults()
    }
}

impl From<RawConfig> for Config {
    fn from(raw: RawConfig) -> Self {
        let target_filename = match raw.target_filename {
            Some(Value::String(name)) => {
                if let Ok(valid) = DemoFileName::parse(&name) {
                    valid.into_inner()
                } else {
                    let val = Value::String(name);
                    warn_config_once(
                        "target_filename",
                        format!(
                            "invalid \"target_filename\" value {val} in config.json; it must be a single .dem filename; using the default {DEFAULT_TARGET_FILENAME}"
                        ),
                    );
                    DEFAULT_TARGET_FILENAME.to_string()
                }
            }
            Some(val) => {
                warn_config_once(
                    "target_filename",
                    format!(
                        "invalid \"target_filename\" value {val} in config.json; it must be a single .dem filename; using the default {DEFAULT_TARGET_FILENAME}"
                    ),
                );
                DEFAULT_TARGET_FILENAME.to_string()
            }
            None => DEFAULT_TARGET_FILENAME.to_string(),
        };

        let delimiter = match raw.delimiter {
            Some(Value::String(delim)) => {
                let trimmed = delim.trim();
                if is_valid_delimiter(trimmed) {
                    trimmed.to_string()
                } else {
                    let val = Value::String(delim);
                    warn_config_once(
                        "delimiter",
                        format!(
                            "invalid \"delimiter\" value {val} in config.json; using the default \"{DEFAULT_DELIMITER}\""
                        ),
                    );
                    DEFAULT_DELIMITER.to_string()
                }
            }
            Some(val) => {
                warn_config_once(
                    "delimiter",
                    format!(
                        "invalid \"delimiter\" value {val} in config.json; using the default \"{DEFAULT_DELIMITER}\""
                    ),
                );
                DEFAULT_DELIMITER.to_string()
            }
            None => DEFAULT_DELIMITER.to_string(),
        };

        let keep_zst = coerce_bool_val(raw.keep_zst, "keep_zst", DEFAULT_KEEP_ZST);
        let move_to_recycle_bin = coerce_bool_val(
            raw.move_to_recycle_bin,
            "move_to_recycle_bin",
            DEFAULT_MOVE_TO_RECYCLE_BIN,
        );
        let overwrite_single_file_extraction = coerce_bool_val(
            raw.overwrite_single_file_extraction,
            "overwrite_single_file_extraction",
            DEFAULT_OVERWRITE_SINGLE_FILE_EXTRACTION,
        );
        let rename_demo = coerce_bool_val(raw.rename_demo, "rename_demo", DEFAULT_RENAME_DEMO);
        let notifications_instead_of_console = coerce_bool_val(
            raw.notifications_instead_of_console,
            "notifications_instead_of_console",
            DEFAULT_NOTIFICATIONS_INSTEAD_OF_CONSOLE,
        );

        let csgo_path = match raw.csgo_path {
            Some(Value::String(path)) if !path.trim().is_empty() => {
                Some(path.trim().trim_matches('"').to_string())
            }
            Some(Value::String(_)) | None | Some(Value::Null) => None,
            Some(other) => {
                warn_config_once(
                    "csgo_path",
                    format!("invalid \"csgo_path\" value {other} in config.json; ignoring it"),
                );
                None
            }
        };

        Self {
            rename_demo,
            target_filename,
            delimiter,
            keep_zst,
            move_to_recycle_bin,
            overwrite_single_file_extraction,
            notifications_instead_of_console,
            csgo_path,
        }
    }
}

fn coerce_bool_val(val: Option<Value>, key: &str, default: bool) -> bool {
    match val {
        Some(Value::Bool(b)) => b,
        Some(other) => {
            warn_config_once(
                key,
                format!(
                    "invalid \"{key}\" value {other} in config.json; using the default {default}"
                ),
            );
            default
        }
        None => default,
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

    /// Converts this [`Config`] into key-value pairs suitable for merging into a JSON object.
    #[must_use]
    pub fn updates(&self) -> Vec<(&'static str, Value)> {
        vec![
            ("rename_demo", Value::Bool(self.rename_demo)),
            (
                "target_filename",
                Value::String(self.target_filename.clone()),
            ),
            ("delimiter", Value::String(self.delimiter.clone())),
            ("keep_zst", Value::Bool(self.keep_zst)),
            ("move_to_recycle_bin", Value::Bool(self.move_to_recycle_bin)),
            (
                "overwrite_single_file_extraction",
                Value::Bool(self.overwrite_single_file_extraction),
            ),
            (
                "notifications_instead_of_console",
                Value::Bool(self.notifications_instead_of_console),
            ),
            (
                "csgo_path",
                self.csgo_path
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            ),
        ]
    }
}

fn warn_config_once(key: &str, message: impl AsRef<str>) {
    let warnings = CONFIG_WARNINGS.get_or_init(|| Mutex::new(HashSet::new()));
    let inserted = warnings
        .lock()
        .map(|mut set| set.insert(key.to_string()))
        .unwrap_or(false);
    if inserted {
        warn(message);
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

/// Reads and parses the raw JSON map from the config file, returning an empty map on failure.
#[must_use]
pub fn read_config_json(path: &Path) -> Map<String, Value> {
    let Ok(bytes) = fs::read(path) else {
        return Map::new();
    };
    parse_config_json(&bytes).unwrap_or_default()
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

    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".config-")
        .suffix(".tmp")
        .tempfile_in(directory)
        .map_err(|err| err.to_string())?;

    serde_json::to_writer_pretty(&mut temp, &Value::Object(merged))
        .map_err(|err| err.to_string())?;
    temp.write_all(b"\n").map_err(|err| err.to_string())?;
    temp.flush().map_err(|err| err.to_string())?;
    temp.as_file().sync_all().map_err(|err| err.to_string())?;
    temp.persist(path).map_err(|err| err.error.to_string())?;

    Ok(())
}

/// Ensures the configuration file exists at `path`, initializing it with defaults or filling in missing keys.
///
/// # Errors
///
/// Returns an error message if saving default or updated settings fails.
pub fn ensure_config_file(path: &Path, announce: bool) -> Result<(), String> {
    match fs::read(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            save_config(path, &Config::defaults().updates())?;
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
                save_config(path, &Config::defaults().updates())
            }
            Ok(data) => {
                let updates: Vec<(&str, Value)> = Config::defaults()
                    .updates()
                    .into_iter()
                    .filter(|(key, _)| !data.contains_key(*key))
                    .collect();

                if updates.is_empty() {
                    return Ok(());
                }

                save_config(path, &updates)?;
                if announce {
                    let missing: Vec<&str> = updates.iter().map(|(key, _)| *key).collect();
                    println!(
                        "{}",
                        output::field(
                            "Config",
                            format!(
                                "added missing setting(s): {}",
                                output::strong(missing.join(", "))
                            )
                        )
                    );
                }
                Ok(())
            }
        },
    }
}

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};

use crate::output::{self, warn};
use crate::validate::{DemoFileName, is_valid_delimiter};
use crate::{
    DEFAULT_DELIMITER, DEFAULT_KEEP_ZST, DEFAULT_MOVE_TO_RECYCLE_BIN,
    DEFAULT_NOTIFICATIONS_INSTEAD_OF_CONSOLE, DEFAULT_OVERWRITE_SINGLE_FILE_EXTRACTION,
    DEFAULT_RENAME_DEMO, DEFAULT_TARGET_FILENAME,
};

static CONFIG_WARNINGS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub rename_demo: bool,
    pub target_filename: String,
    pub delimiter: String,
    pub keep_zst: bool,
    pub move_to_recycle_bin: bool,
    pub overwrite_single_file_extraction: bool,
    pub notifications_instead_of_console: bool,
    pub csgo_path: Option<String>,
}

impl Config {
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

pub fn read_config_json(path: &Path) -> Map<String, Value> {
    let Ok(bytes) = fs::read(path) else {
        return Map::new();
    };
    parse_config_json(&bytes).unwrap_or_default()
}

pub fn load_config(path: &Path) -> Config {
    let data = read_config_json(path);
    let mut config = Config::defaults();

    let name_value = data
        .get("target_filename")
        .cloned()
        .unwrap_or_else(|| Value::String(DEFAULT_TARGET_FILENAME.to_string()));

    match &name_value {
        Value::String(name) => {
            if let Ok(valid) = DemoFileName::parse(name) {
                config.target_filename = valid.into_inner();
            } else {
                warn_config_once(
                    "target_filename",
                    format!(
                        "invalid \"target_filename\" value {name_value} in config.json; it must be a single .dem filename; using the default {DEFAULT_TARGET_FILENAME}"
                    ),
                );
            }
        }
        _ => {
            warn_config_once(
                "target_filename",
                format!(
                    "invalid \"target_filename\" value {name_value} in config.json; it must be a single .dem filename; using the default {DEFAULT_TARGET_FILENAME}"
                ),
            );
        }
    }

    let delim_value = data
        .get("delimiter")
        .cloned()
        .unwrap_or_else(|| Value::String(DEFAULT_DELIMITER.to_string()));

    match &delim_value {
        Value::String(delim) => {
            let trimmed = delim.trim();
            if is_valid_delimiter(trimmed) {
                config.delimiter = trimmed.to_string();
            } else {
                warn_config_once(
                    "delimiter",
                    format!(
                        "invalid \"delimiter\" value {delim_value} in config.json; using the default \"{DEFAULT_DELIMITER}\""
                    ),
                );
            }
        }
        _ => {
            warn_config_once(
                "delimiter",
                format!(
                    "invalid \"delimiter\" value {delim_value} in config.json; using the default \"{DEFAULT_DELIMITER}\""
                ),
            );
        }
    }

    config.keep_zst = coerce_bool(&data, "keep_zst", DEFAULT_KEEP_ZST);
    config.move_to_recycle_bin =
        coerce_bool(&data, "move_to_recycle_bin", DEFAULT_MOVE_TO_RECYCLE_BIN);
    config.overwrite_single_file_extraction = coerce_bool(
        &data,
        "overwrite_single_file_extraction",
        DEFAULT_OVERWRITE_SINGLE_FILE_EXTRACTION,
    );
    config.rename_demo = coerce_bool(&data, "rename_demo", DEFAULT_RENAME_DEMO);
    config.notifications_instead_of_console = coerce_bool(
        &data,
        "notifications_instead_of_console",
        DEFAULT_NOTIFICATIONS_INSTEAD_OF_CONSOLE,
    );
    config.csgo_path = match data.get("csgo_path") {
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

    config
}

fn coerce_bool(data: &Map<String, Value>, key: &str, default: bool) -> bool {
    let value = data.get(key).cloned().unwrap_or(Value::Bool(default));
    match value {
        Value::Bool(value) => value,
        other => {
            warn_config_once(
                key,
                format!(
                    "invalid \"{key}\" value {other} in config.json; using the default {default}"
                ),
            );
            default
        }
    }
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

pub fn save_config(path: &Path, updates: &[(&str, Value)]) -> Result<(), String> {
    let mut merged = match fs::read(path) {
        Ok(bytes) => parse_config_json(&bytes)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Map::new(),
        Err(err) => return Err(format!("could not read config.json: {err}")),
    };
    for (key, value) in updates {
        merged.insert((*key).to_string(), value.clone());
    }

    let body =
        serde_json::to_string_pretty(&Value::Object(merged)).map_err(|err| err.to_string())?;
    let text = format!("{body}\n");

    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".config-")
        .suffix(".tmp")
        .tempfile_in(directory)
        .map_err(|err| err.to_string())?;

    temp.write_all(text.as_bytes())
        .map_err(|err| err.to_string())?;
    temp.flush().map_err(|err| err.to_string())?;
    temp.as_file().sync_all().map_err(|err| err.to_string())?;
    temp.persist(path).map_err(|err| err.error.to_string())?;

    Ok(())
}

pub fn ensure_config_file(path: &Path, announce: bool) -> (bool, Option<String>) {
    match fs::read(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            match save_config(path, &Config::defaults().updates()) {
                Ok(()) => {
                    if announce {
                        println!(
                            "{}",
                            output::field(
                                "Config",
                                output::good(format!("created {}", path.display()))
                            )
                        );
                    }
                    (true, None)
                }
                Err(message) => (false, Some(message)),
            }
        }
        Err(err) => {
            warn(format!(
                "could not read config.json ({err}); using built-in defaults"
            ));
            (true, None)
        }
        Ok(bytes) => match parse_config_json(&bytes) {
            Err(problem) => {
                let backup = match preserve_invalid_config(path) {
                    Ok(backup) => backup,
                    Err(err) => {
                        return (
                            false,
                            Some(format!(
                                "{problem}; refusing to overwrite it because a backup could not be created: {err}"
                            )),
                        );
                    }
                };
                warn(format!(
                    "{problem}; moved the original to {} and reset it to defaults",
                    backup.display()
                ));
                match save_config(path, &Config::defaults().updates()) {
                    Ok(()) => (true, None),
                    Err(message) => (false, Some(message)),
                }
            }
            Ok(data) => {
                let defaults = Config::defaults().updates();
                let missing: Vec<&str> = defaults
                    .iter()
                    .filter(|(key, _)| !data.contains_key(*key))
                    .map(|(key, _)| *key)
                    .collect();

                if missing.is_empty() {
                    return (true, None);
                }

                let updates: Vec<(&str, Value)> = defaults
                    .into_iter()
                    .filter(|(key, _)| missing.contains(key))
                    .collect();

                match save_config(path, &updates) {
                    Ok(()) => {
                        if announce {
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
                        (true, None)
                    }
                    Err(message) => (false, Some(message)),
                }
            }
        },
    }
}

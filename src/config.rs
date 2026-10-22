use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};

use crate::output::{self, warn};
use crate::validate::is_valid_filename;
use crate::{DEFAULT_KEEP_ZST, DEFAULT_MOVE_TO_RECYCLE_BIN, DEFAULT_TARGET_FILENAME};

static CONFIG_WARNINGS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub target_filename: String,
    pub keep_zst: bool,
    pub move_to_recycle_bin: bool,
    pub csgo_path: Option<String>,
}

impl Config {
    pub fn defaults() -> Self {
        Self {
            target_filename: DEFAULT_TARGET_FILENAME.to_string(),
            keep_zst: DEFAULT_KEEP_ZST,
            move_to_recycle_bin: DEFAULT_MOVE_TO_RECYCLE_BIN,
            csgo_path: None,
        }
    }

    pub fn updates(&self) -> Vec<(&'static str, Value)> {
        vec![
            (
                "target_filename",
                Value::String(self.target_filename.clone()),
            ),
            ("keep_zst", Value::Bool(self.keep_zst)),
            ("move_to_recycle_bin", Value::Bool(self.move_to_recycle_bin)),
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

pub fn read_config_json(path: &Path) -> Map<String, Value> {
    let Ok(bytes) = fs::read(path) else {
        return Map::new();
    };
    match serde_json::from_slice::<Value>(strip_bom(&bytes)) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

pub fn load_config(path: &Path) -> Config {
    let data = read_config_json(path);
    let mut config = Config::defaults();

    let name_value = data
        .get("target_filename")
        .cloned()
        .unwrap_or_else(|| Value::String(DEFAULT_TARGET_FILENAME.to_string()));

    match &name_value {
        Value::String(name) if is_valid_filename(name) => {
            config.target_filename = name.clone();
        }
        _ => {
            warn_config_once(
                "target_filename",
                format!(
                    "invalid \"target_filename\" value {name_value} in config.json; using the default {DEFAULT_TARGET_FILENAME}"
                ),
            );
        }
    }

    config.keep_zst = coerce_bool(&data, "keep_zst", DEFAULT_KEEP_ZST);
    config.move_to_recycle_bin =
        coerce_bool(&data, "move_to_recycle_bin", DEFAULT_MOVE_TO_RECYCLE_BIN);
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

pub fn save_config(path: &Path, updates: &[(&str, Value)]) -> Result<(), String> {
    let mut merged = read_config_json(path);
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
    let _ = temp.as_file().sync_all();
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
        Ok(bytes) => match serde_json::from_slice::<Value>(strip_bom(&bytes)) {
            Err(err) => {
                warn(format!(
                    "config.json is not valid JSON ({err}); resetting it to defaults"
                ));
                match save_config(path, &Config::defaults().updates()) {
                    Ok(()) => (true, None),
                    Err(message) => (false, Some(message)),
                }
            }
            Ok(Value::Object(data)) => {
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
            Ok(_) => {
                warn("config.json does not contain a JSON object; resetting it to defaults");
                match save_config(path, &Config::defaults().updates()) {
                    Ok(()) => (true, None),
                    Err(message) => (false, Some(message)),
                }
            }
        },
    }
}

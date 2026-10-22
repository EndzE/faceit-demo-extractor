#[cfg(not(windows))]
compile_error!("faceit-dem is Windows-only.");

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, IsTerminal, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use unicode_general_category::{GeneralCategory, get_general_category};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::UI::Shell::{FO_DELETE, SHFILEOPSTRUCTW, SHFileOperationW};
use windows::core::{BOOL, PCWSTR};
use winreg::RegKey;
use winreg::RegValue;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, RegType};

const APP_NAME: &str = "FACEIT Demo Extractor";
const DEFAULT_TARGET_FILENAME: &str = "1.dem";
const DEFAULT_KEEP_ZST: bool = false;
const DEFAULT_MOVE_TO_RECYCLE_BIN: bool = false;

const DEMO_MAGIC: &[u8] = b"PBDEMS2\x00";
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const CONTENT_CHECKSUM_FLAG: u8 = 0x04;
const MAX_FILENAME_LEN: usize = 255;

const CS2_REG_LOOKUP: &[(&str, &str)] = &[
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

const CTX_KEY: &str = r"Software\Classes\SystemFileAssociations\.zst\shell\FaceitExtract";
const CTX_COMMAND_KEY: &str =
    r"Software\Classes\SystemFileAssociations\.zst\shell\FaceitExtract\command";

const INVALID_FILENAME_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

const COPY_BUFFER: usize = 1024 * 1024;
const FOF_FLAGS: u16 = 0x0004 | 0x0010 | 0x0040 | 0x0400;

const HELP_FLAGS: &[&str] = &["-h", "--help"];
const STUB_FLAGS: &[&str] = &[
    "--context-menu",
    "--queue-worker",
    "--install",
    "--remove",
    "--update-label",
];

static CANCEL: AtomicBool = AtomicBool::new(false);
static EXTRACTING: AtomicBool = AtomicBool::new(false);
static CONFIG_WARNINGS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

struct Paths {
    exe: PathBuf,
    config: PathBuf,
}

impl Paths {
    fn discover() -> Self {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("faceit-dem.exe"));
        let exe_dir = exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let config = exe_dir.join("config.json");
        Self { exe, config }
    }
}

#[derive(Clone)]
struct Config {
    target_filename: String,
    keep_zst: bool,
    move_to_recycle_bin: bool,
}

impl Config {
    fn defaults() -> Self {
        Self {
            target_filename: DEFAULT_TARGET_FILENAME.to_string(),
            keep_zst: DEFAULT_KEEP_ZST,
            move_to_recycle_bin: DEFAULT_MOVE_TO_RECYCLE_BIN,
        }
    }
}

enum ExtractResult {
    Ok,
    Failed,
    Cancelled,
}

enum CopyError {
    Io(io::Error),
    Zstd(String),
    Magic,
    Cancelled,
}

fn split_lines(message: &str) -> Vec<&str> {
    message
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

fn print_prefixed(label: &str, message: &str, stderr: bool) {
    let lines = split_lines(message);
    for (index, line) in lines.iter().enumerate() {
        let text = if index == 0 {
            format!("{label} {line}")
        } else {
            format!("{}{line}", " ".repeat(label.len() + 1))
        };
        if stderr {
            eprintln!("{text}");
        } else {
            println!("{text}");
        }
    }
}

fn warn(message: impl AsRef<str>) {
    print_prefixed("WARNING:", message.as_ref(), true);
}

fn error(message: impl AsRef<str>) {
    print_prefixed("ERROR:", message.as_ref(), true);
}

fn pause(message: &str) {
    if !io::stdin().is_terminal() {
        return;
    }
    print!("{message}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
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

fn py_repr_str(value: &str) -> String {
    let mut out = String::from("'");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 32 || ch as u32 == 127 => {
                out.push_str(&format!("\\x{:02x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('\'');
    out
}

fn py_repr_json(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(string) => py_repr_str(string),
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(py_repr_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(items) => format!(
            "{{{}}}",
            items
                .iter()
                .map(|(key, value)| format!("{}: {}", py_repr_str(key), py_repr_json(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn bytes_repr(bytes: &[u8]) -> String {
    let mut out = String::from("b'");
    for &byte in bytes {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(byte as char),
            byte => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    out.push('\'');
    out
}

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

fn read_config_json(path: &Path) -> Map<String, Value> {
    let Ok(bytes) = fs::read(path) else {
        return Map::new();
    };
    match serde_json::from_slice::<Value>(strip_bom(&bytes)) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

fn load_config(path: &Path) -> Config {
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
                    "invalid \"target_filename\" value {} in config.json - using the default {}",
                    py_repr_json(&name_value),
                    py_repr_str(DEFAULT_TARGET_FILENAME)
                ),
            );
        }
    }

    config.keep_zst = coerce_bool(&data, "keep_zst", DEFAULT_KEEP_ZST);
    config.move_to_recycle_bin =
        coerce_bool(&data, "move_to_recycle_bin", DEFAULT_MOVE_TO_RECYCLE_BIN);

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
                    "invalid {} value {} in config.json - using the default {}",
                    py_repr_str(key),
                    py_repr_json(&other),
                    if default { "True" } else { "False" }
                ),
            );
            default
        }
    }
}

fn save_config(path: &Path, updates: &[(&str, Value)]) -> Result<(), String> {
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

fn ensure_config_file(path: &Path) -> (bool, Option<String>) {
    match fs::read(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let saved = save_config(path, &default_config_updates());
            match saved {
                Ok(()) => println!("Created default config.json: {}", path.display()),
                Err(ref message) => {
                    return (false, Some(message.clone()));
                }
            }
            (true, None)
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
                    "config.json is not valid JSON ({err}) - resetting it to defaults"
                ));
                match save_config(path, &default_config_updates()) {
                    Ok(()) => (true, None),
                    Err(message) => (false, Some(message)),
                }
            }
            Ok(Value::Object(data)) => {
                let defaults = default_config_updates();
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
                        println!(
                            "Updated config.json with missing setting(s): {}",
                            missing.join(", ")
                        );
                        (true, None)
                    }
                    Err(message) => (false, Some(message)),
                }
            }
            Ok(_) => {
                warn("config.json does not contain a JSON object - resetting it to defaults");
                match save_config(path, &default_config_updates()) {
                    Ok(()) => (true, None),
                    Err(message) => (false, Some(message)),
                }
            }
        },
    }
}

fn default_config_updates() -> Vec<(&'static str, Value)> {
    let config = Config::defaults();
    vec![
        ("target_filename", Value::String(config.target_filename)),
        ("keep_zst", Value::Bool(config.keep_zst)),
        (
            "move_to_recycle_bin",
            Value::Bool(config.move_to_recycle_bin),
        ),
    ]
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn is_valid_filename(name: &str) -> bool {
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
    if name.ends_with('.') || name.ends_with(' ') {
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

fn expand_env(value: &str) -> String {
    let source: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let required = unsafe { ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), None) };
    if required == 0 {
        return value.to_string();
    }

    let mut buffer = vec![0u16; required as usize];
    let written = unsafe { ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), Some(&mut buffer)) };
    if written == 0 || written > required {
        return value.to_string();
    }

    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

fn read_reg_string(subkey: &str, value_name: &str) -> Option<String> {
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

fn find_cs2_csgo_path() -> (Option<PathBuf>, String) {
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

fn reg_default_value(subkey: &str) -> Option<String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey_with_flags(subkey, KEY_READ).ok()?;
    let raw = key.get_raw_value("").ok()?;
    decode_reg_string(&raw)
}

fn context_menu_command() -> Option<String> {
    reg_default_value(CTX_COMMAND_KEY)
}

fn context_menu_label() -> Option<String> {
    reg_default_value(CTX_KEY)
}

fn context_menu_installed() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(CTX_KEY, KEY_READ)
        .is_ok()
}

fn split_ext(name: &str) -> (String, String) {
    let base_start = name.rfind(['\\', '/']).map(|index| index + 1).unwrap_or(0);
    let base = &name[base_start..];

    let Some(dot) = base.rfind('.') else {
        return (name.to_string(), String::new());
    };
    if dot == 0 {
        return (name.to_string(), String::new());
    }

    (base[..dot].to_string(), base[dot..].to_string())
}

fn next_free_name(folder: &Path, mut stem: String, ext: &str) -> String {
    while !stem.is_empty() && utf16_len(&stem) + utf16_len(ext) + 6 > MAX_FILENAME_LEN {
        stem.pop();
    }

    for number in 1..10000 {
        let candidate = format!("{stem}_{number}{ext}");
        if !folder.join(&candidate).exists() {
            return candidate;
        }
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("{stem}_{stamp}{ext}")
}

fn delete_original(path: &Path, use_recycle_bin: bool) -> Result<(), String> {
    if use_recycle_bin {
        return recycle_file(path);
    }

    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

fn recycle_file(path: &Path) -> Result<(), String> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut from: Vec<u16> = absolute
        .as_os_str()
        .encode_wide()
        .chain([0u16, 0u16])
        .collect();

    let mut operation = SHFILEOPSTRUCTW {
        hwnd: HWND::default(),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_mut_ptr()),
        pTo: PCWSTR::null(),
        fFlags: FOF_FLAGS,
        fAnyOperationsAborted: BOOL(0),
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: PCWSTR::null(),
    };

    let result = unsafe { SHFileOperationW(&mut operation) };

    if result == 0 && !operation.fAnyOperationsAborted.as_bool() {
        return Ok(());
    }

    if !absolute.exists() {
        return Ok(());
    }

    Err(format!("SHFileOperationW returned {result}"))
}

fn copy_payload(
    reader: &mut impl Read,
    writer: &mut impl Write,
    buffer: &mut [u8],
    head: &mut Vec<u8>,
    total: &mut u64,
) -> Result<(), CopyError> {
    loop {
        if CANCEL.load(Ordering::SeqCst) {
            return Err(CopyError::Cancelled);
        }

        let read = reader
            .read(buffer)
            .map_err(|err| CopyError::Zstd(err.to_string()))?;
        if read == 0 {
            break;
        }

        if head.len() < DEMO_MAGIC.len() {
            let take = (DEMO_MAGIC.len() - head.len()).min(read);
            head.extend_from_slice(&buffer[..take]);
            if head.len() == DEMO_MAGIC.len() && head != DEMO_MAGIC {
                return Err(CopyError::Magic);
            }
        }

        writer.write_all(&buffer[..read]).map_err(CopyError::Io)?;
        *total += read as u64;
    }

    Ok(())
}

fn report_magic_failure(head: &[u8]) {
    error("the decompressed data is not a CS2 demo - extraction aborted.");
    println!(
        "  header check: expected {}, got {}",
        bytes_repr(DEMO_MAGIC),
        bytes_repr(head)
    );
    println!("  nothing was overwritten and the original archive was kept.");
}

fn report_zstd_failure(message: &str) {
    error(format!("zstandard decompression failed ({message})."));
    println!("  the input file is probably not a valid .zst archive; nothing");
    println!("  was overwritten and the original archive was kept.");
}

fn report_permission_denied(output_dir: &Path) {
    error("permission denied.");
    println!("  - close CS2 if it is currently reading the demo file");
    println!("  - check that {} is writable", output_dir.display());
}

fn check_zstd_header(input: &Path) -> Option<bool> {
    let mut file = match File::open(input) {
        Ok(file) => file,
        Err(err) => {
            error(err.to_string());
            return None;
        }
    };

    let mut header = [0u8; 5];
    match file.read_exact(&mut header) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
            report_zstd_failure("unexpected end of file");
            return None;
        }
        Err(err) => {
            error(err.to_string());
            return None;
        }
    }

    if header[..4] != ZSTD_MAGIC {
        report_zstd_failure("unknown frame descriptor");
        return None;
    }

    Some(header[4] & CONTENT_CHECKSUM_FLAG != 0)
}

fn extract_one(
    input: &Path,
    output: &Path,
    keep_zst: bool,
    move_to_recycle_bin: bool,
) -> ExtractResult {
    if !input.is_file() {
        error(format!("input file not found: {}", input.display()));
        return ExtractResult::Failed;
    }

    let Some(has_checksum) = check_zstd_header(input) else {
        return ExtractResult::Failed;
    };

    println!("Extracting to: {}", output.display());

    let output_dir = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut temp = match tempfile::Builder::new()
        .prefix(".faceit-")
        .suffix(".partial")
        .tempfile_in(output_dir)
    {
        Ok(temp) => temp,
        Err(err) => {
            error(format!(
                "cannot create temporary file in {}: {err}",
                output_dir.display()
            ));
            return ExtractResult::Failed;
        }
    };

    let file = match File::open(input) {
        Ok(file) => file,
        Err(err) => {
            error(err.to_string());
            return ExtractResult::Failed;
        }
    };

    let mut decoder =
        match zstd::stream::read::Decoder::new(BufReader::with_capacity(COPY_BUFFER, file)) {
            Ok(decoder) => decoder,
            Err(err) => {
                report_zstd_failure(&err.to_string());
                return ExtractResult::Failed;
            }
        };

    let mut buffer = vec![0u8; COPY_BUFFER];
    let mut head = Vec::with_capacity(DEMO_MAGIC.len());
    let mut total: u64 = 0;

    let copy_result = {
        let mut writer = BufWriter::with_capacity(COPY_BUFFER, temp.as_file_mut());
        let mut result = copy_payload(
            &mut decoder,
            &mut writer,
            &mut buffer,
            &mut head,
            &mut total,
        );
        if result.is_ok()
            && let Err(err) = writer.flush()
        {
            result = Err(CopyError::Io(err));
        }
        result
    };

    if let Err(copy_error) = copy_result {
        match copy_error {
            CopyError::Magic => report_magic_failure(&head),
            CopyError::Cancelled => return ExtractResult::Cancelled,
            CopyError::Zstd(message) => report_zstd_failure(&message),
            CopyError::Io(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                report_permission_denied(output_dir);
            }
            CopyError::Io(err) => error(err.to_string()),
        }
        return ExtractResult::Failed;
    }

    if head.len() < DEMO_MAGIC.len() {
        report_magic_failure(&head);
        return ExtractResult::Failed;
    }

    let _ = temp.as_file().sync_all();

    let written = match temp.as_file().metadata() {
        Ok(metadata) => metadata.len(),
        Err(err) => {
            error(err.to_string());
            return ExtractResult::Failed;
        }
    };

    if written != total {
        error(format!(
            "temporary file size mismatch after extraction (expected {total} bytes, got {written})."
        ));
        return ExtractResult::Failed;
    }

    if CANCEL.load(Ordering::SeqCst) {
        return ExtractResult::Cancelled;
    }

    if let Err(persist_error) = temp.persist(output) {
        if persist_error.error.kind() == io::ErrorKind::PermissionDenied {
            report_permission_denied(output_dir);
        } else {
            error(persist_error.error.to_string());
        }
        return ExtractResult::Failed;
    }

    if CANCEL.load(Ordering::SeqCst) {
        return ExtractResult::Cancelled;
    }

    println!("OK - extraction successful.");

    if !has_checksum {
        warn("archive has no checksum; only truncation and the demo header were verified");
    }

    if keep_zst {
        println!("Original archive kept (keep_zst is enabled).");
        return ExtractResult::Ok;
    }

    match delete_original(input, move_to_recycle_bin) {
        Ok(()) => {
            if move_to_recycle_bin {
                println!("Original archive moved to the Recycle Bin.");
            } else {
                println!("Original archive permanently deleted.");
            }
        }
        Err(message) => {
            if move_to_recycle_bin {
                warn(format!(
                    "could not move the original file to the Recycle Bin ({message}); original kept."
                ));
            } else {
                warn(format!("could not delete the original file: {message}"));
            }
        }
    }

    ExtractResult::Ok
}

fn run_extract_mode(files: &[OsString], pause_after: bool) -> i32 {
    println!("{APP_NAME}");

    if files.is_empty() {
        return 0;
    }

    let (csgo, source) = find_cs2_csgo_path();
    let Some(csgo) = csgo else {
        error("could not locate the CS2 game\\csgo folder.");
        println!("  {source}");
        println!("Checked (under HKEY_LOCAL_MACHINE):");
        for (subkey, value_name) in CS2_REG_LOOKUP {
            println!("  - {subkey} -> {value_name}");
        }
        if pause_after {
            pause("Press Enter to close...");
        }
        return 1;
    };

    let paths = Paths::discover();
    let config = load_config(&paths.config);

    run_extract_batch(files, &csgo, &source, &config, pause_after)
}

fn run_extract_batch(
    files: &[OsString],
    csgo: &Path,
    source: &str,
    config: &Config,
    pause_after: bool,
) -> i32 {
    let target = &config.target_filename;
    let keep_zst = config.keep_zst;
    let move_to_recycle_bin = config.move_to_recycle_bin;

    println!("CS2 folder: {}   [{source}]", csgo.display());

    if keep_zst {
        println!("Original archives will be kept (keep_zst = true).");
    } else if move_to_recycle_bin {
        println!("Original archives will be moved to the Recycle Bin.");
    } else {
        println!("Original archives will be permanently deleted.");
    }

    let total = files.len();
    let (stem, ext) = split_ext(target);
    let mut extracted: Vec<String> = Vec::new();
    let mut failed: usize = 0;

    EXTRACTING.store(true, Ordering::SeqCst);

    for (index, file) in files.iter().enumerate() {
        if CANCEL.load(Ordering::SeqCst) {
            EXTRACTING.store(false, Ordering::SeqCst);
            eprintln!("\nCancelled.");
            return 130;
        }

        let input = PathBuf::from(file);
        let out_name = if total == 1 {
            println!("Processing: {}", input.display());
            target.clone()
        } else {
            println!("[{}/{}] {}", index + 1, total, input.display());
            next_free_name(csgo, stem.clone(), &ext)
        };
        let output_path = csgo.join(&out_name);

        match extract_one(&input, &output_path, keep_zst, move_to_recycle_bin) {
            ExtractResult::Ok => extracted.push(out_name),
            ExtractResult::Failed => failed += 1,
            ExtractResult::Cancelled => {
                EXTRACTING.store(false, Ordering::SeqCst);
                eprintln!("\nCancelled.");
                return 130;
            }
        }

        println!();
    }

    EXTRACTING.store(false, Ordering::SeqCst);

    let plural = if total == 1 { "" } else { "s" };
    println!(
        "Result: {} of {total} archive{plural} extracted.",
        extracted.len()
    );

    if !extracted.is_empty() {
        let stems: Vec<String> = extracted.iter().map(|name| split_ext(name).0).collect();

        if stems.len() == 1 {
            println!("In the CS2 console type: playdemo \"{}\"", stems[0]);
        } else {
            let mut preview = stems
                .iter()
                .take(5)
                .map(|stem| format!("\"{stem}\""))
                .collect::<Vec<_>>()
                .join(", ");
            if stems.len() > 5 {
                preview.push_str(", ...");
            }
            println!("Extracted demos - in the CS2 console, e.g.: playdemo {preview}");
        }
    }

    if pause_after {
        pause("Press Enter to close...");
    }

    if failed == 0 { 0 } else { 1 }
}

fn status_lines(paths: &Paths) -> Vec<String> {
    let (csgo, source) = find_cs2_csgo_path();
    let config = load_config(&paths.config);
    let width = 66;
    let mut lines = vec![
        "=".repeat(width),
        format!("  {APP_NAME}"),
        "=".repeat(width),
        format!("  Executable    : {}", paths.exe.display()),
        format!("  Config        : {}", paths.config.display()),
        format!(
            "  CS2 folder    : {}",
            csgo.as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "not found".to_string())
        ),
    ];

    if csgo.is_some() {
        lines.push(format!("  CS2 source    : {source}"));
    }

    lines.extend([
        "  zstd          : bundled".to_string(),
        "-".repeat(width),
        format!("  target_filename     : {}", config.target_filename),
        format!(
            "  keep_zst            : {}",
            if config.keep_zst { "on" } else { "off" }
        ),
        format!(
            "  move_to_recycle_bin : {}",
            if config.move_to_recycle_bin {
                "on"
            } else {
                "off"
            }
        ),
        "-".repeat(width),
    ]);

    if context_menu_installed() {
        let command = context_menu_command();
        lines.push(format!(
            "  Context menu  : {}",
            if command.is_some() {
                "installed"
            } else {
                "needs repair"
            }
        ));

        if let Some(label) = context_menu_label() {
            lines.push(format!("  Menu label    : {label}"));
        }
        if let Some(command) = command {
            lines.push(format!("  Menu command  : {command}"));
        }
    } else {
        lines.push("  Context menu  : not installed".to_string());
    }

    lines.push("=".repeat(width));
    lines
}

fn usage() -> String {
    format!(
        "{APP_NAME}

Usage:
  faceit-dem                            interactive menu
  faceit-dem <file.zst> [...]           extract file(s) directly
  faceit-dem -- <file> [...]            same, for names starting with \"-\"
  faceit-dem --context-menu <file>      queue a context-menu invocation
  faceit-dem --queue-worker             internal worker for queued files
  faceit-dem --install                  add per-user context-menu entry
  faceit-dem --remove                   remove per-user context-menu entry
  faceit-dem --update-label             update context-menu label
  faceit-dem --status                   show current configuration
  faceit-dem --help                     show this help

Notes:
  * The context-menu entry is per-user and does not require UAC.
  * zstd decoding is statically linked; no Python or extra runtime needed.
  * Context-menu invocations are queued briefly so multiple Explorer
    invocations can be processed by one worker as a batch.
  * config.json options:
      target_filename
      keep_zst
      move_to_recycle_bin
"
    )
}

fn first_arg_str(args: &[OsString]) -> Option<&str> {
    args.first().and_then(|arg| arg.to_str())
}

fn run() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let first = first_arg_str(&args);

    if first.is_some_and(|arg| HELP_FLAGS.contains(&arg)) {
        print!("{}", usage());
        return 0;
    }

    let paths = Paths::discover();
    let (ok, config_error) = ensure_config_file(&paths.config);
    if !ok {
        warn(format!(
            "could not create or update config.json: {}",
            config_error.unwrap_or_default()
        ));
    }

    if args.is_empty() {
        print!("{}", usage());
        return 0;
    }

    let first = first_arg_str(&args);

    if first.is_some_and(|arg| STUB_FLAGS.contains(&arg)) {
        error(format!(
            "{} is not available in this spike build.",
            first.unwrap_or_default()
        ));
        return 1;
    }

    if first == Some("--status") {
        println!("{}", status_lines(&paths).join("\n"));
        return 0;
    }

    if first == Some("--") {
        let rest = &args[1..];
        if rest.is_empty() {
            error("no input files after --");
            print!("{}", usage());
            return 2;
        }
        return run_extract_mode(rest, true);
    }

    if let Some(arg) = first
        && arg.starts_with('-')
        && !Path::new(arg).is_file()
    {
        error(format!("unknown option: {arg}"));
        print!("{}", usage());
        return 2;
    }

    run_extract_mode(&args, true)
}

fn install_ctrlc_handler() {
    let installed = ctrlc::set_handler(|| {
        CANCEL.store(true, Ordering::SeqCst);
        if !EXTRACTING.load(Ordering::SeqCst) {
            eprintln!("\nCancelled.");
            std::process::exit(130);
        }
    });

    if installed.is_err() {
        warn("could not install the Ctrl+C handler; extraction cannot be cancelled cleanly");
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic".to_string()
}

fn main() {
    install_ctrlc_handler();

    let code = match std::panic::catch_unwind(run) {
        Ok(code) => code,
        Err(payload) => {
            error(format!(
                "unexpected internal error: {}",
                panic_message(payload.as_ref())
            ));
            pause("Press Enter to close...");
            1
        }
    };

    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_archive(path: &Path, payload: &[u8], checksum: bool) {
        let file = File::create(path).unwrap();
        let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
        encoder.include_checksum(checksum).unwrap();
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap();
    }

    fn demo_payload(size: usize) -> Vec<u8> {
        let mut payload = DEMO_MAGIC.to_vec();
        payload.extend((0..size).map(|index| (index % 251) as u8));
        payload
    }

    fn no_partial_files(directory: &Path) -> bool {
        fs::read_dir(directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".faceit-")
        })
    }

    #[test]
    fn filename_validation_matches_python_rules() {
        assert!(is_valid_filename("1.dem"));
        assert!(is_valid_filename("demo_2024.dem"));
        assert!(is_valid_filename("a\u{0080}b.dem"));
        assert!(is_valid_filename(&"a".repeat(255)));

        assert!(!is_valid_filename(""));
        assert!(!is_valid_filename("."));
        assert!(!is_valid_filename(".."));
        assert!(!is_valid_filename("CON"));
        assert!(!is_valid_filename("con.dem"));
        assert!(!is_valid_filename("Lpt9.dem"));
        assert!(!is_valid_filename("foo/bar.dem"));
        assert!(!is_valid_filename(r"foo\bar.dem"));
        assert!(!is_valid_filename("bad<name"));
        assert!(!is_valid_filename("trailing."));
        assert!(!is_valid_filename("trailing "));
        assert!(!is_valid_filename("tab\tname"));
        assert!(!is_valid_filename("del\u{007f}"));
        assert!(!is_valid_filename("zero\u{200b}width"));
        assert!(!is_valid_filename(&"a".repeat(256)));
        assert!(!is_valid_filename(".dem"));
    }

    #[test]
    fn split_ext_matches_python_splitext() {
        assert_eq!(split_ext("1.dem"), ("1".to_string(), ".dem".to_string()));
        assert_eq!(split_ext("a.b.c"), ("a.b".to_string(), ".c".to_string()));
        assert_eq!(split_ext("demo"), ("demo".to_string(), String::new()));
        assert_eq!(split_ext(".dem"), (".dem".to_string(), String::new()));
        assert_eq!(split_ext("1."), ("1".to_string(), ".".to_string()));
    }

    #[test]
    fn next_free_name_finds_gaps_and_trims_long_stems() {
        let directory = tempfile::tempdir().unwrap();
        File::create(directory.path().join("1_1.dem")).unwrap();
        File::create(directory.path().join("1_3.dem")).unwrap();

        assert_eq!(
            next_free_name(directory.path(), "1".to_string(), ".dem"),
            "1_2.dem"
        );

        let long_stem = "a".repeat(300);
        let name = next_free_name(directory.path(), long_stem, ".dem");
        assert!(utf16_len(&name) <= MAX_FILENAME_LEN);
        assert!(name.ends_with("_1.dem"));
    }

    #[test]
    fn config_preserves_unknown_keys_and_bom() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let original =
            b"\xEF\xBB\xBF{\n  \"unknown\": 42,\n  \"target_filename\": \"demo.dem\"\n}\n";
        fs::write(&path, original).unwrap();

        save_config(&path, &[("keep_zst", Value::Bool(true))]).unwrap();

        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).expect("valid json");
        assert_eq!(saved["unknown"], Value::from(42));
        assert_eq!(saved["target_filename"], Value::from("demo.dem"));
        assert_eq!(saved["keep_zst"], Value::Bool(true));
        assert!(!fs::read(&path).unwrap().starts_with(&[0xEF, 0xBB, 0xBF]));
    }

    #[test]
    fn config_falls_back_on_invalid_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        fs::write(
            &path,
            r#"{"target_filename": 7, "keep_zst": "yes", "move_to_recycle_bin": true}"#,
        )
        .unwrap();

        let config = load_config(&path);
        assert_eq!(config.target_filename, DEFAULT_TARGET_FILENAME);
        assert!(!config.keep_zst);
        assert!(config.move_to_recycle_bin);
    }

    #[test]
    fn ensure_config_adds_missing_keys_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        fs::write(&path, r#"{"target_filename": "demo.dem", "custom": true}"#).unwrap();

        let (ok, error) = ensure_config_file(&path);
        assert!(ok, "{error:?}");

        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["target_filename"], Value::from("demo.dem"));
        assert_eq!(saved["custom"], Value::Bool(true));
        assert_eq!(saved["keep_zst"], Value::Bool(false));
        assert_eq!(saved["move_to_recycle_bin"], Value::Bool(false));
    }

    #[test]
    fn extract_success_replaces_target_and_keeps_archive() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("demo.dem.zst");
        let output = directory.path().join("1.dem");
        let payload = demo_payload(4096);
        write_archive(&input, &payload, true);

        let result = extract_one(&input, &output, true, false);

        assert!(matches!(result, ExtractResult::Ok));
        assert_eq!(fs::read(&output).unwrap(), payload);
        assert!(input.exists());
        assert!(no_partial_files(directory.path()));
    }

    #[test]
    fn extract_wrong_magic_keeps_target_and_archive() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("wrong.dem.zst");
        let output = directory.path().join("1.dem");
        write_archive(&input, b"NOTADEMO and more bytes", true);
        fs::write(&output, b"existing demo").unwrap();

        let result = extract_one(&input, &output, false, false);

        assert!(matches!(result, ExtractResult::Failed));
        assert_eq!(fs::read(&output).unwrap(), b"existing demo");
        assert!(input.exists());
        assert!(no_partial_files(directory.path()));
    }

    #[test]
    fn extract_truncated_archive_fails_cleanly() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("truncated.dem.zst");
        let output = directory.path().join("1.dem");
        write_archive(&input, &demo_payload(65536), true);

        let bytes = fs::read(&input).unwrap();
        fs::write(&input, &bytes[..bytes.len() / 2]).unwrap();
        fs::write(&output, b"existing demo").unwrap();

        let result = extract_one(&input, &output, false, false);

        assert!(matches!(result, ExtractResult::Failed));
        assert_eq!(fs::read(&output).unwrap(), b"existing demo");
        assert!(input.exists());
        assert!(no_partial_files(directory.path()));
    }

    #[test]
    fn extract_checksum_mismatch_fails_cleanly() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("corrupt.dem.zst");
        let output = directory.path().join("1.dem");
        write_archive(&input, &demo_payload(65536), true);

        let mut bytes = fs::read(&input).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&input, &bytes).unwrap();
        fs::write(&output, b"existing demo").unwrap();

        let result = extract_one(&input, &output, false, false);

        assert!(matches!(result, ExtractResult::Failed));
        assert_eq!(fs::read(&output).unwrap(), b"existing demo");
        assert!(input.exists());
        assert!(no_partial_files(directory.path()));
    }

    #[test]
    fn extract_not_zstd_fails_before_writing() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("plain.zst");
        let output = directory.path().join("1.dem");
        fs::write(&input, b"not a zstd archive").unwrap();
        fs::write(&output, b"existing demo").unwrap();

        let result = extract_one(&input, &output, false, false);

        assert!(matches!(result, ExtractResult::Failed));
        assert_eq!(fs::read(&output).unwrap(), b"existing demo");
        assert!(input.exists());
    }

    #[test]
    fn extract_deletes_original_when_configured() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("demo.dem.zst");
        let output = directory.path().join("1.dem");
        write_archive(&input, &demo_payload(512), true);

        let result = extract_one(&input, &output, false, false);

        assert!(matches!(result, ExtractResult::Ok));
        assert!(!input.exists());
        assert!(output.exists());
    }

    #[test]
    fn bytes_repr_matches_python() {
        assert_eq!(bytes_repr(b"PBDEMS2\x00"), "b'PBDEMS2\\x00'");
        assert_eq!(bytes_repr(b"a'b\\c"), "b'a\\'b\\\\c'");
    }

    #[test]
    fn batch_mode_writes_first_free_slots() {
        let directory = tempfile::tempdir().unwrap();
        let inputs_dir = directory.path().join("in");
        fs::create_dir(&inputs_dir).unwrap();

        let mut inputs = Vec::new();
        for index in 0..2 {
            let input = inputs_dir.join(format!("demo{index}.zst"));
            write_archive(&input, &demo_payload(256 + index), true);
            inputs.push(input.into_os_string());
        }

        File::create(directory.path().join("1_2.dem")).unwrap();
        let code = run_extract_batch(
            &inputs,
            directory.path(),
            "test",
            &Config::defaults(),
            false,
        );

        assert_eq!(code, 0);
        assert!(directory.path().join("1_1.dem").exists());
        assert!(directory.path().join("1_3.dem").exists());
        assert!(!inputs_dir.join("demo0.zst").exists());
        assert!(!inputs_dir.join("demo1.zst").exists());
    }
}

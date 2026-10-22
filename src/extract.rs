use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{self, Config};
use crate::cs2;
use crate::output::{self, bytes_repr, error, warn};
use crate::validate::utf16_len;
use crate::win32;
use crate::{APP_NAME, CANCEL, DEMO_MAGIC, EXTRACTING, MAX_FILENAME_LEN, Paths};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const CONTENT_CHECKSUM_FLAG: u8 = 0x04;
const COPY_BUFFER: usize = 1024 * 1024;

pub const CS2_REG_LOOKUP: &[(&str, &str)] = cs2::CS2_REG_LOOKUP;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractResult {
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

struct ExtractingGuard;

impl ExtractingGuard {
    fn new() -> Self {
        EXTRACTING.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for ExtractingGuard {
    fn drop(&mut self) {
        EXTRACTING.store(false, Ordering::SeqCst);
    }
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

pub fn extract_one(
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

pub fn delete_original(path: &Path, use_recycle_bin: bool) -> Result<(), String> {
    if use_recycle_bin {
        return win32::recycle_file(path);
    }

    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

pub fn split_ext(name: &str) -> (String, String) {
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

pub fn next_free_name(folder: &Path, mut stem: String, ext: &str) -> String {
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

pub fn run_extract_mode(paths: &Paths, files: &[PathBuf], pause_after: bool) -> i32 {
    println!("{APP_NAME}");

    if files.is_empty() {
        return 0;
    }

    let (csgo, source) = cs2::find_cs2_csgo_path();
    let Some(csgo) = csgo else {
        error("could not locate the CS2 game\\csgo folder.");
        println!("  {source}");
        println!("Checked (under HKEY_LOCAL_MACHINE):");
        for (subkey, value_name) in CS2_REG_LOOKUP {
            println!("  - {subkey} -> {value_name}");
        }
        if pause_after {
            output::pause("Press Enter to close...");
        }
        return 1;
    };

    let config = config::load_config(&paths.config);
    run_extract_batch(files, &csgo, &source, &config, pause_after)
}

pub fn run_extract_batch(
    files: &[PathBuf],
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

    let _guard = ExtractingGuard::new();

    for (index, input) in files.iter().enumerate() {
        if CANCEL.load(Ordering::SeqCst) {
            eprintln!("\nCancelled.");
            return 130;
        }

        let out_name = if total == 1 {
            println!("Processing: {}", input.display());
            target.clone()
        } else {
            println!("[{}/{}] {}", index + 1, total, input.display());
            next_free_name(csgo, stem.clone(), &ext)
        };
        let output_path = csgo.join(&out_name);

        match extract_one(input, &output_path, keep_zst, move_to_recycle_bin) {
            ExtractResult::Ok => extracted.push(out_name),
            ExtractResult::Failed => failed += 1,
            ExtractResult::Cancelled => {
                eprintln!("\nCancelled.");
                return 130;
            }
        }

        println!();
    }

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
        output::pause("Press Enter to close...");
    }

    if failed == 0 { 0 } else { 1 }
}

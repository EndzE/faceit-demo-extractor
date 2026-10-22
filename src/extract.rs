use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{self, Config};
use crate::cs2;
use crate::output::{self, error, warn};
use crate::validate::utf16_len;
use crate::win32;
use crate::{CANCEL, DEMO_MAGIC, EXTRACTING, MAX_FILENAME_LEN, Paths};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const CONTENT_CHECKSUM_FLAG: u8 = 0x04;
const COPY_BUFFER: usize = 1024 * 1024;
const HEADER_BUFFER: usize = 8 * 1024;
const PARTIAL_PREFIX: &str = ".faceit-";
const PARTIAL_SUFFIX: &str = ".partial";
const ORPHANED_PARTIAL_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractResult {
    Ok,
    Failed,
    Cancelled,
}

enum CopyError {
    Open(String),
    Io(io::Error),
    Zstd(String),
    Magic,
    Cancelled,
}

struct ZstdHeader {
    has_checksum: bool,
    content_size: Option<u64>,
}

enum HeaderError {
    Open(io::Error),
    Read(io::Error),
    Format(&'static str),
}

impl From<io::Error> for HeaderError {
    fn from(err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::UnexpectedEof {
            HeaderError::Format("unexpected end of file")
        } else {
            HeaderError::Read(err)
        }
    }
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

fn hex_preview(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn report_magic_failure(head: &[u8]) {
    error("the decompressed data is not a CS2 demo; the archive was rejected.");
    println!("{}", output::field("Expected header", "PBDEMS2"));
    println!("{}", output::field("Actual header", hex_preview(head)));
    println!(
        "  {}",
        output::muted("Nothing was overwritten; the original archive was kept.")
    );
}

fn report_zstd_failure(message: &str) {
    error(format!("Zstandard decompression failed ({message})."));
    println!(
        "  {}",
        output::muted("The file does not look like a valid .zst archive.")
    );
    println!(
        "  {}",
        output::muted("Nothing was overwritten; the original archive was kept.")
    );
}

fn report_permission_denied(output_dir: &Path) {
    error("permission denied while writing the demo.");
    println!(
        "  {} Close CS2 if it is currently reading the demo.",
        output::note("-")
    );
    println!(
        "  {} Check that {} is writable.",
        output::note("-"),
        output::strong(output_dir.display())
    );
}

fn report_decode_failure(copy_error: CopyError, head: &[u8], output_dir: &Path) -> ExtractResult {
    match copy_error {
        CopyError::Open(message) => error(format!("could not open the archive: {message}")),
        CopyError::Magic => report_magic_failure(head),
        CopyError::Cancelled => return ExtractResult::Cancelled,
        CopyError::Zstd(message) => report_zstd_failure(&message),
        CopyError::Io(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            report_permission_denied(output_dir);
        }
        CopyError::Io(err) => error(format!("could not write the demo: {err}")),
    }
    ExtractResult::Failed
}

fn report_disk_space_failure(output_dir: &Path, required: u64, available: u64) {
    error("not enough free space to extract the demo.");
    println!(
        "{}",
        output::field("Required", output::strong(output::format_size(required)))
    );
    println!(
        "{}",
        output::field("Available", output::caution(output::format_size(available)))
    );
    println!(
        "  {}",
        output::muted(format!("Nothing was written to {}.", output_dir.display()))
    );
}

fn read_u8(reader: &mut impl Read) -> io::Result<u8> {
    let mut buffer = [0u8; 1];
    reader.read_exact(&mut buffer)?;
    Ok(buffer[0])
}

fn read_u16_le(reader: &mut impl Read) -> io::Result<u16> {
    let mut buffer = [0u8; 2];
    reader.read_exact(&mut buffer)?;
    Ok(u16::from_le_bytes(buffer))
}

fn read_u24_le(reader: &mut impl Read) -> io::Result<u32> {
    let mut buffer = [0u8; 3];
    reader.read_exact(&mut buffer)?;
    Ok(u32::from_le_bytes([buffer[0], buffer[1], buffer[2], 0]))
}

fn read_u32_le(reader: &mut impl Read) -> io::Result<u32> {
    let mut buffer = [0u8; 4];
    reader.read_exact(&mut buffer)?;
    Ok(u32::from_le_bytes(buffer))
}

fn read_u64_le(reader: &mut impl Read) -> io::Result<u64> {
    let mut buffer = [0u8; 8];
    reader.read_exact(&mut buffer)?;
    Ok(u64::from_le_bytes(buffer))
}

fn skip_exact(reader: &mut BufReader<File>, remaining: u64) -> io::Result<()> {
    let offset = i64::try_from(remaining)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "skip is too large"))?;
    reader.seek_relative(offset)
}

fn inspect_zstd_header(input: &Path) -> Result<ZstdHeader, HeaderError> {
    let file = File::open(input).map_err(HeaderError::Open)?;
    let file_len = file.metadata().map_err(HeaderError::Read)?.len();
    let mut reader = BufReader::with_capacity(HEADER_BUFFER, file);
    let mut seen_frame = false;
    let mut has_checksum = true;
    let mut content_size = Some(0u64);

    loop {
        let mut magic = [0u8; 4];
        let first = match reader.read(&mut magic[..1]) {
            Ok(first) => first,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.into()),
        };
        if first == 0 {
            break;
        }
        reader.read_exact(&mut magic[1..])?;

        if magic == ZSTD_MAGIC {
            seen_frame = true;
            let descriptor = read_u8(&mut reader)?;
            if descriptor & 0x08 != 0 {
                return Err(HeaderError::Format("invalid frame header"));
            }

            let single_segment = descriptor & 0x20 != 0;
            if !single_segment {
                let _window_descriptor = read_u8(&mut reader)?;
            }

            let dictionary_size = match descriptor & 0x03 {
                0 => 0,
                1 => 1,
                2 => 2,
                _ => 4,
            };
            skip_exact(&mut reader, dictionary_size)?;

            let content_size_field = match (descriptor >> 6, single_segment) {
                (0, false) => 0,
                (0, true) => 1,
                (1, _) => 2,
                (2, _) => 4,
                _ => 8,
            };
            let frame_size = match content_size_field {
                0 => None,
                1 => Some(u64::from(read_u8(&mut reader)?)),
                2 => Some(u64::from(read_u16_le(&mut reader)?) + 256),
                4 => Some(u64::from(read_u32_le(&mut reader)?)),
                8 => Some(read_u64_le(&mut reader)?),
                _ => unreachable!(),
            };
            content_size = match (content_size, frame_size) {
                (Some(total), Some(size)) => total.checked_add(size),
                _ => None,
            };
            has_checksum &= descriptor & CONTENT_CHECKSUM_FLAG != 0;

            loop {
                let block = read_u24_le(&mut reader)?;
                let last = block & 1 != 0;
                let block_type = (block >> 1) & 0x03;
                let block_size = u64::from(block >> 3);
                match block_type {
                    0 | 2 => skip_exact(&mut reader, block_size)?,
                    1 => skip_exact(&mut reader, 1)?,
                    _ => return Err(HeaderError::Format("invalid block header")),
                }
                if last {
                    break;
                }
            }

            if descriptor & CONTENT_CHECKSUM_FLAG != 0 {
                skip_exact(&mut reader, 4)?;
            }
        } else if matches!(magic, [0x50..=0x5F, 0x2A, 0x4D, 0x18]) {
            let size = u64::from(read_u32_le(&mut reader)?);
            skip_exact(&mut reader, size)?;
        } else {
            return Err(HeaderError::Format("unknown frame descriptor"));
        }
    }

    if !seen_frame {
        return Err(HeaderError::Format("unexpected end of file"));
    }

    if reader.stream_position()? > file_len {
        return Err(HeaderError::Format("unexpected end of file"));
    }

    Ok(ZstdHeader {
        has_checksum,
        content_size,
    })
}

fn check_zstd_header(input: &Path) -> Option<ZstdHeader> {
    match inspect_zstd_header(input) {
        Ok(header) => Some(header),
        Err(HeaderError::Open(err)) => {
            error(format!("could not open the archive: {err}"));
            None
        }
        Err(HeaderError::Read(err)) => {
            error(format!("could not read the archive header: {err}"));
            None
        }
        Err(HeaderError::Format(message)) => {
            report_zstd_failure(message);
            None
        }
    }
}

fn copy_payload(
    reader: &mut impl Read,
    writer: &mut impl Write,
    buffer: &mut [u8],
    head: &mut Vec<u8>,
    total: &mut u64,
    progress: &mut output::Progress,
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
        progress.update(*total);
    }

    Ok(())
}

fn decode_archive(
    input: &Path,
    writer: &mut impl Write,
    content_size: Option<u64>,
    head: &mut Vec<u8>,
) -> Result<u64, CopyError> {
    let file = File::open(input).map_err(|err| CopyError::Open(err.to_string()))?;
    let mut decoder = zstd::stream::read::Decoder::new(BufReader::with_capacity(COPY_BUFFER, file))
        .map_err(|err| CopyError::Zstd(err.to_string()))?;

    let mut buffer = vec![0u8; COPY_BUFFER];
    let mut total = 0;
    let mut progress = output::Progress::new(content_size);
    progress.update(0);

    let result = copy_payload(
        &mut decoder,
        writer,
        &mut buffer,
        head,
        &mut total,
        &mut progress,
    );
    drop(progress);

    result?;
    Ok(total)
}

fn cleanup_orphaned_partials(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let now = SystemTime::now();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(PARTIAL_PREFIX) || !name.ends_with(PARTIAL_SUFFIX) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        let orphaned = now
            .duration_since(modified)
            .is_ok_and(|age| age > ORPHANED_PARTIAL_AGE);
        if orphaned {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    if let (Ok(left), Ok(right)) = (fs::canonicalize(left), fs::canonicalize(right)) {
        return left == right;
    }

    let absolute = |path: &Path| std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    absolute(left) == absolute(right)
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

    if paths_refer_to_same_file(input, output) {
        error(format!(
            "the archive and the demo target are the same file: {}",
            input.display()
        ));
        println!(
            "  {}",
            output::muted("Change \"target_filename\" in config.json; nothing was modified.")
        );
        return ExtractResult::Failed;
    }

    let Some(header) = check_zstd_header(input) else {
        return ExtractResult::Failed;
    };
    let has_checksum = header.has_checksum;
    let content_size = header.content_size;

    println!("{}", output::field("Output", output.display()));

    let output_dir = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    cleanup_orphaned_partials(output_dir);

    if let Some(required) = content_size {
        match win32::available_space(output_dir) {
            Ok(available) if available < required => {
                report_disk_space_failure(output_dir, required, available);
                return ExtractResult::Failed;
            }
            Ok(_) => {}
            Err(err) => warn(format!(
                "could not check free space in {}: {err}",
                output_dir.display()
            )),
        }
    }

    let mut temp = match tempfile::Builder::new()
        .prefix(PARTIAL_PREFIX)
        .suffix(PARTIAL_SUFFIX)
        .tempfile_in(output_dir)
    {
        Ok(temp) => temp,
        Err(err) => {
            error(format!(
                "could not create a temporary file in {}: {err}",
                output_dir.display()
            ));
            return ExtractResult::Failed;
        }
    };

    let mut head = Vec::with_capacity(DEMO_MAGIC.len());
    let copy_result = {
        let mut writer = BufWriter::with_capacity(COPY_BUFFER, temp.as_file_mut());
        let decoded = decode_archive(input, &mut writer, content_size, &mut head);
        match decoded {
            Ok(total) => match writer.flush() {
                Ok(()) => Ok(total),
                Err(err) => Err(CopyError::Io(err)),
            },
            Err(err) => Err(err),
        }
    };
    let total = match copy_result {
        Ok(total) => total,
        Err(copy_error) => return report_decode_failure(copy_error, &head, output_dir),
    };

    if head.len() < DEMO_MAGIC.len() {
        report_magic_failure(&head);
        return ExtractResult::Failed;
    }

    if let Err(err) = temp.as_file().sync_all() {
        error(format!("could not flush the demo to disk: {err}"));
        return ExtractResult::Failed;
    }

    let written = match temp.as_file().metadata() {
        Ok(metadata) => metadata.len(),
        Err(err) => {
            error(format!("could not inspect the temporary file: {err}"));
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
            error(format!("could not save the demo: {}", persist_error.error));
        }
        return ExtractResult::Failed;
    }

    println!("  {}  Extraction complete.", output::good("OK"));

    if !has_checksum {
        warn(
            "one or more archive frames have no checksum; only truncation and the demo header were verified",
        );
    }

    if keep_zst || !has_checksum {
        println!(
            "{}",
            output::field(
                "Archive",
                output::muted(if keep_zst {
                    "kept"
                } else {
                    "kept (checksum missing)"
                })
            )
        );
        return ExtractResult::Ok;
    }

    match delete_original(input, move_to_recycle_bin) {
        Ok(()) => {
            println!(
                "{}",
                output::field(
                    "Archive",
                    output::muted(if move_to_recycle_bin {
                        "moved to Recycle Bin"
                    } else {
                        "deleted"
                    })
                )
            );
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

pub fn test_archive(input: &Path) -> ExtractResult {
    if !input.is_file() {
        error(format!("input file not found: {}", input.display()));
        return ExtractResult::Failed;
    }

    let Some(header) = check_zstd_header(input) else {
        return ExtractResult::Failed;
    };

    println!("{}", output::field("Input", input.display()));

    let _guard = ExtractingGuard::new();
    let mut head = Vec::with_capacity(DEMO_MAGIC.len());
    let mut sink = io::sink();

    match decode_archive(input, &mut sink, header.content_size, &mut head) {
        Ok(total) => {
            if head.len() < DEMO_MAGIC.len() {
                report_magic_failure(&head);
                println!("{}", output::field("Result", output::bad("failed")));
                return ExtractResult::Failed;
            }

            if !header.has_checksum {
                warn(
                    "one or more archive frames have no XXH64 checksum; integrity could not be fully verified",
                );
                println!(
                    "{}",
                    output::field("Result", output::caution("not verified"))
                );
                return ExtractResult::Failed;
            }

            println!(
                "{}",
                output::field("Checksum", output::good("XXH64 verified"))
            );
            println!(
                "{}",
                output::field(
                    "Result",
                    output::good(format!("passed ({} decoded)", output::format_size(total)))
                )
            );
            println!("  {}", output::muted("No files were written."));
            ExtractResult::Ok
        }
        Err(copy_error) => {
            let output_dir = input
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let result = report_decode_failure(copy_error, &head, output_dir);
            if result == ExtractResult::Cancelled {
                output::cancelled();
            } else {
                println!("{}", output::field("Result", output::bad("failed")));
            }
            result
        }
    }
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

fn name_with_suffix(stem: &str, suffix: &str, ext: &str) -> String {
    let mut stem = stem.to_string();
    while !stem.is_empty()
        && utf16_len(&stem) + utf16_len(suffix) + utf16_len(ext) > MAX_FILENAME_LEN
    {
        stem.pop();
    }
    format!("{stem}{suffix}{ext}")
}

fn next_free_name_with_attempts(folder: &Path, stem: &str, ext: &str, attempts: u32) -> String {
    for number in 1..attempts {
        let candidate = name_with_suffix(stem, &format!("_{number}"), ext);
        if !folder.join(&candidate).exists() {
            return candidate;
        }
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    name_with_suffix(stem, &format!("_{stamp}"), ext)
}

pub fn next_free_name(folder: &Path, stem: String, ext: &str) -> String {
    next_free_name_with_attempts(folder, &stem, ext, 10_000)
}

pub fn run_extract_mode(paths: &Paths, files: &[PathBuf], pause_after: bool) -> i32 {
    if files.is_empty() {
        return 0;
    }

    let config = config::load_config(&paths.config);
    let (csgo, source) = cs2::find_cs2_csgo_path(config.csgo_path.as_deref(), &paths.exe_dir);
    let Some(csgo) = csgo else {
        error("could not locate the CS2 game\\csgo folder.");
        println!("  {}", output::muted(&source));
        println!();
        if config.csgo_path.is_some() {
            println!(
                "  {}",
                output::muted(format!("Config: {}", paths.config.display()))
            );
            println!(
                "  {}",
                output::muted("Fix or remove \"csgo_path\" in config.json.")
            );
        } else {
            println!("{}", output::section("Locations checked"));
            for (subkey, value_name) in cs2::CS2_REG_LOOKUP {
                println!("  {} -> {value_name}", output::muted(subkey));
            }
            println!(
                "  {}",
                output::muted("Set \"csgo_path\" in config.json to use a custom install.")
            );
        }
        if pause_after {
            println!();
            output::pause("Press Enter to close...");
        }
        return 1;
    };

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

    println!("{}", output::field("CS2 folder", csgo.display()));
    println!("{}", output::field("CS2 source", source));
    println!(
        "{}",
        output::field(
            "Archives",
            if keep_zst {
                output::good("keep originals")
            } else if move_to_recycle_bin {
                output::caution("move to Recycle Bin")
            } else {
                output::caution("delete originals")
            }
        )
    );
    println!();

    let total = files.len();
    let (stem, ext) = split_ext(target);
    let mut extracted: Vec<String> = Vec::new();
    let mut failed: Vec<&Path> = Vec::new();

    let _guard = ExtractingGuard::new();

    for (index, input) in files.iter().enumerate() {
        if CANCEL.load(Ordering::SeqCst) {
            output::cancelled();
            return 130;
        }

        let out_name = if total == 1 {
            println!("{}", output::field("Input", input.display()));
            target.clone()
        } else {
            println!(
                "{}",
                output::title(format!("[{}/{}] {}", index + 1, total, input.display()))
            );
            next_free_name(csgo, stem.clone(), &ext)
        };
        let output_path = csgo.join(&out_name);

        match extract_one(input, &output_path, keep_zst, move_to_recycle_bin) {
            ExtractResult::Ok => extracted.push(out_name),
            ExtractResult::Failed => failed.push(input),
            ExtractResult::Cancelled => {
                output::cancelled();
                return 130;
            }
        }

        println!();
    }

    let plural = if total == 1 { "" } else { "s" };
    let result = format!(
        "{} of {total} archive{plural} extracted{}",
        extracted.len(),
        if failed.is_empty() {
            String::new()
        } else {
            format!("; {} failed", failed.len())
        }
    );
    println!(
        "{}",
        output::field(
            "Result",
            if failed.is_empty() {
                output::good(result)
            } else {
                output::caution(result)
            }
        )
    );

    if !failed.is_empty() {
        println!();
        println!("{}", output::section("Failed archives"));
        for input in &failed {
            println!("  {}", output::bad(input.display()));
        }
    }

    if !extracted.is_empty() {
        let stems: Vec<String> = extracted.iter().map(|name| split_ext(name).0).collect();

        println!();
        println!("{}", output::section("Play in CS2"));
        println!(
            "  {} {}",
            output::muted("Console:"),
            output::strong(format!("playdemo \"{}\"", stems[0]))
        );

        if stems.len() > 1 {
            let mut others = stems
                .iter()
                .skip(1)
                .take(5)
                .map(|stem| format!("\"{stem}\""))
                .collect::<Vec<_>>()
                .join(", ");
            if stems.len() > 6 {
                others.push_str(", ...");
            }
            println!("  {} {others}", output::muted("Other demos:"));
        }
    }

    if pause_after {
        println!();
        output::pause("Press Enter to close...");
    }

    if failed.is_empty() { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zstd_frame(payload: &[u8], checksum: bool) -> Vec<u8> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        encoder
            .set_pledged_src_size(Some(payload.len() as u64))
            .unwrap();
        encoder.include_checksum(checksum).unwrap();
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn frame_header_exposes_content_size_and_checksum() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("demo.dem.zst");
        let payload = b"PBDEMS2\x00test demo payload";
        let file = File::create(&input).unwrap();
        let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
        encoder
            .set_pledged_src_size(Some(payload.len() as u64))
            .unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap();

        let header = check_zstd_header(&input).expect("zstd header");

        assert!(header.has_checksum);
        assert_eq!(header.content_size, Some(payload.len() as u64));
    }

    #[test]
    fn frame_header_reads_a_two_byte_content_size() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("demo.dem.zst");
        let payload = vec![0xAB; 4096];
        fs::write(&input, zstd_frame(&payload, true)).unwrap();

        let header = check_zstd_header(&input).expect("zstd header");

        assert_eq!(header.content_size, Some(payload.len() as u64));
    }

    #[test]
    fn frame_header_sums_concatenated_frames() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("demo.dem.zst");
        let first = b"PBDEMS2\x00first";
        let second = b"second";

        let mut bytes = zstd_frame(first, true);
        bytes.extend(zstd_frame(second, true));
        fs::write(&input, bytes).unwrap();

        let header = check_zstd_header(&input).expect("zstd header");

        assert!(header.has_checksum);
        assert_eq!(
            header.content_size,
            Some((first.len() + second.len()) as u64)
        );
    }

    #[test]
    fn frame_header_requires_a_checksum_in_every_frame() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("demo.dem.zst");
        let first = b"PBDEMS2\x00first";
        let second = b"second";

        let mut bytes = zstd_frame(first, true);
        bytes.extend(zstd_frame(second, false));
        fs::write(&input, bytes).unwrap();

        let header = check_zstd_header(&input).expect("zstd header");

        assert!(!header.has_checksum);
        assert_eq!(
            header.content_size,
            Some((first.len() + second.len()) as u64)
        );
    }

    #[test]
    fn frame_header_accepts_a_skippable_frame_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("demo.dem.zst");
        let payload = b"PBDEMS2\x00demo";

        let mut bytes = vec![0x50, 0x2A, 0x4D, 0x18];
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(b"abc");
        bytes.extend(zstd_frame(payload, true));
        fs::write(&input, bytes).unwrap();

        let header = check_zstd_header(&input).expect("zstd header");

        assert!(header.has_checksum);
        assert_eq!(header.content_size, Some(payload.len() as u64));
    }

    #[test]
    fn name_with_suffix_fits_the_filename_limit() {
        let stem = "a".repeat(MAX_FILENAME_LEN);
        let suffix = "_1234567890";

        let name = name_with_suffix(&stem, suffix, ".dem");

        assert!(utf16_len(&name) <= MAX_FILENAME_LEN);
        assert!(name.ends_with("_1234567890.dem"));
    }

    #[test]
    fn next_free_name_falls_back_to_a_timestamp() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("1_1.dem"), b"x").unwrap();
        fs::write(directory.path().join("1_2.dem"), b"x").unwrap();

        let name = next_free_name_with_attempts(directory.path(), "1", ".dem", 3);

        let suffix = name
            .strip_prefix("1_")
            .and_then(|rest| rest.strip_suffix(".dem"))
            .expect("timestamp suffix");
        assert!(suffix.chars().all(|ch| ch.is_ascii_digit()));
        assert!(utf16_len(&name) <= MAX_FILENAME_LEN);
        assert!(!directory.path().join(&name).exists());
    }

    #[test]
    fn orphaned_partials_are_removed() {
        let directory = tempfile::tempdir().unwrap();
        let stale = directory.path().join(".faceit-old.partial");
        let fresh = directory.path().join(".faceit-new.partial");
        let unrelated = directory.path().join("keep.partial");
        fs::write(&stale, b"x").unwrap();
        fs::write(&fresh, b"x").unwrap();
        fs::write(&unrelated, b"x").unwrap();

        let modified = SystemTime::now() - ORPHANED_PARTIAL_AGE - Duration::from_secs(1);
        File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(modified)
            .unwrap();

        cleanup_orphaned_partials(directory.path());

        assert!(!stale.exists());
        assert!(fresh.exists());
        assert!(unrelated.exists());
    }
}

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{self, Config};
use crate::cs2;
use crate::output::{self, error, warn};
use crate::validate::{DemoFileName, utf16_len};
use crate::win32;
use crate::{CANCEL, DEMO_MAGIC, EXTRACTING, MAX_FILENAME_LEN, Paths};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const BZIP2_MAGIC: &[u8; 3] = b"BZh";
const BZIP2_BLOCK_MAGIC: [u8; 6] = [0x31, 0x41, 0x59, 0x26, 0x53, 0x59];
const CONTENT_CHECKSUM_FLAG: u8 = 0x04;
const MAX_ZSTD_BLOCK_SIZE: u64 = 128 * 1024;
const COPY_BUFFER: usize = 1024 * 1024;
const HEADER_BUFFER: usize = 64;
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
    Codec(ArchiveFormat, String),
    Magic,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArchiveFormat {
    Zstd,
    Bzip2,
}

impl fmt::Display for ArchiveFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl ArchiveFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Zstd => "Zstandard",
            Self::Bzip2 => "Bzip2",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Zstd => ".zst",
            Self::Bzip2 => ".bz2",
        }
    }
}

#[derive(Debug)]
struct ArchiveHeader {
    has_checksum: bool,
    content_size: Option<u64>,
}

#[derive(Debug)]
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
        EXTRACTING.store(true, Ordering::Relaxed);
        Self
    }
}

impl Drop for ExtractingGuard {
    fn drop(&mut self) {
        EXTRACTING.store(false, Ordering::Relaxed);
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

fn report_codec_failure(format: ArchiveFormat, message: &str) {
    error(format!(
        "{} decompression failed ({}).",
        format.name(),
        message
    ));
    println!(
        "  {}",
        output::muted(format!(
            "The file does not look like a valid {} archive.",
            format.extension()
        ))
    );
    println!(
        "  {}",
        output::muted("Nothing was overwritten; the original archive was kept.")
    );
}

fn report_unrecognized_format(head: &[u8]) {
    error("the archive format is not recognized; the archive was rejected.");
    println!(
        "{}",
        output::field(
            "Expected",
            output::muted("a .zst (Zstandard) or .bz2 (bzip2) archive")
        )
    );
    println!("{}", output::field("Actual header", hex_preview(head)));
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
        output::strong(output_dir.to_string_lossy())
    );
}

fn report_decode_failure(copy_error: CopyError, head: &[u8], output_dir: &Path) -> ExtractResult {
    match copy_error {
        CopyError::Open(message) => error(format!("could not open the archive: {message}")),
        CopyError::Magic => report_magic_failure(head),
        CopyError::Cancelled => return ExtractResult::Cancelled,
        CopyError::Codec(format, message) => report_codec_failure(format, &message),
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

fn inspect_zstd_header(input: &Path) -> Result<ArchiveHeader, HeaderError> {
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
                if block_type == 0 && block_size == 0 && !last {
                    return Err(HeaderError::Format("empty non-terminal raw block detected"));
                }
                if block_size > MAX_ZSTD_BLOCK_SIZE {
                    return Err(HeaderError::Format(
                        "block size exceeds maximum allowed Zstandard block size (128 KB)",
                    ));
                }
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

    Ok(ArchiveHeader {
        has_checksum,
        content_size,
    })
}

fn read_available(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(ref err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

#[derive(Debug)]
enum DetectError {
    Open(io::Error),
    Read(io::Error),
}

#[derive(Debug)]
enum DetectedFormat {
    Zstd,
    Bzip2,
    Unrecognized { head: Vec<u8> },
}

fn detect_format(input: &Path) -> Result<DetectedFormat, DetectError> {
    let file = File::open(input).map_err(DetectError::Open)?;
    let mut reader = BufReader::with_capacity(HEADER_BUFFER, file);
    let mut head = [0u8; 4];
    let filled = read_available(&mut reader, &mut head).map_err(DetectError::Read)?;

    if filled != head.len() {
        return Ok(DetectedFormat::Unrecognized {
            head: head[..filled].to_vec(),
        });
    }
    if head == ZSTD_MAGIC {
        return Ok(DetectedFormat::Zstd);
    }
    if head.starts_with(BZIP2_MAGIC) && (b'1'..=b'9').contains(&head[3]) {
        return Ok(DetectedFormat::Bzip2);
    }
    if matches!(head, [0x50..=0x5F, 0x2A, 0x4D, 0x18]) {
        // a zstd stream may start with skippable frames; the frame parser
        // itself decides whether what follows is actually valid
        return Ok(DetectedFormat::Zstd);
    }
    Ok(DetectedFormat::Unrecognized {
        head: head.to_vec(),
    })
}

fn inspect_bzip2_header(input: &Path) -> Result<ArchiveHeader, HeaderError> {
    let file = File::open(input).map_err(HeaderError::Open)?;
    let mut reader = BufReader::with_capacity(HEADER_BUFFER, file);

    // detect_format already checked the "BZh" + level-digit magic; the first
    // compressed block signature separates real streams from look-alikes
    // before anything is written.
    let mut head = [0u8; 10];
    let filled = read_available(&mut reader, &mut head)?;
    if filled < head.len()
        || !head.starts_with(BZIP2_MAGIC)
        || !(b'1'..=b'9').contains(&head[3])
        || head[4..] != BZIP2_BLOCK_MAGIC
    {
        return Err(HeaderError::Format("invalid block header"));
    }

    // block CRCs and the combined stream CRC are verified by the decoder as
    // the file is read, and a file cut short mid-block fails with an
    // unexpected end of file; the decompressed length is unknown by design,
    // so the free-space pre-check is skipped and the progress bar prints the
    // amount decoded without a total.
    Ok(ArchiveHeader {
        has_checksum: true,
        content_size: None,
    })
}

fn check_header(input: &Path) -> Option<(ArchiveFormat, ArchiveHeader)> {
    let format = match detect_format(input) {
        Ok(DetectedFormat::Zstd) => ArchiveFormat::Zstd,
        Ok(DetectedFormat::Bzip2) => ArchiveFormat::Bzip2,
        Ok(DetectedFormat::Unrecognized { head }) => {
            report_unrecognized_format(&head);
            return None;
        }
        Err(DetectError::Open(err)) => {
            error(format!("could not open the archive: {err}"));
            return None;
        }
        Err(DetectError::Read(err)) => {
            error(format!("could not read the archive header: {err}"));
            return None;
        }
    };

    let inspected = match format {
        ArchiveFormat::Zstd => inspect_zstd_header(input),
        ArchiveFormat::Bzip2 => inspect_bzip2_header(input),
    };

    match inspected {
        Ok(header) => Some((format, header)),
        Err(HeaderError::Open(err)) => {
            error(format!("could not open the archive: {err}"));
            None
        }
        Err(HeaderError::Read(err)) => {
            error(format!("could not read the archive header: {err}"));
            None
        }
        Err(HeaderError::Format(message)) => {
            report_codec_failure(format, message);
            None
        }
    }
}

fn copy_payload(
    reader: &mut impl Read,
    format: ArchiveFormat,
    writer: &mut impl Write,
    buffer: &mut [u8],
    head: &mut Vec<u8>,
    total: &mut u64,
    progress: &mut output::Progress,
) -> Result<(), CopyError> {
    loop {
        if CANCEL.load(Ordering::Relaxed) {
            return Err(CopyError::Cancelled);
        }

        let read = reader
            .read(buffer)
            .map_err(|err| CopyError::Codec(format, err.to_string()))?;
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
    format: ArchiveFormat,
    writer: &mut impl Write,
    content_size: Option<u64>,
    head: &mut Vec<u8>,
    buffer: &mut [u8],
) -> Result<u64, CopyError> {
    let file = File::open(input).map_err(|err| CopyError::Open(err.to_string()))?;
    let reader = BufReader::with_capacity(COPY_BUFFER, file);

    let mut total = 0;
    let mut progress = output::Progress::new(content_size);
    progress.update(0);

    let result = match format {
        ArchiveFormat::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::with_buffer(reader)
                .map_err(|err| CopyError::Codec(format, err.to_string()))?;
            copy_payload(
                &mut decoder,
                format,
                writer,
                buffer,
                head,
                &mut total,
                &mut progress,
            )
        }
        ArchiveFormat::Bzip2 => {
            // MultiBzDecoder verifies every block CRC, the combined stream
            // CRC and each stream's end marker while decoding, and reports
            // "decompression not finished but EOF reached" when a file was
            // cut short mid-block. Concatenated (pbzip2-style) streams are
            // decoded back to back.
            let mut decoder = bzip2::bufread::MultiBzDecoder::new(reader);
            copy_payload(
                &mut decoder,
                format,
                writer,
                buffer,
                head,
                &mut total,
                &mut progress,
            )
        }
    };
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
    crate::queue::path_key(&absolute(left)) == crate::queue::path_key(&absolute(right))
}

pub fn extract_one(
    input: &Path,
    output: &Path,
    keep_zst: bool,
    move_to_recycle_bin: bool,
) -> ExtractResult {
    let mut buffer = vec![0u8; COPY_BUFFER];
    extract_one_with_buffer(input, output, keep_zst, move_to_recycle_bin, &mut buffer)
}

pub fn extract_one_with_buffer(
    input: &Path,
    output: &Path,
    keep_zst: bool,
    move_to_recycle_bin: bool,
    buffer: &mut [u8],
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

    let Some(output_name) = output.file_name().and_then(|s| s.to_str()) else {
        error(format!("invalid demo output path: {}", output.display()));
        return ExtractResult::Failed;
    };

    if DemoFileName::parse(output_name).is_err() {
        error(format!(
            "output demo filename is invalid or reserved: {output_name}"
        ));
        return ExtractResult::Failed;
    }

    let Some((format, header)) = check_header(input) else {
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
        let decoded = decode_archive(input, format, &mut writer, content_size, &mut head, buffer);
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

    if CANCEL.load(Ordering::Relaxed) {
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

    let Some((format, header)) = check_header(input) else {
        return ExtractResult::Failed;
    };

    println!("{}", output::field("Input", input.display()));

    let _guard = ExtractingGuard::new();
    let mut head = Vec::with_capacity(DEMO_MAGIC.len());
    let mut sink = io::sink();
    let mut buffer = vec![0u8; COPY_BUFFER];

    match decode_archive(
        input,
        format,
        &mut sink,
        header.content_size,
        &mut head,
        &mut buffer,
    ) {
        Ok(total) => {
            if head.len() < DEMO_MAGIC.len() {
                report_magic_failure(&head);
                println!("{}", output::field("Result", output::bad("failed")));
                return ExtractResult::Failed;
            }

            let checksum = match format {
                ArchiveFormat::Zstd => {
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
                    "XXH64 verified"
                }
                ArchiveFormat::Bzip2 => "CRC32 verified",
            };

            println!("{}", output::field("Checksum", output::good(checksum)));
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

#[derive(Debug, thiserror::Error)]
pub enum DeleteOriginalError {
    #[error("failed to recycle file: {0}")]
    Recycle(#[from] crate::win32::Win32Error),

    #[error("failed to remove file: {0}")]
    Io(#[from] io::Error),
}

pub fn delete_original(path: &Path, use_recycle_bin: bool) -> Result<(), DeleteOriginalError> {
    if use_recycle_bin {
        win32::recycle_file(path)?;
        return Ok(());
    }

    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(DeleteOriginalError::Io(err)),
    }
}

#[must_use]
pub fn split_ext(name: &str) -> (&str, &str) {
    let base_start = name.rfind(['\\', '/']).map_or(0, |index| index + 1);
    let base = &name[base_start..];

    match base.rfind('.') {
        Some(dot) if dot > 0 => (&base[..dot], &base[dot..]),
        _ => (base, ""),
    }
}

#[must_use]
pub fn uncompressed_demo_name(input: &Path) -> DemoFileName {
    let file_name = input
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "demo.dem".to_string());

    let (stem, ext) = split_ext(&file_name);
    let name = if ext.eq_ignore_ascii_case(".zst") || ext.eq_ignore_ascii_case(".bz2") {
        stem
    } else {
        &file_name
    };

    let candidate = if name.to_ascii_lowercase().ends_with(".dem") {
        name.to_string()
    } else {
        format!("{name}.dem")
    };

    match DemoFileName::parse(&candidate) {
        Ok(valid) => valid,
        Err(_) => DemoFileName::sanitize(&candidate),
    }
}

fn name_with_suffix(stem: &str, suffix: &str, ext: &str) -> String {
    let max_stem_utf16 = MAX_FILENAME_LEN.saturating_sub(utf16_len(suffix) + utf16_len(ext));
    let mut accumulated_utf16 = 0;
    let mut truncate_byte_idx = stem.len();

    for (idx, ch) in stem.char_indices() {
        let ch_utf16_len = ch.len_utf16();
        if accumulated_utf16 + ch_utf16_len > max_stem_utf16 {
            truncate_byte_idx = idx;
            break;
        }
        accumulated_utf16 += ch_utf16_len;
    }

    let stem = &stem[..truncate_byte_idx];
    format!("{stem}{suffix}{ext}")
}

fn next_free_name_with_attempts(
    folder: &Path,
    stem: &str,
    ext: &str,
    delimiter: &str,
    attempts: u32,
) -> String {
    for number in 1..attempts {
        let candidate = name_with_suffix(stem, &format!("{delimiter}{number}"), ext);
        if !folder.join(&candidate).exists() {
            return candidate;
        }
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    name_with_suffix(stem, &format!("{delimiter}{stamp}"), ext)
}

#[must_use]
pub fn next_free_name(folder: &Path, stem: &str, ext: &str, delimiter: &str) -> String {
    next_free_name_with_attempts(folder, stem, ext, delimiter, 10_000)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractOutcome {
    pub exit_code: i32,
    pub extracted: Vec<String>,
    pub failed: usize,
    pub total: usize,
}

pub fn run_extract_mode(paths: &Paths, files: &[PathBuf], pause_after: bool) -> ExtractOutcome {
    if files.is_empty() {
        return ExtractOutcome {
            exit_code: 0,
            extracted: Vec::new(),
            failed: 0,
            total: 0,
        };
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
        return ExtractOutcome {
            exit_code: 1,
            extracted: Vec::new(),
            failed: files.len(),
            total: files.len(),
        };
    };

    run_extract_batch_outcome(files, &csgo, &source, &config, pause_after)
}

pub fn run_extract_batch(
    files: &[PathBuf],
    csgo: &Path,
    source: &str,
    config: &Config,
    pause_after: bool,
) -> i32 {
    run_extract_batch_outcome(files, csgo, source, config, pause_after).exit_code
}

pub fn run_extract_batch_outcome(
    files: &[PathBuf],
    csgo: &Path,
    source: &str,
    config: &Config,
    pause_after: bool,
) -> ExtractOutcome {
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
    let mut buffer = vec![0u8; COPY_BUFFER];

    for (index, input) in files.iter().enumerate() {
        if CANCEL.load(Ordering::Relaxed) {
            output::cancelled();
            return ExtractOutcome {
                exit_code: 130,
                extracted,
                failed: failed.len(),
                total,
            };
        }

        let out_name = if !config.rename_demo {
            let natural = uncompressed_demo_name(input);
            if total == 1 {
                println!("{}", output::field("Input", input.display()));
                if config.overwrite_single_file_extraction || !csgo.join(&natural).exists() {
                    natural
                } else {
                    let (stem, ext) = split_ext(natural.as_str());
                    let candidate = next_free_name(csgo, stem, ext, &config.delimiter);
                    match DemoFileName::parse(&candidate) {
                        Ok(valid) => valid,
                        Err(_) => DemoFileName::sanitize(&candidate),
                    }
                }
            } else {
                println!(
                    "{}",
                    output::title(format!("[{}/{}] {}", index + 1, total, input.display()))
                );
                if !csgo.join(&natural).exists() {
                    natural
                } else {
                    let (stem, ext) = split_ext(natural.as_str());
                    let candidate = next_free_name(csgo, stem, ext, &config.delimiter);
                    match DemoFileName::parse(&candidate) {
                        Ok(valid) => valid,
                        Err(_) => DemoFileName::sanitize(&candidate),
                    }
                }
            }
        } else if total == 1 {
            println!("{}", output::field("Input", input.display()));
            if config.overwrite_single_file_extraction || !csgo.join(target).exists() {
                match DemoFileName::parse(target) {
                    Ok(valid) => valid,
                    Err(_) => DemoFileName::sanitize(target),
                }
            } else {
                let candidate = next_free_name(csgo, stem, ext, &config.delimiter);
                match DemoFileName::parse(&candidate) {
                    Ok(valid) => valid,
                    Err(_) => DemoFileName::sanitize(&candidate),
                }
            }
        } else {
            println!(
                "{}",
                output::title(format!("[{}/{}] {}", index + 1, total, input.display()))
            );
            let candidate = next_free_name(csgo, stem, ext, &config.delimiter);
            match DemoFileName::parse(&candidate) {
                Ok(valid) => valid,
                Err(_) => DemoFileName::sanitize(&candidate),
            }
        };
        let output_path = csgo.join(out_name.as_str());

        match extract_one_with_buffer(
            input,
            &output_path,
            keep_zst,
            move_to_recycle_bin,
            &mut buffer,
        ) {
            ExtractResult::Ok => extracted.push(out_name.into_inner()),
            ExtractResult::Failed => failed.push(input),
            ExtractResult::Cancelled => {
                output::cancelled();
                return ExtractOutcome {
                    exit_code: 130,
                    extracted,
                    failed: failed.len(),
                    total,
                };
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
            println!("  {}", output::bad(input.to_string_lossy()));
        }
    }

    if !extracted.is_empty() {
        let stems: Vec<&str> = extracted.iter().map(|name| split_ext(name).0).collect();

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

    ExtractOutcome {
        exit_code: if failed.is_empty() { 0 } else { 1 },
        extracted,
        failed: failed.len(),
        total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_archive_format_traits() {
        use std::collections::HashSet;

        assert_eq!(ArchiveFormat::Zstd.to_string(), "Zstandard");
        assert_eq!(ArchiveFormat::Bzip2.to_string(), "Bzip2");
        assert_eq!(format!("{}", ArchiveFormat::Zstd), "Zstandard");

        let mut set = HashSet::new();
        set.insert(ArchiveFormat::Zstd);
        set.insert(ArchiveFormat::Bzip2);
        assert!(set.contains(&ArchiveFormat::Zstd));
        assert_eq!(set.len(), 2);
    }

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

        let (_, header) = check_header(&input).expect("zstd header");

        assert!(header.has_checksum);
        assert_eq!(header.content_size, Some(payload.len() as u64));
    }

    #[test]
    fn frame_header_reads_a_two_byte_content_size() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("demo.dem.zst");
        let payload = vec![0xAB; 4096];
        fs::write(&input, zstd_frame(&payload, true)).unwrap();

        let (_, header) = check_header(&input).expect("zstd header");

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

        let (_, header) = check_header(&input).expect("zstd header");

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

        let (_, header) = check_header(&input).expect("zstd header");

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

        let (_, header) = check_header(&input).expect("zstd header");

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
        assert_eq!(
            name,
            format!("{}_1234567890.dem", "a".repeat(MAX_FILENAME_LEN - 15))
        );
    }

    #[test]
    fn name_with_suffix_handles_short_and_empty_stem() {
        assert_eq!(name_with_suffix("match", "_1", ".dem"), "match_1.dem");
        assert_eq!(name_with_suffix("", "_1", ".dem"), "_1.dem");
    }

    #[test]
    fn name_with_suffix_handles_surrogate_pairs_at_boundary() {
        // ext: ".dem" (4 units), suffix: "_1" (2 units) -> max_stem_utf16 = 255 - 6 = 249
        // 248 'a's (248 units) + "🦀" (2 units) = 250 units > 249, so "🦀" must be omitted
        let stem = format!("{}🦀", "a".repeat(248));
        let name = name_with_suffix(&stem, "_1", ".dem");
        assert_eq!(name, format!("{}_1.dem", "a".repeat(248)));
        assert!(utf16_len(&name) <= MAX_FILENAME_LEN);

        // 247 'a's (247 units) + "🦀" (2 units) = 249 units <= 249, so "🦀" must be kept
        let stem_fits = format!("{}🦀", "a".repeat(247));
        let name_fits = name_with_suffix(&stem_fits, "_1", ".dem");
        assert_eq!(name_fits, format!("{}🦀_1.dem", "a".repeat(247)));
        assert_eq!(utf16_len(&name_fits), MAX_FILENAME_LEN);
    }

    #[test]
    fn name_with_suffix_truncates_stem_completely_when_suffix_and_ext_exceed_limit() {
        let long_suffix = "s".repeat(MAX_FILENAME_LEN);
        let name = name_with_suffix("demo_stem", &long_suffix, ".dem");
        assert_eq!(name, format!("{long_suffix}.dem"));
    }

    #[test]
    fn next_free_name_falls_back_to_a_timestamp() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("1_1.dem"), b"x").unwrap();
        fs::write(directory.path().join("1_2.dem"), b"x").unwrap();

        let name = next_free_name_with_attempts(directory.path(), "1", ".dem", "_", 3);

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

    fn bzip2_frame(payload: &[u8]) -> Vec<u8> {
        let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn bzip2_header_is_detected_with_inherent_checksums() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("demo.dem.bz2");
        let payload = b"PBDEMS2\x00bzip demo payload";
        fs::write(&input, bzip2_frame(payload)).unwrap();

        let (format, header) = check_header(&input).expect("bzip2 header");

        assert_eq!(format, ArchiveFormat::Bzip2);
        assert!(header.has_checksum);
        assert_eq!(header.content_size, None);
    }

    #[test]
    fn bzip2_header_rejects_lookalikes() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("lookalike.bz2");

        fs::write(&input, b"BZh9not-a-block-signature").unwrap();
        assert!(check_header(&input).is_none());

        fs::write(&input, b"BZh0").unwrap();
        assert!(check_header(&input).is_none());

        fs::write(&input, b"BZh").unwrap();
        assert!(check_header(&input).is_none());
    }

    #[test]
    fn paths_refer_to_same_file_matches_case_insensitively_in_fallback() {
        let left = Path::new(r"C:\nonexistent_test_dir_fallback\demo.dem");
        let right = Path::new(r"c:\NONEXISTENT_TEST_DIR_FALLBACK\DEMO.dem");
        assert!(paths_refer_to_same_file(left, right));

        let different = Path::new(r"c:\NONEXISTENT_TEST_DIR_FALLBACK\OTHER.dem");
        assert!(!paths_refer_to_same_file(left, different));
    }

    #[test]
    fn unrecognized_headers_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("garbage.bin");

        fs::write(&input, b"\x00\x01\x02\x03\x04").unwrap();
        assert!(check_header(&input).is_none());
    }

    #[test]
    fn uncompressed_demo_name_strips_archive_extension_and_ensures_dem() {
        assert_eq!(
            uncompressed_demo_name(Path::new(r"C:\demos\match-12345.dem.zst")),
            "match-12345.dem"
        );
        assert_eq!(
            uncompressed_demo_name(Path::new(r"C:\demos\match-12345.dem.bz2")),
            "match-12345.dem"
        );
        assert_eq!(
            uncompressed_demo_name(Path::new(r"C:\demos\match-12345.zst")),
            "match-12345.dem"
        );
        assert_eq!(
            uncompressed_demo_name(Path::new(r"C:\demos\match-12345.dem")),
            "match-12345.dem"
        );
        assert_eq!(
            uncompressed_demo_name(Path::new(r"C:\demos\MATCH.DEM.ZST")),
            "MATCH.DEM"
        );
        assert_eq!(
            uncompressed_demo_name(Path::new(r"C:\demos\CON.dem.zst")),
            "_CON.dem"
        );
        assert_eq!(
            uncompressed_demo_name(Path::new(r"C:\demos\nul.dem.bz2")),
            "_nul.dem"
        );
        assert_eq!(
            uncompressed_demo_name(Path::new(r"C:\demos\match.dem:stream.zst")),
            "match.dem_stream.dem"
        );
        assert_eq!(
            uncompressed_demo_name(Path::new(r"C:\demos\COM1.zst")),
            "_COM1.dem"
        );
    }

    #[test]
    fn inspect_zstd_header_rejects_empty_non_terminal_raw_block() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("empty_raw_block.dem.zst");

        let mut bytes = ZSTD_MAGIC.to_vec();
        bytes.push(0x20);
        bytes.push(0x00);
        bytes.extend_from_slice(&[0x00, 0x00, 0x00]);
        let terminal_block = 1u32 | (1u32 << 3);
        bytes.extend_from_slice(&terminal_block.to_le_bytes()[..3]);
        bytes.push(0x42);

        fs::write(&input, bytes).unwrap();

        match inspect_zstd_header(&input) {
            Err(HeaderError::Format(msg)) => {
                assert_eq!(msg, "empty non-terminal raw block detected");
            }
            other => panic!("expected Format error, got {other:?}"),
        }
    }

    #[test]
    fn inspect_zstd_header_rejects_block_size_exceeding_128kb() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("oversized_block.dem.zst");

        let mut bytes = ZSTD_MAGIC.to_vec();
        bytes.push(0x20);
        bytes.push(0x00);
        let oversized_block = 1u32 | ((128 * 1024 + 1) << 3);
        bytes.extend_from_slice(&oversized_block.to_le_bytes()[..3]);

        fs::write(&input, bytes).unwrap();

        match inspect_zstd_header(&input) {
            Err(HeaderError::Format(msg)) => {
                assert_eq!(
                    msg,
                    "block size exceeds maximum allowed Zstandard block size (128 KB)"
                );
            }
            other => panic!("expected Format error, got {other:?}"),
        }
    }

    #[test]
    fn detect_format_reports_open_error_for_missing_file() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.dem.zst");
        match detect_format(&missing) {
            Err(DetectError::Open(_)) => {}
            other => panic!("expected DetectError::Open, got {other:?}"),
        }
    }

    #[test]
    fn check_header_handles_missing_file_without_panic() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.dem.zst");
        assert!(check_header(&missing).is_none());
    }

    #[test]
    fn extract_one_with_buffer_reuses_buffer_across_calls() {
        let directory = tempfile::tempdir().unwrap();
        let input1 = directory.path().join("first.dem.zst");
        let output1 = directory.path().join("first.dem");
        let payload1 = b"PBDEMS2\x00first payload data";
        fs::write(&input1, zstd_frame(payload1, true)).unwrap();

        let input2 = directory.path().join("second.dem.zst");
        let output2 = directory.path().join("second.dem");
        let payload2 = b"PBDEMS2\x00second payload data with different length";
        fs::write(&input2, zstd_frame(payload2, true)).unwrap();

        let mut buffer = vec![0u8; COPY_BUFFER];
        let res1 = extract_one_with_buffer(&input1, &output1, true, false, &mut buffer);
        assert_eq!(res1, ExtractResult::Ok);
        assert_eq!(fs::read(&output1).unwrap(), payload1);

        let res2 = extract_one_with_buffer(&input2, &output2, true, false, &mut buffer);
        assert_eq!(res2, ExtractResult::Ok);
        assert_eq!(fs::read(&output2).unwrap(), payload2);
    }
}

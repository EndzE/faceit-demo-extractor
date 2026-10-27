use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::output;
use crate::{CANCEL, Paths, WORKER_MODE, config, extract};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_WINDOW: Duration = Duration::from_millis(700);
const MAX_TOTAL: Duration = Duration::from_secs(60);
const STARTUP_POLL: Duration = Duration::from_millis(10);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_LOCK_RETRY: Duration = Duration::from_millis(500);
const CANCEL_GRACE: Duration = Duration::from_millis(1000);

struct WorkerModeGuard;

impl WorkerModeGuard {
    fn new() -> Self {
        WORKER_MODE.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for WorkerModeGuard {
    fn drop(&mut self) {
        WORKER_MODE.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ContextMenuError {
    #[error("{0}")]
    Usage(String),
    #[error("{0}")]
    Failure(String),
}

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("could not open the worker lock: {0}")]
    Lock(#[from] io::Error),

    #[error("faceit-dem.exe was not found next to this executable.\nExpected: {}", .0.display())]
    MissingExecutable(PathBuf),

    #[error("could not spawn worker process: {0}")]
    Spawn(#[from] crate::win32::Win32Error),
}

pub fn ensure_queue_dir(paths: &Paths) -> io::Result<()> {
    fs::create_dir_all(&paths.queue_dir)
}

fn is_path_padding(unit: u16) -> bool {
    unit == '"' as u16 || unit <= 0x20
}

fn normalize_path(file: &OsStr) -> Option<PathBuf> {
    let units: Vec<u16> = file.encode_wide().collect();
    let start = units.iter().position(|&unit| !is_path_padding(unit))?;
    let end = units.iter().rposition(|&unit| !is_path_padding(unit))? + 1;
    let trimmed = OsString::from_wide(&units[start..end]);
    std::path::absolute(Path::new(&trimmed)).ok()
}

pub(crate) fn path_key(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .map(|unit| {
            if unit <= 0x7F {
                u16::from((unit as u8).to_ascii_lowercase())
            } else {
                unit
            }
        })
        .collect()
}

const UTF16_LE_BOM: [u8; 2] = [0xFF, 0xFE];

fn encode_queue_path(path: &Path) -> Vec<u8> {
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    let mut bytes = Vec::with_capacity(UTF16_LE_BOM.len() + units.len() * 2);
    bytes.extend_from_slice(&UTF16_LE_BOM);
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

fn decode_queue_path(bytes: &[u8]) -> Option<PathBuf> {
    if let Some(rest) = bytes.strip_prefix(&UTF16_LE_BOM) {
        if rest.len() % 2 != 0 {
            return None;
        }
        let (pairs, _) = rest.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        let path = PathBuf::from(OsString::from_wide(&units));
        return (!path.as_os_str().is_empty()).then_some(path);
    }

    let text = String::from_utf8_lossy(strip_utf8_bom(bytes))
        .trim()
        .to_string();
    if text.is_empty() {
        None
    } else {
        Some(PathBuf::from(text))
    }
}

fn write_queue_entry(paths: &Paths, path: &Path) -> io::Result<()> {
    let mut temp = tempfile::Builder::new()
        .prefix("q")
        .suffix(".tmp")
        .tempfile_in(&paths.queue_dir)?;

    temp.write_all(&encode_queue_path(path))?;
    temp.flush()?;

    let name = temp
        .path()
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "q".to_string());
    let final_path = paths.queue_dir.join(format!("{name}.path"));

    temp.persist(&final_path).map_err(|err| err.error)?;
    Ok(())
}

fn queued_path_keys(paths: &Paths) -> io::Result<HashSet<Vec<u16>>> {
    let mut keys = HashSet::new();
    let directory = match fs::read_dir(&paths.queue_dir) {
        Ok(directory) => directory,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(keys),
        Err(err) => return Err(err),
    };

    for entry in directory.flatten() {
        let path = entry.path();
        if !is_path_file(&path) {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        if let Some(path) = decode_queue_path(&bytes) {
            keys.insert(path_key(&path));
        }
    }

    Ok(keys)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct EnqueueOutcome {
    pub queued: usize,
    pub duplicates: usize,
    pub failures: Vec<String>,
}

pub fn enqueue_paths(paths: &Paths, files: &[OsString]) -> io::Result<EnqueueOutcome> {
    ensure_queue_dir(paths)?;

    let mut keys = queued_path_keys(paths)?;
    let mut outcome = EnqueueOutcome::default();

    for file in files {
        let Some(path) = normalize_path(file) else {
            outcome
                .failures
                .push(format!("invalid path: {}", file.to_string_lossy()));
            continue;
        };

        let key = path_key(&path);
        if keys.contains(&key) {
            outcome.duplicates += 1;
            continue;
        }

        match write_queue_entry(paths, &path) {
            Ok(()) => {
                keys.insert(key);
                outcome.queued += 1;
            }
            Err(err) => outcome
                .failures
                .push(format!("could not queue {}: {err}", path.display())),
        }
    }

    Ok(outcome)
}

fn is_path_file(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("path"))
}

fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

pub fn take_queue_files(paths: &Paths) -> Vec<PathBuf> {
    let Ok(directory) = fs::read_dir(&paths.queue_dir) else {
        return Vec::new();
    };

    let mut entries: Vec<(SystemTime, PathBuf)> = Vec::new();

    for entry in directory.flatten() {
        let path = entry.path();
        if !is_path_file(&path) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        entries.push((modified, path));
    }

    entries.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    let mut keys = HashSet::new();
    let mut paths_out = Vec::new();

    for (_, entry_path) in entries {
        let Ok(bytes) = fs::read(&entry_path) else {
            continue;
        };
        let Some(path) = decode_queue_path(&bytes) else {
            let _ = fs::remove_file(&entry_path);
            continue;
        };
        let _ = fs::remove_file(&entry_path);
        if keys.insert(path_key(&path)) {
            paths_out.push(path);
        }
    }

    paths_out
}

pub fn has_queued_files(paths: &Paths) -> bool {
    fs::read_dir(&paths.queue_dir)
        .map(|directory| directory.flatten().any(|entry| is_path_file(&entry.path())))
        .unwrap_or(false)
}

fn discard_queued_files(paths: &Paths) {
    let deadline = Instant::now() + CANCEL_GRACE;
    loop {
        let _ = take_queue_files(paths);
        if Instant::now() >= deadline {
            break;
        }
        sleep(POLL_INTERVAL);
    }
}

pub fn collect_queued_files(paths: &Paths) -> Vec<PathBuf> {
    collect_queued_files_with_timing(paths, IDLE_WINDOW, Duration::ZERO, MAX_TOTAL)
}

pub fn collect_queued_files_with_timing(
    paths: &Paths,
    idle: Duration,
    initial_timeout: Duration,
    max_total: Duration,
) -> Vec<PathBuf> {
    let mut collected = Vec::new();
    let mut seen = HashSet::new();
    let start = Instant::now();
    let mut last = start;

    loop {
        if CANCEL.load(Ordering::SeqCst) {
            break;
        }

        let batch = take_queue_files(paths);

        if !batch.is_empty() {
            for path in batch {
                if seen.insert(path_key(&path)) {
                    collected.push(path);
                }
            }
            last = Instant::now();
            sleep(POLL_INTERVAL);
            continue;
        }

        let now = Instant::now();

        if !collected.is_empty() && now.duration_since(last) >= idle {
            break;
        }
        if collected.is_empty() && now.duration_since(start) >= initial_timeout {
            break;
        }
        if now.duration_since(start) >= max_total {
            break;
        }

        sleep(POLL_INTERVAL);
    }

    collected
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn open_worker_lock(paths: &Paths) -> io::Result<File> {
    ensure_queue_dir(paths)?;
    open_lock_file(&paths.worker_lock())
}

fn open_spawn_lock(paths: &Paths) -> io::Result<File> {
    ensure_queue_dir(paths)?;
    open_lock_file(&paths.spawn_lock())
}

pub fn try_acquire_worker_lock(paths: &Paths) -> io::Result<Option<File>> {
    let file = open_worker_lock(paths)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

fn acquire_worker_lock(paths: &Paths) -> io::Result<Option<File>> {
    let file = open_worker_lock(paths)?;
    let deadline = Instant::now() + WORKER_LOCK_RETRY;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => sleep(STARTUP_POLL),
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(err)) => return Err(err),
        }
    }
}

pub fn try_acquire_spawn_lock(paths: &Paths) -> io::Result<Option<File>> {
    let file = open_spawn_lock(paths)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

pub fn worker_lock_is_free(paths: &Paths) -> io::Result<bool> {
    let Some(file) = try_acquire_worker_lock(paths)? else {
        return Ok(false);
    };
    let _ = file.unlock();
    Ok(true)
}

pub fn write_worker_pid(paths: &Paths) {
    let _ = write_worker_pid_atomically(paths);
}

fn write_worker_pid_atomically(paths: &Paths) -> io::Result<()> {
    ensure_queue_dir(paths)?;

    let mut temp = tempfile::Builder::new()
        .prefix(".worker-pid-")
        .suffix(".tmp")
        .tempfile_in(&paths.queue_dir)?;

    temp.write_all(std::process::id().to_string().as_bytes())?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(paths.worker_pid()).map_err(|err| err.error)?;

    Ok(())
}

pub fn read_worker_pid(paths: &Paths) -> Option<u32> {
    fs::read_to_string(paths.worker_pid())
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn cleanup_worker_pid(paths: &Paths) {
    let _ = fs::remove_file(paths.worker_pid());
}

fn cleanup_own_worker_pid(paths: &Paths) {
    if read_worker_pid(paths) == Some(std::process::id()) {
        cleanup_worker_pid(paths);
    }
}

pub fn worker_state(paths: &Paths) -> Result<(bool, Option<u32>), QueueError> {
    match worker_lock_is_free(paths) {
        Ok(true) => Ok((false, None)),
        Ok(false) => Ok((true, read_worker_pid(paths))),
        Err(err) => Err(QueueError::Lock(err)),
    }
}

pub fn spawn_worker(paths: &Paths) -> Result<(), QueueError> {
    let exe = paths.console_exe();
    if !exe.is_file() {
        return Err(QueueError::MissingExecutable(exe));
    }

    if config::load_config(&paths.config).notifications_instead_of_console {
        crate::win32::spawn_hidden_console(&exe, &["--queue-worker"])?;
    } else {
        crate::win32::spawn_new_console(&exe, &["--queue-worker"])?;
    }
    Ok(())
}

fn notify_worker_error(message: &str) {
    let _ = crate::win32::notify("Extraction failed", message);
}

pub fn format_playdemo_commands(extracted: &[String]) -> String {
    let stems: Vec<String> = extracted
        .iter()
        .map(|name| extract::split_ext(name).0)
        .collect();

    if stems.len() <= 4 {
        stems
            .iter()
            .map(|stem| format!("playdemo \"{stem}\""))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        let first_three = stems
            .iter()
            .take(3)
            .map(|stem| format!("playdemo \"{stem}\""))
            .collect::<Vec<_>>()
            .join("\n");
        let remaining = stems.len() - 3;
        let others = stems
            .iter()
            .skip(3)
            .take(3)
            .map(|stem| format!("\"{stem}\""))
            .collect::<Vec<_>>()
            .join(", ");
        if remaining > 3 {
            format!(
                "{first_three}\nOther: {others}, ... (+{} more)",
                remaining - 3
            )
        } else {
            format!("{first_three}\nOther: {others}")
        }
    }
}

pub fn extraction_notification_content(outcome: &extract::ExtractOutcome) -> (String, String) {
    if outcome.exit_code == 130 {
        return (
            "Extraction cancelled".to_string(),
            if outcome.extracted.is_empty() {
                "The extraction was cancelled.".to_string()
            } else {
                format!(
                    "The extraction was cancelled.\n\nExtracted before cancelling:\n{}",
                    format_playdemo_commands(&outcome.extracted)
                )
            },
        );
    }

    if outcome.extracted.is_empty() {
        return (
            "Extraction failed".to_string(),
            if outcome.total == 1 {
                "The archive could not be extracted. It may be damaged or the CS2 folder is unavailable."
                    .to_string()
            } else {
                format!("None of the {} archives could be extracted.", outcome.total)
            },
        );
    }

    let commands = format_playdemo_commands(&outcome.extracted);

    if outcome.failed > 0 {
        let title = format!(
            "Extraction complete with errors ({}/{} demos)",
            outcome.extracted.len(),
            outcome.total
        );
        let plural = if outcome.failed == 1 { "" } else { "s" };
        let message = format!("{commands}\n\n({} archive{plural} failed)", outcome.failed);
        (title, message)
    } else if outcome.total == 1 {
        ("Extraction complete".to_string(), commands)
    } else {
        (
            format!("Extraction complete ({} demos)", outcome.extracted.len()),
            commands,
        )
    }
}

fn notify_extraction_result(outcome: &extract::ExtractOutcome) {
    let (title, message) = extraction_notification_content(outcome);
    let _ = crate::win32::notify(&title, &message);
}

fn pause_worker(notifications: bool) {
    if !notifications {
        output::pause("Press Enter to close...");
    }
}

fn failure_result(failures: Option<String>) -> Result<(), ContextMenuError> {
    match failures {
        Some(message) => Err(ContextMenuError::Failure(message)),
        None => Ok(()),
    }
}

fn combine_failure(failures: Option<String>, message: impl Into<String>) -> ContextMenuError {
    let message = message.into();
    match failures {
        Some(failures) => ContextMenuError::Failure(format!("{failures}\n{message}")),
        None => ContextMenuError::Failure(message),
    }
}

pub fn handle_context_menu(paths: &Paths, files: &[OsString]) -> Result<(), ContextMenuError> {
    if files.is_empty() {
        return Err(ContextMenuError::Usage(
            "no file was provided for --context-menu".to_string(),
        ));
    }

    let outcome = enqueue_paths(paths, files)
        .map_err(|err| ContextMenuError::Failure(format!("could not queue file(s): {err}")))?;
    let partial_failures = (!outcome.failures.is_empty()).then(|| outcome.failures.join("\n"));

    if outcome.queued == 0 && outcome.duplicates == 0 {
        return Err(match partial_failures {
            Some(message) => ContextMenuError::Failure(message),
            None => ContextMenuError::Usage("no valid files were queued".to_string()),
        });
    }

    let spawn_guard = match try_acquire_spawn_lock(paths) {
        Ok(Some(guard)) => guard,
        Ok(None) => return failure_result(partial_failures),
        Err(err) => {
            return Err(combine_failure(
                partial_failures,
                format!("could not open the spawn lock: {err}"),
            ));
        }
    };

    match worker_lock_is_free(paths) {
        Ok(true) => {}
        Ok(false) => return failure_result(partial_failures),
        Err(err) => {
            return Err(combine_failure(
                partial_failures,
                format!("could not open the worker lock: {err}"),
            ));
        }
    }

    cleanup_worker_pid(paths);

    if let Err(message) = spawn_worker(paths) {
        return Err(combine_failure(
            partial_failures,
            format!("could not start the worker: {message}"),
        ));
    }

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            break;
        }
        if read_worker_pid(paths).is_some() || !has_queued_files(paths) {
            break;
        }
        sleep(STARTUP_POLL);
    }

    drop(spawn_guard);
    failure_result(partial_failures)
}

pub fn run_worker(paths: &Paths) -> i32 {
    let worker_mode = WorkerModeGuard::new();
    let notifications = config::load_config(&paths.config).notifications_instead_of_console;

    if let Err(err) = ensure_queue_dir(paths) {
        output::error(format!("could not create the queue directory: {err}"));
        if notifications {
            notify_worker_error(&format!("Could not prepare the extraction queue: {err}"));
        }
        drop(worker_mode);
        pause_worker(notifications);
        return 1;
    }

    let lock = match acquire_worker_lock(paths) {
        Ok(Some(lock)) => lock,
        Ok(None) => return 0,
        Err(err) => {
            output::error(format!("could not open the worker lock: {err}"));
            if notifications {
                notify_worker_error(&format!("Could not start the extraction worker: {err}"));
            }
            drop(worker_mode);
            pause_worker(notifications);
            return 1;
        }
    };

    println!("{}", output::banner());
    write_worker_pid(paths);

    let mut any_failed = false;
    let mut processed_any = false;

    loop {
        let files = collect_queued_files(paths);

        if CANCEL.load(Ordering::SeqCst) {
            output::cancelled();
            discard_queued_files(paths);
            cleanup_own_worker_pid(paths);
            drop(lock);
            return 130;
        }

        if files.is_empty() {
            if has_queued_files(paths) {
                continue;
            }

            cleanup_own_worker_pid(paths);
            drop(lock);
            break;
        }

        processed_any = true;
        let outcome = extract::run_extract_mode(paths, &files, false);

        if notifications {
            notify_extraction_result(&outcome);
        }

        if outcome.exit_code == 130 {
            discard_queued_files(paths);
            cleanup_own_worker_pid(paths);
            drop(lock);
            return 130;
        }
        if outcome.exit_code != 0 {
            any_failed = true;
        }
    }

    if !processed_any {
        println!("  {}", output::muted("No queued files were found."));
    }

    drop(worker_mode);
    pause_worker(notifications);

    if any_failed { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, OPEN_EXISTING,
    };
    use windows::core::PCWSTR;

    use super::*;

    struct NoReadHandle(HANDLE);

    impl Drop for NoReadHandle {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a valid open file handle created in `deny_read_sharing`.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn deny_read_sharing(path: &Path) -> NoReadHandle {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `wide` is null-terminated UTF-16; null security attributes and template handle are valid.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .unwrap()
        };
        NoReadHandle(handle)
    }

    #[test]
    fn unreadable_queue_entries_are_kept() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::for_tests(directory.path());
        fs::create_dir_all(&paths.queue_dir).unwrap();
        let entry_path = paths.queue_dir.join("locked.path");
        fs::write(&entry_path, r"C:\demo.zst").unwrap();

        let denied = deny_read_sharing(&entry_path);
        assert!(fs::read(&entry_path).is_err());
        assert!(take_queue_files(&paths).is_empty());
        drop(denied);

        assert_eq!(
            take_queue_files(&paths),
            vec![PathBuf::from(r"C:\demo.zst")]
        );
        assert!(!entry_path.exists());
    }

    #[test]
    fn worker_lock_acquisition_waits_out_a_probe() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::for_tests(directory.path());
        let probe = try_acquire_worker_lock(&paths)
            .unwrap()
            .expect("probe lock");

        let (started_tx, started_rx) = mpsc::channel();
        let worker_paths = paths.clone();
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            acquire_worker_lock(&worker_paths).unwrap()
        });

        started_rx.recv().unwrap();
        thread::sleep(Duration::from_millis(400));
        drop(probe);

        assert!(worker.join().unwrap().is_some());
    }

    #[test]
    fn format_playdemo_commands_formats_single_and_batch() {
        let single = vec!["1.dem".to_string()];
        assert_eq!(format_playdemo_commands(&single), "playdemo \"1\"");

        let two = vec!["1.dem".to_string(), "1_1.dem".to_string()];
        assert_eq!(
            format_playdemo_commands(&two),
            "playdemo \"1\"\nplaydemo \"1_1\""
        );

        let three = vec![
            "1.dem".to_string(),
            "1_1.dem".to_string(),
            "1_2.dem".to_string(),
        ];
        assert_eq!(
            format_playdemo_commands(&three),
            "playdemo \"1\"\nplaydemo \"1_1\"\nplaydemo \"1_2\""
        );

        let five = vec![
            "1.dem".to_string(),
            "1_1.dem".to_string(),
            "1_2.dem".to_string(),
            "1_3.dem".to_string(),
            "1_4.dem".to_string(),
        ];
        assert_eq!(
            format_playdemo_commands(&five),
            "playdemo \"1\"\nplaydemo \"1_1\"\nplaydemo \"1_2\"\nOther: \"1_3\", \"1_4\""
        );

        let seven = vec![
            "1.dem".to_string(),
            "1_1.dem".to_string(),
            "1_2.dem".to_string(),
            "1_3.dem".to_string(),
            "1_4.dem".to_string(),
            "1_5.dem".to_string(),
            "1_6.dem".to_string(),
        ];
        assert_eq!(
            format_playdemo_commands(&seven),
            "playdemo \"1\"\nplaydemo \"1_1\"\nplaydemo \"1_2\"\nOther: \"1_3\", \"1_4\", \"1_5\", ... (+1 more)"
        );
    }

    #[test]
    fn extraction_notification_content_formats_various_outcomes() {
        use crate::extract::ExtractOutcome;

        let single_ok = ExtractOutcome {
            exit_code: 0,
            extracted: vec!["1.dem".to_string()],
            failed: 0,
            total: 1,
        };
        let (title, message) = extraction_notification_content(&single_ok);
        assert_eq!(title, "Extraction complete");
        assert_eq!(message, "playdemo \"1\"");

        let batch_ok = ExtractOutcome {
            exit_code: 0,
            extracted: vec![
                "1.dem".to_string(),
                "1_1.dem".to_string(),
                "1_2.dem".to_string(),
            ],
            failed: 0,
            total: 3,
        };
        let (title, message) = extraction_notification_content(&batch_ok);
        assert_eq!(title, "Extraction complete (3 demos)");
        assert_eq!(
            message,
            "playdemo \"1\"\nplaydemo \"1_1\"\nplaydemo \"1_2\""
        );

        let partial_fail = ExtractOutcome {
            exit_code: 1,
            extracted: vec!["1.dem".to_string(), "1_1.dem".to_string()],
            failed: 1,
            total: 3,
        };
        let (title, message) = extraction_notification_content(&partial_fail);
        assert_eq!(title, "Extraction complete with errors (2/3 demos)");
        assert_eq!(
            message,
            "playdemo \"1\"\nplaydemo \"1_1\"\n\n(1 archive failed)"
        );

        let all_failed_single = ExtractOutcome {
            exit_code: 1,
            extracted: vec![],
            failed: 1,
            total: 1,
        };
        let (title, message) = extraction_notification_content(&all_failed_single);
        assert_eq!(title, "Extraction failed");
        assert!(message.contains("archive could not be extracted"));

        let all_failed_batch = ExtractOutcome {
            exit_code: 1,
            extracted: vec![],
            failed: 3,
            total: 3,
        };
        let (title, message) = extraction_notification_content(&all_failed_batch);
        assert_eq!(title, "Extraction failed");
        assert_eq!(message, "None of the 3 archives could be extracted.");

        let cancelled = ExtractOutcome {
            exit_code: 130,
            extracted: vec!["1.dem".to_string()],
            failed: 0,
            total: 2,
        };
        let (title, message) = extraction_notification_content(&cancelled);
        assert_eq!(title, "Extraction cancelled");
        assert!(message.contains("playdemo \"1\""));
    }
}

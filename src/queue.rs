use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::output;
use crate::{CANCEL, Paths, WORKER_MODE, extract};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_WINDOW: Duration = Duration::from_millis(700);
const INITIAL_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_TOTAL: Duration = Duration::from_secs(60);
const STARTUP_POLL: Duration = Duration::from_millis(10);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
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

pub enum ContextMenuError {
    Usage(String),
    Failure(String),
}

pub fn ensure_queue_dir(paths: &Paths) -> io::Result<()> {
    fs::create_dir_all(&paths.queue_dir)
}

fn normalize_path(file: &OsStr) -> Option<PathBuf> {
    let raw = file.to_string_lossy();
    let trimmed = raw.trim().trim_matches('"').trim();
    if trimmed.is_empty() {
        return None;
    }
    std::path::absolute(trimmed).ok()
}

fn write_queue_entry(paths: &Paths, path: &Path) -> io::Result<()> {
    let mut temp = tempfile::Builder::new()
        .prefix("q")
        .suffix(".tmp")
        .tempfile_in(&paths.queue_dir)?;

    temp.write_all(path.to_string_lossy().as_bytes())?;
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

pub fn enqueue_paths(paths: &Paths, files: &[OsString]) -> io::Result<usize> {
    ensure_queue_dir(paths)?;

    let mut queued = 0;
    for file in files {
        let Some(path) = normalize_path(file) else {
            continue;
        };
        if write_queue_entry(paths, &path).is_ok() {
            queued += 1;
        }
    }

    Ok(queued)
}

fn is_path_file(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("path"))
}

fn strip_bom(bytes: &[u8]) -> &[u8] {
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

    let mut paths_out = Vec::new();

    for (_, path) in entries {
        let raw = fs::read(&path).ok();
        let _ = fs::remove_file(&path);

        let Some(bytes) = raw else {
            continue;
        };
        let text = String::from_utf8_lossy(strip_bom(&bytes))
            .trim()
            .to_string();

        if !text.is_empty() {
            paths_out.push(PathBuf::from(text));
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
    collect_queued_files_with_timing(paths, IDLE_WINDOW, INITIAL_TIMEOUT, MAX_TOTAL)
}

pub fn collect_queued_files_with_timing(
    paths: &Paths,
    idle: Duration,
    initial_timeout: Duration,
    max_total: Duration,
) -> Vec<PathBuf> {
    let mut collected = Vec::new();
    let start = Instant::now();
    let mut last = start;

    loop {
        if CANCEL.load(Ordering::SeqCst) {
            break;
        }

        let batch = take_queue_files(paths);

        if !batch.is_empty() {
            collected.extend(batch);
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

pub fn try_acquire_worker_lock(paths: &Paths) -> Option<File> {
    let file = open_worker_lock(paths).ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(_) => None,
    }
}

pub fn try_acquire_spawn_lock(paths: &Paths) -> Option<File> {
    let file = open_spawn_lock(paths).ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(_) => None,
    }
}

pub fn worker_lock_is_free(paths: &Paths) -> bool {
    let Some(file) = try_acquire_worker_lock(paths) else {
        return false;
    };
    let _ = file.unlock();
    true
}

pub fn write_worker_pid(paths: &Paths) {
    let _ = fs::write(paths.worker_pid(), std::process::id().to_string());
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

pub fn worker_state(paths: &Paths) -> (bool, Option<u32>) {
    if worker_lock_is_free(paths) {
        (false, None)
    } else {
        (true, read_worker_pid(paths))
    }
}

pub fn spawn_worker(paths: &Paths) -> Result<(), String> {
    let exe = paths.exe_dir.join("faceit-dem.exe");
    if !exe.is_file() {
        return Err(format!(
            "faceit-dem.exe was not found next to this executable.\nExpected: {}",
            exe.display()
        ));
    }

    crate::win32::spawn_new_console(&exe, &["--queue-worker"])
}

pub fn handle_context_menu(paths: &Paths, files: &[OsString]) -> Result<(), ContextMenuError> {
    if files.is_empty() {
        return Err(ContextMenuError::Usage(
            "no file was provided for --context-menu".to_string(),
        ));
    }

    let queued = enqueue_paths(paths, files)
        .map_err(|err| ContextMenuError::Failure(format!("could not queue file(s): {err}")))?;

    if queued == 0 {
        return Err(ContextMenuError::Usage(
            "no valid files were queued".to_string(),
        ));
    }

    let Some(spawn_guard) = try_acquire_spawn_lock(paths) else {
        return Ok(());
    };

    if !worker_lock_is_free(paths) {
        return Ok(());
    }

    spawn_worker(paths).map_err(ContextMenuError::Failure)?;

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline && worker_lock_is_free(paths) {
        sleep(STARTUP_POLL);
    }

    drop(spawn_guard);
    Ok(())
}

pub fn run_worker(paths: &Paths) -> i32 {
    let worker_mode = WorkerModeGuard::new();

    if let Err(err) = ensure_queue_dir(paths) {
        output::error(format!("could not create the queue directory: {err}"));
        drop(worker_mode);
        output::pause("Press Enter to close...");
        return 1;
    }

    let Some(mut lock) = try_acquire_worker_lock(paths) else {
        return 0;
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
            return 130;
        }

        if files.is_empty() {
            drop(lock);

            if has_queued_files(paths) {
                match try_acquire_worker_lock(paths) {
                    Some(new_lock) => {
                        lock = new_lock;
                        write_worker_pid(paths);
                        continue;
                    }
                    None => {
                        return if any_failed { 1 } else { 0 };
                    }
                }
            }

            cleanup_own_worker_pid(paths);
            break;
        }

        processed_any = true;
        let code = extract::run_extract_mode(paths, &files, false);

        if code == 130 {
            discard_queued_files(paths);
            cleanup_own_worker_pid(paths);
            return 130;
        }
        if code != 0 {
            any_failed = true;
        }
    }

    if !processed_any {
        println!("  {}", output::muted("No queued files were found."));
    }

    drop(worker_mode);
    output::pause("Press Enter to close...");

    if any_failed { 1 } else { 0 }
}

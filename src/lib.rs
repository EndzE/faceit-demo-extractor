#[cfg(not(windows))]
compile_error!("faceit-dem is Windows-only.");

pub mod cli;
pub mod config;
pub mod cs2;
pub mod extract;
pub mod output;
pub mod queue;
pub mod registry;
pub mod validate;
pub mod win32;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const APP_NAME: &str = "FACEIT Demo Extractor";
pub const DEFAULT_TARGET_FILENAME: &str = "1.dem";
pub const DEFAULT_KEEP_ZST: bool = false;
pub const DEFAULT_MOVE_TO_RECYCLE_BIN: bool = false;

pub const DEMO_MAGIC: &[u8] = b"PBDEMS2\x00";
pub const MAX_FILENAME_LEN: usize = 255;

pub static CANCEL: AtomicBool = AtomicBool::new(false);
pub static EXTRACTING: AtomicBool = AtomicBool::new(false);
pub static WORKER_MODE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug)]
pub struct Paths {
    pub exe: PathBuf,
    pub exe_dir: PathBuf,
    pub config: PathBuf,
    pub queue_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Self {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("faceit-dem.exe"));
        let exe_dir = exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let config = exe_dir.join("config.json");
        let queue_dir = std::env::temp_dir().join("faceit_demo_extractor.queue");
        Self {
            exe,
            exe_dir,
            config,
            queue_dir,
        }
    }

    pub fn for_tests(root: &Path) -> Self {
        let exe_dir = root.to_path_buf();
        Self {
            exe: exe_dir.join("faceit-dem.exe"),
            config: exe_dir.join("config.json"),
            queue_dir: exe_dir.join("queue"),
            exe_dir,
        }
    }

    pub fn worker_lock(&self) -> PathBuf {
        self.queue_dir.join("worker.lock")
    }

    pub fn worker_pid(&self) -> PathBuf {
        self.queue_dir.join("worker.pid")
    }

    pub fn spawn_lock(&self) -> PathBuf {
        self.queue_dir.join("spawn.lock")
    }
}

pub fn run_console() -> i32 {
    #[cfg(windows)]
    let _ = colored::control::set_virtual_terminal(true);

    install_ctrlc_handler();

    match std::panic::catch_unwind(cli::run) {
        Ok(code) => code,
        Err(payload) => {
            output::error(format!(
                "unexpected internal error: {}",
                panic_message(payload.as_ref())
            ));
            output::pause("Press Enter to close...");
            1
        }
    }
}

pub fn run_quiet() -> i32 {
    match std::panic::catch_unwind(cli::run_quiet) {
        Ok(code) => code,
        Err(payload) => {
            win32::message_box(
                APP_NAME,
                &format!(
                    "Unexpected internal error.\n\n{}",
                    panic_message(payload.as_ref())
                ),
            );
            1
        }
    }
}

fn install_ctrlc_handler() {
    let installed = ctrlc::set_handler(|| {
        CANCEL.store(true, Ordering::SeqCst);
        if EXTRACTING.load(Ordering::SeqCst) || WORKER_MODE.load(Ordering::SeqCst) {
            return;
        }
        output::cancelled();
        std::process::exit(130);
    });

    if installed.is_err() {
        output::warn(
            "could not install the Ctrl+C handler; extraction cannot be cancelled cleanly",
        );
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

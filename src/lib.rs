#![warn(clippy::undocumented_unsafe_blocks)]
#![warn(missing_docs)]
//! # FACEIT Demo Extractor
//!
//! A fast, background Windows utility for extracting and renaming CS2 demos
//! (`.dem.zst` and `.dem.bz2`) downloaded from FACEIT.
//!
//! ## Overview
//!
//! The crate provides decompression for Zstandard and bzip2 archives, CS2 demo folder
//! auto-discovery, Windows Explorer context menu integration, and a background queue worker.
//!
//! ## Submodules
//!
//! - [`cli`] - Command-line dispatch and terminal interaction.
//! - [`config`] - Application configuration loading, saving, and defaults.
//! - [`cs2`] - Counter-Strike 2 demo directory location via Steam registry keys.
//! - [`extract`] - Archive decompression routines, checksum verification, and file conflict resolution.
//! - [`output`] - Formatted console output, styled banners, and interactive progress bars.
//! - [`queue`] - Inter-process FIFO job queue for Explorer batch extractions.
//! - [`registry`] - Windows Registry management for Explorer right-click context menus.
//! - [`validate`] - Windows filename and demo naming convention validation.
//! - [`win32`] - Win32 Shell API, WinRT toast notifications, and console primitives.

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

pub use crate::extract::DeleteOriginalError;
pub use crate::queue::{ContextMenuError, QueueError};
pub use crate::registry::RegistryError;
pub use crate::validate::{DemoFileName, DemoFileNameError};
pub use crate::win32::Win32Error;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Human-readable application name used in banners and dialog titles.
pub const APP_NAME: &str = "FACEIT Demo Extractor";
/// Default demo filename when renaming is enabled.
pub const DEFAULT_TARGET_FILENAME: &str = "1.dem";
/// Default suffix delimiter when resolving filename collisions.
pub const DEFAULT_DELIMITER: &str = "_";
/// Default preference for keeping the original compressed archive after extraction.
pub const DEFAULT_KEEP_ZST: bool = false;
/// Default preference for moving deleted archives to the Windows Recycle Bin.
pub const DEFAULT_MOVE_TO_RECYCLE_BIN: bool = false;
/// Default preference for overwriting target files during single-file extraction.
pub const DEFAULT_OVERWRITE_SINGLE_FILE_EXTRACTION: bool = true;
/// Default preference for renaming extracted demos to [`DEFAULT_TARGET_FILENAME`].
pub const DEFAULT_RENAME_DEMO: bool = true;
/// Default preference for desktop toast notifications instead of console output.
pub const DEFAULT_NOTIFICATIONS_INSTEAD_OF_CONSOLE: bool = false;

/// Expected 8-byte magic header at the beginning of a valid CS2 demo file.
pub const DEMO_MAGIC: &[u8] = b"PBDEMS2\x00";
/// Maximum allowed length in UTF-16 code units for a single Windows filename component.
pub const MAX_FILENAME_LEN: usize = 255;
/// Environment variable name that overrides the application home directory root.
pub const HOME_ENV: &str = "FACEIT_DEMO_EXTRACTOR_HOME";

/// Flag set when an extraction cancellation (e.g. Ctrl+C) is requested.
pub static CANCEL: AtomicBool = AtomicBool::new(false);
/// Flag indicating whether active decompression is in progress.
pub static EXTRACTING: AtomicBool = AtomicBool::new(false);
/// Flag indicating whether the current process is running as a background queue worker.
pub static WORKER_MODE: AtomicBool = AtomicBool::new(false);
/// Flag indicating whether the console UI is waiting on an interactive pause prompt.
pub static PAUSING: AtomicBool = AtomicBool::new(false);

/// Standard application paths for binaries, configuration, and job queue directories.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Paths {
    /// Path to the executing binary.
    pub exe: PathBuf,
    /// Directory containing the executing binary.
    pub exe_dir: PathBuf,
    /// Path to `config.json`.
    pub config: PathBuf,
    /// Path to the temporary queue directory for job scheduling.
    pub queue_dir: PathBuf,
}

impl Paths {
    /// Discovers application paths from the environment or the current executable location.
    #[must_use]
    pub fn discover() -> Self {
        if let Some(root) = std::env::var_os(HOME_ENV) {
            return Self::from_root(Path::new(&root));
        }

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

    fn from_root(root: &Path) -> Self {
        let exe_dir = root.to_path_buf();
        Self {
            exe: exe_dir.join("faceit-dem.exe"),
            config: exe_dir.join("config.json"),
            queue_dir: exe_dir.join("queue"),
            exe_dir,
        }
    }

    /// Constructs isolated [`Paths`] anchored at `root` for testing.
    #[must_use]
    pub fn for_tests(root: &Path) -> Self {
        Self::from_root(root)
    }

    /// Returns the path to the console executable (`faceit-dem.exe`).
    #[must_use]
    pub fn console_exe(&self) -> PathBuf {
        self.exe_dir.join("faceit-dem.exe")
    }

    /// Returns the path to the quiet windowless executable (`faceit-dem-quiet.exe`).
    #[must_use]
    pub fn quiet_exe(&self) -> PathBuf {
        self.exe_dir.join("faceit-dem-quiet.exe")
    }

    /// Returns the path to the inter-process worker lockfile.
    #[must_use]
    pub fn worker_lock(&self) -> PathBuf {
        self.queue_dir.join("worker.lock")
    }

    /// Returns the path to the worker process ID file.
    #[must_use]
    pub fn worker_pid(&self) -> PathBuf {
        self.queue_dir.join("worker.pid")
    }

    /// Returns the path to the inter-process spawn lockfile.
    #[must_use]
    pub fn spawn_lock(&self) -> PathBuf {
        self.queue_dir.join("spawn.lock")
    }
}

/// Executes the console application entry point, returning the process exit code.
pub fn run_console() -> i32 {
    #[cfg(windows)]
    let _ = colored::control::set_virtual_terminal(true);

    install_ctrlc_handler();

    match std::panic::catch_unwind(cli::run) {
        Ok(code) => code,
        Err(payload) => {
            let message = format!(
                "unexpected internal error: {}",
                panic_message(payload.as_ref())
            );
            output::error(&message);

            let notifications = WORKER_MODE.load(Ordering::Relaxed)
                && config::load_config(&Paths::discover().config).notifications_instead_of_console;
            if notifications {
                if let Err(err) = win32::notify("Extraction failed", &message) {
                    output::warn(format!("failed to show error notification: {err}"));
                }
            } else {
                output::pause("Press Enter to close...");
            }
            1
        }
    }
}

/// Executes the quiet windowless entry point, returning the process exit code.
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
        CANCEL.store(true, Ordering::Relaxed);
        if !PAUSING.load(Ordering::Relaxed)
            && (EXTRACTING.load(Ordering::Relaxed) || WORKER_MODE.load(Ordering::Relaxed))
        {
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

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return message;
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.as_str();
    }
    "unknown panic"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_panic_message_str() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("explicit panic message");
        assert_eq!(panic_message(payload.as_ref()), "explicit panic message");
    }

    #[test]
    fn test_panic_message_string() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("heap panic message"));
        assert_eq!(panic_message(payload.as_ref()), "heap panic message");
    }

    #[test]
    fn test_panic_message_other() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(panic_message(payload.as_ref()), "unknown panic");
    }

    #[test]
    fn test_paths_traits() {
        use std::collections::HashSet;

        let p1 = Paths {
            exe: PathBuf::from("faceit-dem.exe"),
            exe_dir: PathBuf::from("."),
            config: PathBuf::from("config.json"),
            queue_dir: PathBuf::from("queue"),
        };
        let p2 = p1.clone();
        assert_eq!(p1, p2);

        let mut set = HashSet::new();
        set.insert(p1);
        assert!(set.contains(&p2));
    }
}

//! Terminal formatting, ANSI color styling, diagnostic logs, and decompression progress display.

use std::fmt::Display;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use colored::Colorize;

use crate::APP_NAME;

/// Default character width for horizontal rule dividers.
pub const RULE_WIDTH: usize = 66;

static SILENT: AtomicBool = AtomicBool::new(false);

/// Enables or disables all console output globally for quiet background worker operations.
pub fn set_silent(silent: bool) {
    SILENT.store(silent, Ordering::Relaxed);
}

fn is_silent() -> bool {
    SILENT.load(Ordering::Relaxed)
}

/// Styles text as a prominent bright cyan title.
#[must_use]
pub fn title(text: impl AsRef<str>) -> String {
    text.as_ref().bright_cyan().bold().to_string()
}

/// Styles text as a cyan section header.
#[must_use]
pub fn section(text: impl AsRef<str>) -> String {
    text.as_ref().cyan().bold().to_string()
}

/// Styles text as a dimmed field label.
#[must_use]
pub fn label(text: impl AsRef<str>) -> String {
    text.as_ref().dimmed().to_string()
}

/// Styles text in bold font.
#[must_use]
pub fn strong(text: impl AsRef<str>) -> String {
    text.as_ref().bold().to_string()
}

/// Styles text in green representing success or healthy state.
#[must_use]
pub fn good(text: impl AsRef<str>) -> String {
    text.as_ref().green().to_string()
}

/// Styles text in yellow representing a caution or non-critical issue.
#[must_use]
pub fn caution(text: impl AsRef<str>) -> String {
    text.as_ref().yellow().to_string()
}

/// Styles text in red representing an error or failure.
#[must_use]
pub fn bad(text: impl AsRef<str>) -> String {
    text.as_ref().red().to_string()
}

/// Styles text in cyan for informative notes or bullets.
#[must_use]
pub fn note(text: impl AsRef<str>) -> String {
    text.as_ref().cyan().to_string()
}

/// Styles text in dimmed gray for secondary or muted info.
#[must_use]
pub fn muted(text: impl AsRef<str>) -> String {
    text.as_ref().dimmed().to_string()
}

/// Renders a horizontal rule of the specified character `width`.
#[must_use]
pub fn rule(width: usize) -> String {
    "─".repeat(width).dimmed().to_string()
}

/// Generates the standard application header banner with title and divider.
#[must_use]
pub fn banner() -> String {
    format!("{}\n{}", title(APP_NAME), rule(RULE_WIDTH))
}

/// Clears the terminal screen and scrollback buffer when attached to an interactive console.
pub fn clear_screen() {
    if is_silent() || !io::stdout().is_terminal() {
        return;
    }

    print!("\x1b[3J");
    let _ = io::stdout().flush();
    if crate::win32::clear_console().is_err() {
        print!("\x1b[2J\x1b[H");
        let _ = io::stdout().flush();
    }
}

/// Formats a key-value diagnostic pair aligned with standard indentation.
#[must_use]
pub fn field(name: &str, value: impl Display) -> String {
    format!("  {}  {value}", format!("{name:<16}").dimmed())
}

/// Formats a boolean flag as green "on" or dimmed "off".
#[must_use]
pub fn on_off(value: bool) -> String {
    if value {
        "on".green().to_string()
    } else {
        "off".dimmed().to_string()
    }
}

/// Splits a message into trimmed lines stripping carriage returns.
#[must_use]
pub fn split_lines(message: &str) -> Vec<&str> {
    message
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

fn print_prefixed(prefix: &str, styled_prefix: String, message: &str) {
    let indent = " ".repeat(prefix.chars().count() + 1);
    for (index, line) in split_lines(message).iter().enumerate() {
        if index == 0 {
            eprintln!("{styled_prefix} {line}");
        } else {
            eprintln!("{indent}{line}");
        }
    }
}

/// Prints a yellow warning diagnostic message with appropriate indentation.
pub fn warn(message: impl AsRef<str>) {
    if is_silent() {
        return;
    }
    print_prefixed(
        "warning:",
        "warning:".yellow().bold().to_string(),
        message.as_ref(),
    );
}

/// Prints a red error diagnostic message with appropriate indentation.
pub fn error(message: impl AsRef<str>) {
    if is_silent() {
        return;
    }
    print_prefixed(
        "error:",
        "error:".red().bold().to_string(),
        message.as_ref(),
    );
}

/// Prints a yellow "Cancelled." cancellation notice to stderr.
pub fn cancelled() {
    eprintln!("\n{}", "Cancelled.".yellow().bold());
}

/// Pauses execution until the user presses Enter, unless silenced or redirected.
pub fn pause(message: &str) {
    if is_silent()
        || std::env::var_os("FACEIT_DEMO_EXTRACTOR_TEST_NO_PAUSE").is_some()
        || !io::stdin().is_terminal()
    {
        return;
    }

    crate::PAUSING.store(true, Ordering::Relaxed);
    print!("{}", message.dimmed());
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    crate::PAUSING.store(false, Ordering::Relaxed);
}

/// Formats a byte size in megabytes with 1 decimal place (e.g. `12.3 MB`).
#[must_use]
pub fn format_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// Formats a byte size into the most natural binary unit (B, KB, MB, GB, TB).
#[must_use]
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;

    let value = bytes as f64;
    if value >= TB {
        format!("{:.1} TB", value / TB)
    } else if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Interactive console progress indicator showing percentage and byte throughput.
pub struct Progress {
    total: Option<u64>,
    interactive: bool,
    active: bool,
    last_render: Option<Instant>,
}

impl Progress {
    /// Constructs a new [`Progress`] tracker with optional known total decompressed size.
    #[must_use]
    pub fn new(total: Option<u64>) -> Self {
        Self {
            total: total.filter(|total| *total > 0),
            interactive: !is_silent() && io::stdout().is_terminal(),
            active: false,
            last_render: None,
        }
    }

    /// Updates the displayed progress with the current number of bytes decoded.
    pub fn update(&mut self, current: u64) {
        if !self.interactive {
            return;
        }

        let at_end = self.total.is_some_and(|total| current >= total);
        if !at_end
            && self
                .last_render
                .is_some_and(|last| last.elapsed() < Duration::from_millis(100))
        {
            return;
        }

        let line = match self.total {
            Some(total) => {
                let percent = current.saturating_mul(100) / total;
                format!(
                    "\r  {:<16}  {}  {} / {}",
                    "Progress".dimmed(),
                    format!("{:>3}%", percent.min(100)).bright_cyan().bold(),
                    format_mb(current),
                    format_mb(total).dimmed()
                )
            }
            None => format!("\r  {:<16}  {}", "Progress".dimmed(), format_mb(current)),
        };

        print!("{line}");
        let _ = io::stdout().flush();
        self.active = true;
        self.last_render = Some(Instant::now());
    }

    /// Clears the progress line from the console display.
    pub fn clear(&mut self) {
        if !self.active {
            return;
        }

        print!("\r{}\r", " ".repeat(96));
        let _ = io::stdout().flush();
        self.active = false;
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn format_size_uses_a_readable_unit() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(512 * 1024), "512.0 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(format_size(1024u64.pow(4)), "1.0 TB");
    }

    #[test]
    fn styling_helpers_accept_various_string_types() {
        assert!(title("title").contains("title"));
        assert!(section("section").contains("section"));
        assert!(label("label").contains("label"));
        assert!(strong(String::from("strong")).contains("strong"));
        assert!(good("good").contains("good"));
        assert!(caution(Cow::Borrowed("caution")).contains("caution"));
        assert!(bad(String::from("bad")).contains("bad"));
        assert!(note("note").contains("note"));
        assert!(muted("muted").contains("muted"));
    }
}

use std::fmt::Display;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use colored::Colorize;

use crate::APP_NAME;

pub const RULE_WIDTH: usize = 66;

static SILENT: AtomicBool = AtomicBool::new(false);

pub fn set_silent(silent: bool) {
    SILENT.store(silent, Ordering::SeqCst);
}

fn is_silent() -> bool {
    SILENT.load(Ordering::SeqCst)
}

pub fn title(text: impl Display) -> String {
    text.to_string().bright_cyan().bold().to_string()
}

pub fn section(text: &str) -> String {
    text.cyan().bold().to_string()
}

pub fn label(text: &str) -> String {
    text.dimmed().to_string()
}

pub fn strong(text: impl Display) -> String {
    text.to_string().bold().to_string()
}

pub fn good(text: impl Display) -> String {
    text.to_string().green().to_string()
}

pub fn caution(text: impl Display) -> String {
    text.to_string().yellow().to_string()
}

pub fn bad(text: impl Display) -> String {
    text.to_string().red().to_string()
}

pub fn note(text: impl Display) -> String {
    text.to_string().cyan().to_string()
}

pub fn muted(text: impl Display) -> String {
    text.to_string().dimmed().to_string()
}

pub fn rule(width: usize) -> String {
    "─".repeat(width).dimmed().to_string()
}

pub fn banner() -> String {
    format!("{}\n{}", title(APP_NAME), rule(RULE_WIDTH))
}

pub fn field(name: &str, value: impl Display) -> String {
    format!("  {}  {value}", format!("{name:<16}").dimmed())
}

pub fn indented_field(indent: usize, name: &str, value: impl Display) -> String {
    format!(
        "{}{}  {value}",
        " ".repeat(indent),
        format!("{name:<16}").dimmed()
    )
}

pub fn on_off(value: bool) -> String {
    if value {
        "on".green().to_string()
    } else {
        "off".dimmed().to_string()
    }
}

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

pub fn cancelled() {
    eprintln!("\n{}", "Cancelled.".yellow().bold());
}

pub fn pause(message: &str) {
    if is_silent() || !io::stdin().is_terminal() {
        return;
    }

    print!("{}", message.dimmed());
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
}

pub fn format_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

pub fn format_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;

    let bytes = bytes as f64;
    if bytes >= TB {
        format!("{:.1} TB", bytes / TB)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else {
        format!("{:.1} MB", bytes / MB)
    }
}

pub struct Progress {
    total: Option<u64>,
    interactive: bool,
    active: bool,
    last_render: Option<Instant>,
}

impl Progress {
    pub fn new(total: Option<u64>) -> Self {
        Self {
            total: total.filter(|total| *total > 0),
            interactive: !is_silent() && io::stdout().is_terminal(),
            active: false,
            last_render: None,
        }
    }

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

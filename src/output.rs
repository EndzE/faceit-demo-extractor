use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

static SILENT: AtomicBool = AtomicBool::new(false);

pub fn set_silent(silent: bool) {
    SILENT.store(silent, Ordering::SeqCst);
}

fn is_silent() -> bool {
    SILENT.load(Ordering::SeqCst)
}

pub fn split_lines(message: &str) -> Vec<&str> {
    message
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

fn print_prefixed(label: &str, message: &str, stderr: bool) {
    for (index, line) in split_lines(message).iter().enumerate() {
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

pub fn warn(message: impl AsRef<str>) {
    if is_silent() {
        return;
    }
    print_prefixed("WARNING:", message.as_ref(), true);
}

pub fn error(message: impl AsRef<str>) {
    if is_silent() {
        return;
    }
    print_prefixed("ERROR:", message.as_ref(), true);
}

pub fn pause(message: &str) {
    if is_silent() || !io::stdin().is_terminal() {
        return;
    }

    print!("{message}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
}

pub fn py_repr_str(value: &str) -> String {
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

pub fn py_repr_json(value: &Value) -> String {
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

pub fn bytes_repr(bytes: &[u8]) -> String {
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

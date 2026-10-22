use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use serde_json::Value;

use crate::queue::ContextMenuError;
use crate::validate::is_valid_filename;
use crate::{APP_NAME, Paths, config, cs2, extract, output, queue, registry, win32};

const REGISTRY_FLAGS: &[&str] = &["--install", "--remove", "--update-label"];
const HELP_FLAGS: &[&str] = &["-h", "--help"];

pub fn run() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    dispatch(&args)
}

pub fn run_quiet() -> i32 {
    output::set_silent(true);

    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let paths = Paths::discover();

    let (ok, config_error) = config::ensure_config_file(&paths.config, false);
    if !ok {
        win32::message_box(
            APP_NAME,
            &format!(
                "WARNING: could not create or update config.json: {}",
                config_error.unwrap_or_default()
            ),
        );
    }

    if args.first().and_then(|arg| arg.to_str()) != Some("--context-menu") {
        win32::message_box(
            APP_NAME,
            "This executable is invoked from Explorer.\n\nUse faceit-dem.exe --help for the command-line interface.",
        );
        return 2;
    }

    match queue::handle_context_menu(&paths, &args[1..]) {
        Ok(()) => 0,
        Err(ContextMenuError::Usage(message)) => {
            win32::message_box(APP_NAME, &format!("ERROR: {message}"));
            2
        }
        Err(ContextMenuError::Failure(message)) => {
            win32::message_box(APP_NAME, &format!("ERROR: {message}"));
            1
        }
    }
}

fn dispatch(args: &[OsString]) -> i32 {
    let first = args.first().and_then(|arg| arg.to_str());

    if first.is_some_and(|arg| HELP_FLAGS.contains(&arg)) {
        print!("{}", usage());
        return 0;
    }

    let paths = Paths::discover();
    let (ok, config_error) = config::ensure_config_file(&paths.config, true);
    if !ok {
        output::warn(format!(
            "could not create or update config.json: {}",
            config_error.unwrap_or_default()
        ));
    }

    if args.is_empty() {
        return run_menu(&paths);
    }

    let first = first.unwrap_or_default();

    if REGISTRY_FLAGS.contains(&first) {
        return run_registry_action(first, &paths);
    }

    match first {
        "--status" => {
            println!("{}", status_lines(&paths).join("\n"));
            0
        }
        "--context-menu" => run_context_menu(&paths, &args[1..]),
        "--queue-worker" => queue::run_worker(&paths),
        "--" => {
            let rest = &args[1..];
            if rest.is_empty() {
                output::error("no input files after --");
                print!("{}", usage());
                return 2;
            }
            run_extract(&paths, rest)
        }
        arg if arg.starts_with('-') && !PathBuf::from(arg).is_file() => {
            output::error(format!("unknown option: {arg}"));
            print!("{}", usage());
            2
        }
        _ => run_extract(&paths, args),
    }
}

fn run_extract(paths: &Paths, args: &[OsString]) -> i32 {
    let files: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();
    extract::run_extract_mode(paths, &files, true)
}

fn run_context_menu(paths: &Paths, files: &[OsString]) -> i32 {
    match queue::handle_context_menu(paths, files) {
        Ok(()) => 0,
        Err(ContextMenuError::Usage(message)) => {
            output::error(message);
            2
        }
        Err(ContextMenuError::Failure(message)) => {
            output::error(message);
            1
        }
    }
}

fn run_registry_action(flag: &str, paths: &Paths) -> i32 {
    println!("--> Running {flag}");

    let result = match flag {
        "--install" => registry::perform_install(paths),
        "--remove" => registry::perform_remove(),
        "--update-label" => registry::perform_update_label(paths),
        _ => unreachable!(),
    };

    match result {
        Ok(()) => {
            println!("    done.");
            if flag == "--install" {
                println!("    right-click any .zst file to try it out");
            }
            0
        }
        Err(message) => {
            output::error(message);
            output::pause("Press Enter to close...");
            1
        }
    }
}

pub fn usage() -> String {
    format!(
        "{APP_NAME}

Usage:
  faceit-dem                            interactive menu
  faceit-dem <file.zst> [...]           extract file(s) directly
  faceit-dem -- <file> [...]            same, for names starting with \"-\"
  faceit-dem --context-menu <file>      queue a context-menu invocation
  faceit-dem --queue-worker             internal worker for queued files
  faceit-dem --install                  add per-user context-menu entry
  faceit-dem --remove                   remove per-user context-menu entry
  faceit-dem --update-label             update context-menu label
  faceit-dem --status                   show current configuration
  faceit-dem --help                     show this help

Notes:
  * The context-menu entry is per-user and does not require UAC.
  * zstd decoding is statically linked; no Python, pip, venv, or
    zstandard module is needed.
  * Context-menu invocations are queued briefly so multiple Explorer
    invocations can be processed by one worker as a batch.
  * config.json options:
      target_filename
      keep_zst
      move_to_recycle_bin
"
    )
}

pub fn status_lines(paths: &Paths) -> Vec<String> {
    let (csgo, source) = cs2::find_cs2_csgo_path();
    let config = config::load_config(&paths.config);
    let (worker_running, worker_pid) = queue::worker_state(paths);
    let width = 66;

    let mut lines = vec![
        "=".repeat(width),
        format!("  {APP_NAME}"),
        "=".repeat(width),
        format!("  Executable    : {}", paths.exe.display()),
        format!("  Config        : {}", paths.config.display()),
        format!(
            "  CS2 folder    : {}",
            csgo.as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "not found".to_string())
        ),
    ];

    if csgo.is_some() {
        lines.push(format!("  CS2 source    : {source}"));
    }

    lines.push("  zstd          : bundled".to_string());
    lines.push(format!(
        "  Worker        : {}",
        match (worker_running, worker_pid) {
            (true, Some(pid)) => format!("running (pid {pid})"),
            (true, None) => "running".to_string(),
            (false, _) => "not running".to_string(),
        }
    ));
    lines.push("-".repeat(width));
    lines.push(format!(
        "  target_filename     : {}",
        config.target_filename
    ));
    lines.push(format!(
        "  keep_zst            : {}",
        if config.keep_zst { "on" } else { "off" }
    ));
    lines.push(format!(
        "  move_to_recycle_bin : {}",
        if config.move_to_recycle_bin {
            "on"
        } else {
            "off"
        }
    ));
    lines.push("-".repeat(width));

    if registry::context_menu_installed() {
        let command = registry::context_menu_command();
        lines.push(format!(
            "  Context menu  : {}",
            if command.is_some() {
                "installed"
            } else {
                "needs repair"
            }
        ));

        if let Some(label) = registry::context_menu_label() {
            lines.push(format!("  Menu label    : {label}"));
        }
        if let Some(command) = command {
            lines.push(format!("  Menu command  : {command}"));
        }
    } else {
        lines.push("  Context menu  : not installed".to_string());
    }

    lines.push("=".repeat(width));
    lines
}

fn menu_status_lines() -> Vec<String> {
    let (csgo, _) = cs2::find_cs2_csgo_path();
    let paths = Paths::discover();
    let width = 66;

    vec![
        "=".repeat(width),
        format!("  {APP_NAME}"),
        "=".repeat(width),
        format!(
            "  CS2 folder : {}",
            csgo.as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "not found".to_string())
        ),
        format!("  Config     : {}", paths.config.display()),
        format!("  Executable : {}", paths.exe.display()),
        "-".repeat(width),
    ]
}

fn read_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    io::stdout().flush().ok()?;

    let mut line = String::new();
    if io::stdin().read_line(&mut line).ok()? == 0 {
        return None;
    }

    Some(line)
}

fn menu_install(paths: &Paths) {
    println!("\nInstalling right-click menu...");

    match registry::perform_install(paths) {
        Ok(()) => println!("Done. Right-click any .zst file to extract it."),
        Err(message) => output::error(message),
    }
}

fn menu_remove() {
    if !registry::context_menu_installed() {
        println!("\nRight-click menu is not installed.");
        return;
    }

    println!("\nRemoving right-click menu...");

    match registry::perform_remove() {
        Ok(()) => println!("Done."),
        Err(message) => output::error(message),
    }
}

fn menu_change_filename(paths: &Paths) {
    let current = config::load_config(&paths.config).target_filename;

    println!("\nCurrent demo filename: {current}");

    let Some(input) = read_line("New demo filename (blank to cancel): ") else {
        return;
    };
    let new_name = input.trim().trim_matches('"').trim().to_string();

    if new_name.is_empty() {
        println!("Cancelled.");
        return;
    }

    if !is_valid_filename(&new_name) {
        println!("That filename cannot be used on Windows.");
        return;
    }

    let updated = config::save_config(
        &paths.config,
        &[("target_filename", Value::String(new_name.clone()))],
    );

    if let Err(message) = updated {
        output::error(message);
        return;
    }

    println!("Saved. Demos will now be extracted as: {new_name}");

    if registry::context_menu_command().is_some() {
        let Some(answer) = read_line("Update the right-click menu text too? [y/N]: ") else {
            return;
        };
        let answer = answer.trim().to_lowercase();

        if answer == "y" || answer == "yes" {
            match registry::perform_update_label(paths) {
                Ok(()) => println!("Right-click menu text updated."),
                Err(message) => output::error(message),
            }
        }
    }
}

fn menu_toggle_keep_zst(paths: &Paths) {
    let keep = !config::load_config(&paths.config).keep_zst;

    match config::save_config(&paths.config, &[("keep_zst", Value::Bool(keep))]) {
        Ok(()) => {
            if keep {
                println!("Original .zst files will be kept.");
            } else {
                println!("Original .zst files will be removed after extraction.");
            }
        }
        Err(message) => output::error(message),
    }
}

fn menu_toggle_recycle_bin(paths: &Paths) {
    let recycle = !config::load_config(&paths.config).move_to_recycle_bin;

    match config::save_config(
        &paths.config,
        &[("move_to_recycle_bin", Value::Bool(recycle))],
    ) {
        Ok(()) => {
            if recycle {
                println!("Removed .zst files will go to the Recycle Bin.");
            } else {
                println!("Removed .zst files will be permanently deleted.");
            }
        }
        Err(message) => output::error(message),
    }
}

fn run_menu(paths: &Paths) -> i32 {
    loop {
        for line in menu_status_lines() {
            println!("{line}");
        }

        let config = config::load_config(&paths.config);

        let menu_state = if registry::context_menu_command().is_some() {
            "installed"
        } else if registry::context_menu_installed() {
            "needs repair"
        } else {
            "not installed"
        };

        let option_one = if menu_state == "installed" {
            "Reinstall right-click menu - installed"
        } else if menu_state == "needs repair" {
            "Repair right-click menu - needs repair"
        } else {
            "Install right-click menu - not installed"
        };

        println!(
            "\n  1. {option_one}
  2. Remove right-click menu
  3. Demo filename: {}
  4. Keep original .zst: {}
  5. Use Recycle Bin: {}
  0. Exit
",
            config.target_filename,
            if config.keep_zst { "on" } else { "off" },
            if config.move_to_recycle_bin {
                "on"
            } else {
                "off"
            }
        );

        let Some(choice) = read_line("Choose an option: ") else {
            println!();
            break;
        };
        let choice = choice.trim().to_lowercase();

        match choice.as_str() {
            "0" | "q" | "quit" | "exit" => break,
            "1" => menu_install(paths),
            "2" => menu_remove(),
            "3" => menu_change_filename(paths),
            "4" => menu_toggle_keep_zst(paths),
            "5" => menu_toggle_recycle_bin(paths),
            _ => println!("Unknown option: {choice}"),
        }
    }

    0
}

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use serde_json::Value;

use crate::queue::ContextMenuError;
use crate::validate::is_valid_demo_filename;
use crate::{Paths, config, cs2, extract, output, queue, registry, win32};

const REGISTRY_FLAGS: &[&str] = &["--install", "--remove", "--update-label"];
const HELP_FLAGS: &[&str] = &["-h", "--help"];
const MENU_WIDTH: usize = 32;

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
            crate::APP_NAME,
            &format!(
                "Could not create or update config.json.\n\n{}",
                config_error.unwrap_or_default()
            ),
        );
        return 1;
    }

    if args.first().and_then(|arg| arg.to_str()) != Some("--context-menu") {
        win32::message_box_info(
            crate::APP_NAME,
            "This program is launched from Explorer.\n\nUse faceit-dem.exe for the command-line interface.",
        );
        return 2;
    }

    match queue::handle_context_menu(&paths, &args[1..]) {
        Ok(()) => 0,
        Err(ContextMenuError::Usage(message)) => {
            win32::message_box(
                crate::APP_NAME,
                &format!("Could not queue the selection.\n\n{message}"),
            );
            2
        }
        Err(ContextMenuError::Failure(message)) => {
            win32::message_box(
                crate::APP_NAME,
                &format!("Could not process the selection.\n\n{message}"),
            );
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

    if first == Some("--test") {
        return run_test(&args[1..]);
    }

    let paths = Paths::discover();
    let (ok, config_error) = config::ensure_config_file(&paths.config, true);
    if !ok {
        output::error(format!(
            "could not create or update config.json: {}",
            config_error.unwrap_or_default()
        ));
        return 1;
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

fn run_test(args: &[OsString]) -> i32 {
    if args.len() != 1 {
        output::error("--test expects one archive path");
        print!("{}", usage());
        return 2;
    }

    match extract::test_archive(&PathBuf::from(&args[0])) {
        extract::ExtractResult::Ok => 0,
        extract::ExtractResult::Failed => 1,
        extract::ExtractResult::Cancelled => 130,
    }
}

fn command_line(command: &str, description: &str) -> String {
    format!(
        "  {}  {description}",
        output::note(format!("{command:<40}"))
    )
}

pub fn usage() -> String {
    let lines = [
        output::banner(),
        String::new(),
        format!(
            "  {}",
            output::muted("Extract FACEIT .dem.zst demos straight into your CS2 folder.")
        ),
        String::new(),
        output::section("Usage"),
        command_line("faceit-dem", "Open the interactive menu"),
        command_line(
            "faceit-dem <demo.zst> [...]",
            "Extract one or more archives",
        ),
        command_line(
            "faceit-dem -- <demo.zst> [...]",
            "Extract names that start with \"-\"",
        ),
        command_line("faceit-dem --status", "Show settings and current state"),
        command_line(
            "faceit-dem --test <demo.zst>",
            "Verify the archive checksum without writing",
        ),
        command_line("faceit-dem --install", "Add the Explorer right-click menu"),
        command_line(
            "faceit-dem --remove",
            "Remove the Explorer right-click menu",
        ),
        command_line(
            "faceit-dem --update-label",
            "Refresh the right-click menu text",
        ),
        command_line("faceit-dem --help", "Show this help"),
        String::new(),
        output::section("Internal commands"),
        command_line(
            "faceit-dem --context-menu <file>",
            "Queue an Explorer selection",
        ),
        command_line("faceit-dem --queue-worker", "Process queued archives"),
        String::new(),
        output::section("How it works"),
        format!(
            "  {} Multi-file selections are queued briefly and extracted as one batch.",
            output::note("-")
        ),
        format!(
            "  {} Demos are only replaced after the archive passes its integrity checks.",
            output::note("-")
        ),
        format!(
            "  {} Original .zst files can be kept, recycled, or permanently deleted.",
            output::note("-")
        ),
        String::new(),
        output::section("Settings"),
        format!(
            "  {}  config.json, next to faceit-dem.exe",
            output::muted("File")
        ),
        format!(
            "  {}  target_filename, keep_zst, move_to_recycle_bin, csgo_path",
            output::muted("Keys")
        ),
        String::new(),
        format!("  {}", output::muted("Set NO_COLOR=1 to disable colors.")),
    ];

    format!("{}\n", lines.join("\n"))
}

fn worker_field(paths: &Paths) -> String {
    match queue::worker_state(paths) {
        Ok((true, Some(pid))) => output::good(format!("running (pid {pid})")),
        Ok((true, None)) => output::caution("running (pid unknown)"),
        Ok((false, _)) => output::muted("not running"),
        Err(message) => output::bad(message),
    }
}

pub fn status_lines(paths: &Paths) -> Vec<String> {
    let config = config::load_config(&paths.config);
    let (csgo, source) = cs2::find_cs2_csgo_path(config.csgo_path.as_deref(), &paths.exe_dir);

    let mut lines = vec![
        output::banner(),
        String::new(),
        output::section("Paths"),
        output::field("Executable", paths.exe.display()),
        output::field("Config", paths.config.display()),
        output::field(
            "CS2 folder",
            match &csgo {
                Some(path) => path.display().to_string(),
                None => output::caution("not found"),
            },
        ),
    ];

    if csgo.is_some() || config.csgo_path.is_some() {
        lines.push(output::field("CS2 source", source));
    }

    lines.push(String::new());
    lines.push(output::section("Runtime"));
    lines.push(output::field("Worker", worker_field(paths)));

    lines.push(String::new());
    lines.push(output::section("Settings"));
    lines.push(output::field("Target demo", config.target_filename));
    lines.push(output::field(
        "Keep archives",
        output::on_off(config.keep_zst),
    ));
    lines.push(output::field(
        "Recycle bin",
        output::on_off(config.move_to_recycle_bin),
    ));

    lines.push(String::new());
    lines.push(output::section("Right-click menu"));

    let installed = registry::context_menu_installed();
    let command = registry::context_menu_command();

    lines.push(output::field(
        "Status",
        if !installed {
            output::muted("not installed")
        } else if command.is_some() {
            output::good("installed")
        } else {
            output::caution("needs repair")
        },
    ));

    if installed {
        if let Some(label) = registry::context_menu_label() {
            lines.push(output::field("Label", label));
        }
        if let Some(command) = command {
            lines.push(output::field("Command", command));
        }
    }

    lines.push(String::new());
    lines.push(output::rule(output::RULE_WIDTH));
    lines
}

fn read_line(prompt: &str) -> Option<String> {
    print!("{}", output::note(prompt));
    io::stdout().flush().ok()?;

    let mut line = String::new();
    if io::stdin().read_line(&mut line).ok()? == 0 {
        return None;
    }

    Some(line)
}

fn menu_install(paths: &Paths) {
    println!("{}", output::section("Right-click menu"));
    println!("  {}", output::muted("Installing..."));

    match registry::perform_install(paths) {
        Ok(()) => {
            println!("  {}", output::good("Installed."));
            println!(
                "  {}",
                output::muted("Right-click any .zst file in Explorer to extract it.")
            );
        }
        Err(message) => output::error(message),
    }
}

fn menu_remove() {
    if !registry::context_menu_installed() {
        println!(
            "  {}",
            output::muted("The right-click menu is not installed.")
        );
        return;
    }

    println!("{}", output::section("Right-click menu"));
    println!("  {}", output::muted("Removing..."));

    match registry::perform_remove() {
        Ok(()) => println!("  {}", output::good("Removed.")),
        Err(message) => output::error(message),
    }
}

fn menu_change_filename(paths: &Paths) {
    let current = config::load_config(&paths.config).target_filename;

    println!("{}", output::section("Demo filename"));
    println!(
        "  {}  {}",
        output::label("Current"),
        output::strong(&current)
    );

    let Some(input) = read_line("  New demo filename (Enter to cancel): ") else {
        return;
    };
    let new_name = input.trim().trim_matches('"').trim().to_string();

    if new_name.is_empty() {
        println!("  {}", output::muted("Cancelled."));
        return;
    }

    if !is_valid_demo_filename(&new_name) {
        println!("  {}", output::caution("That demo filename is not valid."));
        println!(
            "  {}",
            output::muted(
                "Use a single filename ending in .dem, without reserved names or <>:\"|?* / \\ characters."
            )
        );
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

    println!(
        "  {} New demos will be written as {}.",
        output::good("Saved."),
        output::strong(&new_name)
    );

    if registry::context_menu_command().is_some() {
        let Some(answer) = read_line("  Update the right-click menu text? [y/N]: ") else {
            return;
        };
        let answer = answer.trim().to_lowercase();

        if answer == "y" || answer == "yes" {
            match registry::perform_update_label(paths) {
                Ok(()) => println!("  {}", output::good("Right-click menu text updated.")),
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
                println!(
                    "  {} Original .zst archives will be kept.",
                    output::good("On.")
                );
            } else {
                println!(
                    "  {} Original .zst archives will be removed after extraction.",
                    output::caution("Off.")
                );
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
                println!(
                    "  {} Removed archives will be moved to the Recycle Bin.",
                    output::good("On.")
                );
            } else {
                println!(
                    "  {} Removed archives will be permanently deleted.",
                    output::caution("Off.")
                );
            }
        }
        Err(message) => output::error(message),
    }
}

fn menu_option(key: &str, name: &str, value: Option<String>) -> String {
    match value {
        Some(value) => format!("  {}  {:<MENU_WIDTH$}  {value}", output::title(key), name),
        None => format!("  {}  {name}", output::title(key)),
    }
}

fn run_menu(paths: &Paths) -> i32 {
    let mut first = true;

    loop {
        if !first {
            println!();
        }
        first = false;

        let config = config::load_config(&paths.config);
        let (csgo, source) = cs2::find_cs2_csgo_path(config.csgo_path.as_deref(), &paths.exe_dir);
        let installed = registry::context_menu_installed();
        let command = registry::context_menu_command();

        println!("{}", output::banner());
        println!();
        println!(
            "{}",
            output::field(
                "CS2 folder",
                match &csgo {
                    Some(path) => path.display().to_string(),
                    None => output::caution("not found"),
                }
            )
        );
        if csgo.is_none() && config.csgo_path.is_some() {
            println!("{}", output::field("CS2 source", output::caution(source)));
        }
        println!("{}", output::field("Config", paths.config.display()));
        println!(
            "{}",
            output::field(
                "Right-click",
                if command.is_some() {
                    output::good("installed")
                } else if installed {
                    output::caution("needs repair")
                } else {
                    output::muted("not installed")
                }
            )
        );
        println!("{}", output::field("Worker", worker_field(paths)));
        println!();
        println!("{}", output::rule(output::RULE_WIDTH));
        println!();

        let install_label = if command.is_some() {
            "Reinstall right-click menu"
        } else if installed {
            "Repair right-click menu"
        } else {
            "Install right-click menu"
        };

        println!("{}", menu_option("1", install_label, None));
        println!("{}", menu_option("2", "Remove right-click menu", None));
        println!(
            "{}",
            menu_option(
                "3",
                "Change demo filename",
                Some(output::strong(&config.target_filename))
            )
        );
        println!(
            "{}",
            menu_option(
                "4",
                "Keep original archives",
                Some(output::on_off(config.keep_zst))
            )
        );
        println!(
            "{}",
            menu_option(
                "5",
                "Use the Recycle Bin",
                Some(output::on_off(config.move_to_recycle_bin))
            )
        );
        println!("{}", menu_option("0", "Exit", None));
        println!();

        let Some(choice) = read_line("  Select an option [0-5]: ") else {
            println!();
            break;
        };
        let choice = choice.trim().to_lowercase();

        println!();

        match choice.as_str() {
            "0" | "q" | "quit" | "exit" => break,
            "1" => menu_install(paths),
            "2" => menu_remove(),
            "3" => menu_change_filename(paths),
            "4" => menu_toggle_keep_zst(paths),
            "5" => menu_toggle_recycle_bin(paths),
            _ => println!(
                "  {}",
                output::caution(format!("Unknown option: {choice}. Choose 0-5."))
            ),
        }
    }

    println!();
    println!("  {}", output::muted("Goodbye."));
    0
}

fn run_registry_action(flag: &str, paths: &Paths) -> i32 {
    let (title, done) = match flag {
        "--install" => (
            "Installing right-click menu...",
            "Right-click menu installed.",
        ),
        "--remove" => ("Removing right-click menu...", "Right-click menu removed."),
        "--update-label" => (
            "Updating right-click menu text...",
            "Right-click menu text updated.",
        ),
        _ => unreachable!(),
    };

    println!("{}", output::title(title));

    let result = match flag {
        "--install" => registry::perform_install(paths),
        "--remove" => registry::perform_remove(),
        "--update-label" => registry::perform_update_label(paths),
        _ => unreachable!(),
    };

    match result {
        Ok(()) => {
            println!("  {}", output::good(done));
            if flag == "--install" {
                println!(
                    "  {}",
                    output::muted("Right-click any .zst file in Explorer to extract it.")
                );
            }
            0
        }
        Err(message) => {
            output::error(message);
            println!();
            output::pause("Press Enter to close...");
            1
        }
    }
}

fn run_extract(paths: &Paths, args: &[OsString]) -> i32 {
    let files: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();
    println!("{}", output::banner());
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

use std::io;
use std::path::PathBuf;

use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

use crate::Paths;
use crate::config;

pub const CTX_KEY: &str = r"Software\Classes\SystemFileAssociations\.zst\shell\FaceitExtract";
pub const CTX_COMMAND_KEY: &str =
    r"Software\Classes\SystemFileAssociations\.zst\shell\FaceitExtract\command";

pub fn menu_label_for(target_filename: &str) -> String {
    format!("Extract to CS2 folder ({target_filename})")
}

pub fn quiet_exe_path(paths: &Paths) -> PathBuf {
    paths.exe_dir.join("faceit-dem-quiet.exe")
}

pub fn ensure_quiet_exe(paths: &Paths) -> Result<(), String> {
    let quiet = quiet_exe_path(paths);
    if quiet.is_file() {
        return Ok(());
    }

    Err(format!(
        "faceit-dem-quiet.exe was not found next to this executable.\nExpected: {}\n\nBuild or copy the quiet binary first.",
        quiet.display()
    ))
}

pub fn build_context_menu_command(paths: &Paths) -> String {
    format!(
        "\"{}\" --context-menu \"%1\"",
        quiet_exe_path(paths).display()
    )
}

fn reg_default(subkey: &str) -> Option<String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey_with_flags(subkey, KEY_READ).ok()?;
    key.get_value::<String, _>("").ok()
}

pub fn context_menu_command() -> Option<String> {
    reg_default(CTX_COMMAND_KEY)
}

pub fn context_menu_label() -> Option<String> {
    reg_default(CTX_KEY)
}

pub fn context_menu_installed() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(CTX_KEY, KEY_READ)
        .is_ok()
}

pub fn install_context_menu(label: &str, command: &str) -> Result<(), String> {
    install_context_menu_at(CTX_KEY, label, command)
}

pub fn install_context_menu_at(root: &str, label: &str, command: &str) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(root).map_err(|err| err.to_string())?;
    key.set_value("", &label.to_string())
        .map_err(|err| err.to_string())?;

    let (command_key, _) = hkcu
        .create_subkey(format!("{root}\\command"))
        .map_err(|err| err.to_string())?;
    command_key
        .set_value("", &command.to_string())
        .map_err(|err| err.to_string())?;

    Ok(())
}

pub fn set_context_menu_label(label: &str) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(CTX_KEY).map_err(|err| err.to_string())?;
    key.set_value("", &label.to_string())
        .map_err(|err| err.to_string())
}

pub fn remove_context_menu() -> Result<(), String> {
    remove_context_menu_at(CTX_KEY)
}

pub fn remove_context_menu_at(root: &str) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    match hkcu.delete_subkey_all(root) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

pub fn perform_install(paths: &Paths) -> Result<(), String> {
    ensure_quiet_exe(paths)?;
    let target = config::load_config(&paths.config).target_filename;
    install_context_menu(&menu_label_for(&target), &build_context_menu_command(paths))
}

pub fn perform_remove() -> Result<(), String> {
    remove_context_menu()
}

pub fn perform_update_label(paths: &Paths) -> Result<(), String> {
    if context_menu_command().is_none() {
        if context_menu_installed() {
            return perform_install(paths);
        }
        return Err("the right-click menu is not installed; nothing to update".to_string());
    }

    let target = config::load_config(&paths.config).target_filename;
    set_context_menu_label(&menu_label_for(&target))
}

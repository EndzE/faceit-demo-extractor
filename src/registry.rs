use std::io;

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

pub fn ensure_install_binaries(paths: &Paths) -> Result<(), String> {
    for (exe, name) in [
        (paths.quiet_exe(), "faceit-dem-quiet.exe"),
        (paths.console_exe(), "faceit-dem.exe"),
    ] {
        if !exe.is_file() {
            return Err(format!(
                "{name} was not found next to this executable.\nExpected: {}\n\nBuild or copy both binaries first.",
                exe.display()
            ));
        }
    }

    Ok(())
}

pub fn build_context_menu_command(paths: &Paths) -> String {
    format!("\"{}\" --context-menu \"%1\"", paths.quiet_exe().display())
}

fn reg_default(subkey: &str) -> Option<String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey_with_flags(subkey, KEY_READ).ok()?;
    key.get_value::<String, _>("").ok()
}

fn reg_key_exists(subkey: &str) -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(subkey, KEY_READ)
        .is_ok()
}

pub fn context_menu_command() -> Option<String> {
    reg_default(CTX_COMMAND_KEY)
}

pub fn context_menu_label() -> Option<String> {
    reg_default(CTX_KEY)
}

pub fn context_menu_installed() -> bool {
    reg_key_exists(CTX_KEY)
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

pub fn set_context_menu_label_at(root: &str, label: &str) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(root).map_err(|err| err.to_string())?;
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

fn install_context_menu_with(paths: &Paths, root: &str) -> Result<(), String> {
    ensure_install_binaries(paths)?;
    let target = config::load_config(&paths.config).target_filename;
    install_context_menu_at(
        root,
        &menu_label_for(&target),
        &build_context_menu_command(paths),
    )
}

pub fn perform_install(paths: &Paths) -> Result<(), String> {
    install_context_menu_with(paths, CTX_KEY)
}

pub fn perform_remove() -> Result<(), String> {
    remove_context_menu()
}

pub fn perform_update_label_at(paths: &Paths, root: &str) -> Result<(), String> {
    if reg_default(&format!("{root}\\command")).is_none() {
        if reg_key_exists(root) {
            return install_context_menu_with(paths, root);
        }
        return Err("the right-click menu is not installed; nothing to update".to_string());
    }

    let target = config::load_config(&paths.config).target_filename;
    set_context_menu_label_at(root, &menu_label_for(&target))
}

pub fn perform_update_label(paths: &Paths) -> Result<(), String> {
    perform_update_label_at(paths, CTX_KEY)
}

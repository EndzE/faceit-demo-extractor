use std::io;
use std::path::PathBuf;

use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

use crate::Paths;
use crate::config;

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("{name} was not found next to this executable.\nExpected: {}\n\nBuild or copy both binaries first.", path.display())]
    MissingBinary { name: &'static str, path: PathBuf },

    #[error("the right-click menu is not installed; nothing to update")]
    NotInstalled,

    #[error("registry I/O error: {0}")]
    Io(#[from] io::Error),
}

pub const CTX_KEY: &str = r"Software\Classes\SystemFileAssociations\.zst\shell\FaceitExtract";
pub const CTX_COMMAND_KEY: &str =
    r"Software\Classes\SystemFileAssociations\.zst\shell\FaceitExtract\command";
pub const CTX_BZ2_KEY: &str = r"Software\Classes\SystemFileAssociations\.bz2\shell\FaceitExtract";
pub const CTX_BZ2_COMMAND_KEY: &str =
    r"Software\Classes\SystemFileAssociations\.bz2\shell\FaceitExtract\command";

/// Every extension whose Explorer right-click menu is installed and removed.
pub const CTX_MENUS: &[ContextMenu] = &[
    ContextMenu {
        extension: ".zst",
        key: CTX_KEY,
        command_key: CTX_COMMAND_KEY,
    },
    ContextMenu {
        extension: ".bz2",
        key: CTX_BZ2_KEY,
        command_key: CTX_BZ2_COMMAND_KEY,
    },
];

/// One Explorer right-click menu registration.
#[derive(Debug, Clone, Copy)]
pub struct ContextMenu {
    pub extension: &'static str,
    pub key: &'static str,
    pub command_key: &'static str,
}

impl ContextMenu {
    #[must_use]
    pub fn installed(&self) -> bool {
        reg_key_exists(self.key)
    }

    #[must_use]
    pub fn label(&self) -> Option<String> {
        reg_default(self.key)
    }

    #[must_use]
    pub fn command(&self) -> Option<String> {
        reg_default(self.command_key)
    }
}

#[must_use]
pub fn menu_label_for(target_filename: &str) -> String {
    format!("Extract to CS2 folder ({target_filename})")
}

pub fn ensure_install_binaries(paths: &Paths) -> Result<(), RegistryError> {
    for (exe, name) in [
        (paths.quiet_exe(), "faceit-dem-quiet.exe"),
        (paths.console_exe(), "faceit-dem.exe"),
    ] {
        if !exe.is_file() {
            return Err(RegistryError::MissingBinary { name, path: exe });
        }
    }

    Ok(())
}

#[must_use]
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

#[must_use]
pub fn context_menu_command() -> Option<String> {
    CTX_MENUS.iter().find_map(ContextMenu::command)
}

#[must_use]
pub fn context_menu_installed() -> bool {
    CTX_MENUS.iter().any(ContextMenu::installed)
}

#[must_use]
pub fn context_menu_state_for(menus: &[ContextMenu], paths: &Paths) -> Option<bool> {
    if !menus.iter().any(ContextMenu::installed) {
        return None;
    }
    let expected_command = build_context_menu_command(paths);
    Some(menus.iter().all(|menu| {
        menu.installed()
            && menu.label().is_some()
            && menu.command().as_deref() == Some(&expected_command)
    }))
}

/// Overall right-click state: `None` when no menu is installed, `Some(true)`
/// when every menu is installed with a command and label matching the current paths,
/// `Some(false)` when at least one installed menu needs repair.
#[must_use]
pub fn context_menu_state(paths: &Paths) -> Option<bool> {
    context_menu_state_for(CTX_MENUS, paths)
}

pub fn install_context_menu_at(
    root: &str,
    label: &str,
    command: &str,
) -> Result<(), RegistryError> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(root)?;
    key.set_value("", &label)?;

    let (command_key, _) = hkcu.create_subkey(format!("{root}\\command"))?;
    command_key.set_value("", &command)?;

    Ok(())
}

pub fn set_context_menu_label_at(root: &str, label: &str) -> Result<(), RegistryError> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(root)?;
    key.set_value("", &label)?;
    Ok(())
}

pub fn remove_context_menu_at(root: &str) -> Result<(), RegistryError> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    match hkcu.delete_subkey_all(root) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(RegistryError::Io(err)),
    }
}

fn expected_menu_label(paths: &Paths) -> String {
    let config = config::load_config(&paths.config);
    if config.rename_demo {
        menu_label_for(&config.target_filename)
    } else {
        "Extract to CS2 folder".to_string()
    }
}

fn install_context_menu_with(paths: &Paths, root: &str) -> Result<(), RegistryError> {
    ensure_install_binaries(paths)?;
    install_context_menu_at(
        root,
        &expected_menu_label(paths),
        &build_context_menu_command(paths),
    )
}

pub fn perform_install(paths: &Paths) -> Result<(), RegistryError> {
    for menu in CTX_MENUS {
        install_context_menu_with(paths, menu.key)?;
    }
    Ok(())
}

pub fn perform_remove() -> Result<(), RegistryError> {
    for menu in CTX_MENUS {
        remove_context_menu_at(menu.key)?;
    }
    Ok(())
}

pub fn perform_update_label_at(paths: &Paths, root: &str) -> Result<(), RegistryError> {
    if reg_default(&format!("{root}\\command")).is_none() {
        if reg_key_exists(root) {
            return install_context_menu_with(paths, root);
        }
        return Err(RegistryError::NotInstalled);
    }

    set_context_menu_label_at(root, &expected_menu_label(paths))
}

pub fn perform_update_label(paths: &Paths) -> Result<(), RegistryError> {
    let mut any_installed = false;
    for menu in CTX_MENUS.iter().filter(|menu| menu.installed()) {
        any_installed = true;
        perform_update_label_at(paths, menu.key)?;
    }

    if !any_installed {
        return Err(RegistryError::NotInstalled);
    }

    Ok(())
}

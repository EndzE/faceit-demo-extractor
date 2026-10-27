//! Windows Registry configuration for Explorer right-click shell context menus.

use std::io;
use std::path::PathBuf;

use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

use crate::Paths;
use crate::config;

/// Errors that can occur during registry inspection or modification.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// A required binary was missing from next to the current executable.
    #[error("{name} was not found next to this executable.\nExpected: {}\n\nBuild or copy both binaries first.", path.display())]
    MissingBinary {
        /// Display filename of the missing binary.
        name: &'static str,
        /// Expected path where the binary was looked up.
        path: PathBuf,
    },

    /// The context menu is not installed in the registry.
    #[error("the right-click menu is not installed; nothing to update")]
    NotInstalled,

    /// Registry read or write failure.
    #[error("registry I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Registry subkey for the `.zst` file association shell verb.
pub const CTX_KEY: &str = r"Software\Classes\SystemFileAssociations\.zst\shell\FaceitExtract";
/// Registry subkey for the `.zst` shell verb command string.
pub const CTX_COMMAND_KEY: &str =
    r"Software\Classes\SystemFileAssociations\.zst\shell\FaceitExtract\command";
/// Registry subkey for the `.bz2` file association shell verb.
pub const CTX_BZ2_KEY: &str = r"Software\Classes\SystemFileAssociations\.bz2\shell\FaceitExtract";
/// Registry subkey for the `.bz2` shell verb command string.
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

/// One Explorer right-click menu registration definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContextMenu {
    /// Target file extension (e.g. `.zst` or `.bz2`).
    pub extension: &'static str,
    /// Registry key path defining the shell verb and label.
    pub key: &'static str,
    /// Registry key path defining the execution command line.
    pub command_key: &'static str,
}

impl ContextMenu {
    /// Checks whether this context menu verb key exists in the registry.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        reg_key_exists(self.key)
    }

    /// Reads the current display label string from the registry default value.
    #[must_use]
    pub fn label(&self) -> Option<String> {
        reg_default(self.key)
    }

    /// Reads the current execution command string from the registry default value.
    #[must_use]
    pub fn command(&self) -> Option<String> {
        reg_default(self.command_key)
    }
}

/// Generates the menu label string for the given target demo filename.
#[must_use]
pub fn menu_label_for(target_filename: &str) -> String {
    format!("Extract to CS2 folder ({target_filename})")
}

/// Verifies that both `faceit-dem.exe` and `faceit-dem-quiet.exe` exist before registering.
///
/// # Errors
///
/// Returns [`RegistryError::MissingBinary`] if either required binary is absent.
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

/// Constructs the Explorer invocation command line for `faceit-dem-quiet.exe`.
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

/// Returns the installed context menu command string if any menu is installed.
#[must_use]
pub fn context_menu_command() -> Option<String> {
    CTX_MENUS.iter().find_map(ContextMenu::command)
}

/// Checks whether at least one context menu verb is present in the registry.
#[must_use]
pub fn is_context_menu_installed() -> bool {
    CTX_MENUS.iter().any(ContextMenu::is_installed)
}

/// Computes the overall configuration health of a set of context menus against expected paths.
#[must_use]
pub fn context_menu_state_for(menus: &[ContextMenu], paths: &Paths) -> Option<bool> {
    if !menus.iter().any(ContextMenu::is_installed) {
        return None;
    }
    let expected_command = build_context_menu_command(paths);
    Some(menus.iter().all(|menu| {
        menu.is_installed()
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

/// Writes registry keys configuring a context menu verb and execution command under `root`.
///
/// # Errors
///
/// Returns a [`RegistryError`] if creating subkeys or setting values fails.
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

/// Updates the display label default value for the context menu subkey at `root`.
///
/// # Errors
///
/// Returns a [`RegistryError`] if setting the value fails.
pub fn set_context_menu_label_at(root: &str, label: &str) -> Result<(), RegistryError> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(root)?;
    key.set_value("", &label)?;
    Ok(())
}

/// Recursively removes a context menu subkey tree from the registry.
///
/// # Errors
///
/// Returns a [`RegistryError`] if deletion fails.
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

/// Registers all Explorer context menus defined in [`CTX_MENUS`].
///
/// # Errors
///
/// Returns a [`RegistryError`] if binary verification or registry creation fails.
pub fn perform_install(paths: &Paths) -> Result<(), RegistryError> {
    for menu in CTX_MENUS {
        install_context_menu_with(paths, menu.key)?;
    }
    Ok(())
}

/// Unregisters and deletes all Explorer context menus defined in [`CTX_MENUS`].
///
/// # Errors
///
/// Returns a [`RegistryError`] if deletion fails.
pub fn perform_remove() -> Result<(), RegistryError> {
    for menu in CTX_MENUS {
        remove_context_menu_at(menu.key)?;
    }
    Ok(())
}

/// Updates the context menu label for a single menu root according to current configuration.
///
/// # Errors
///
/// Returns [`RegistryError::NotInstalled`] if the menu does not exist, or an I/O error on failure.
pub fn perform_update_label_at(paths: &Paths, root: &str) -> Result<(), RegistryError> {
    if reg_default(&format!("{root}\\command")).is_none() {
        if reg_key_exists(root) {
            return install_context_menu_with(paths, root);
        }
        return Err(RegistryError::NotInstalled);
    }

    set_context_menu_label_at(root, &expected_menu_label(paths))
}

/// Updates context menu labels for all currently installed menus.
///
/// # Errors
///
/// Returns [`RegistryError::NotInstalled`] if no menus are installed, or an I/O error on failure.
pub fn perform_update_label(paths: &Paths) -> Result<(), RegistryError> {
    let mut any_installed = false;
    for menu in CTX_MENUS.iter().filter(|menu| menu.is_installed()) {
        any_installed = true;
        perform_update_label_at(paths, menu.key)?;
    }

    if !any_installed {
        return Err(RegistryError::NotInstalled);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_context_menu_traits() {
        let menu1 = ContextMenu {
            extension: ".zst",
            key: CTX_KEY,
            command_key: CTX_COMMAND_KEY,
        };
        let menu2 = menu1;
        assert_eq!(menu1, menu2);

        let mut set = HashSet::new();
        set.insert(menu1);
        assert!(set.contains(&menu2));
    }
}

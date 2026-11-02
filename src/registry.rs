//! Windows Registry configuration for Explorer right-click shell context menus.

use std::io;
use std::path::PathBuf;

use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

use crate::Paths;

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

/// Display label shown in the Windows Explorer right-click context menu.
pub const MENU_LABEL: &str = "Extract to CS2 folder";

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

/// Registers all Explorer context menus defined in [`CTX_MENUS`].
///
/// # Errors
///
/// Returns a [`RegistryError`] if binary verification or registry creation fails.
pub fn perform_install(paths: &Paths) -> Result<(), RegistryError> {
    ensure_install_binaries(paths)?;
    let command = build_context_menu_command(paths);
    for menu in CTX_MENUS {
        install_context_menu_at(menu.key, MENU_LABEL, &command)?;
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

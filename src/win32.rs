use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::Foundation::{CloseHandle, E_ABORT};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED,
    CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, COORD, FillConsoleOutputAttribute, FillConsoleOutputCharacterW,
    GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleCursorPosition,
};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CreateProcessW, PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW,
};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING,
    FOFX_RECYCLEONDELETE, FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MESSAGEBOX_STYLE, MessageBoxW, SW_HIDE,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

const APP_AUMID: &str = "FaceitDemoExtractor";
const APP_AUMID_KEY: &str = r"Software\Classes\AppUserModelId\FaceitDemoExtractor";
static APP_AUMID_REGISTERED: OnceLock<Result<(), Win32Error>> = OnceLock::new();

#[derive(Debug, Clone, thiserror::Error)]
pub enum Win32Error {
    #[error("Windows API error: {0}")]
    Windows(#[source] windows::core::Error),

    #[error("I/O error: {0}")]
    Io(#[source] Arc<std::io::Error>),

    #[error("registry error: {0}")]
    Registry(String),

    #[error("{0}")]
    Custom(String),
}

impl From<windows::core::Error> for Win32Error {
    fn from(err: windows::core::Error) -> Self {
        Self::Windows(err)
    }
}

impl From<std::io::Error> for Win32Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(Arc::new(err))
    }
}

/// Teardown grace period allowing asynchronous WinRT RPC transmission to the
/// Windows Notification Platform (Action Center) before the COM apartment is uninitialized.
const TOAST_TEARDOWN_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_millis(250);

struct ComGuard(bool);

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: Balances the successful CoInitializeEx call on the same thread.
            unsafe { CoUninitialize() };
        }
    }
}

pub fn recycle_file(path: &Path) -> Result<(), Win32Error> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let normalized = strip_extended_prefix(&absolute);
    if !normalized.exists() && !absolute.exists() {
        return Ok(());
    }

    // SAFETY: Initializes COM for the calling thread as single-threaded apartment (STA),
    // which is required for Windows Shell IFileOperation interfaces.
    let initialized =
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok();
    let _com_guard = ComGuard(initialized);

    let result = (|| -> windows::core::Result<()> {
        // SAFETY: FileOperation is a standard Windows Shell coclass implementing IFileOperation.
        let operation: IFileOperation =
            unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }?;

        let flags = FOF_ALLOWUNDO
            | FOF_NOCONFIRMATION
            | FOF_NOERRORUI
            | FOF_SILENT
            | FOF_WANTNUKEWARNING
            | FOFX_RECYCLEONDELETE;
        // SAFETY: `operation` is a valid COM interface pointer queried above.
        unsafe { operation.SetOperationFlags(flags) }?;

        let path_wide: Vec<u16> = normalized
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: `path_wide` is a valid null-terminated UTF-16 wide string.
        let item: IShellItem =
            unsafe { SHCreateItemFromParsingName(PCWSTR(path_wide.as_ptr()), None) }?;

        // SAFETY: `item` is a valid IShellItem interface pointer; None specifies no custom progress sink.
        unsafe { operation.DeleteItem(&item, None) }?;
        // SAFETY: Executes the scheduled file operations via the Windows Shell subsystem.
        unsafe { operation.PerformOperations() }?;

        // SAFETY: Queries whether any operation in the batch was cancelled by the user.
        let aborted = unsafe { operation.GetAnyOperationsAborted() }?;
        if aborted.as_bool() {
            return Err(windows::core::Error::new(
                E_ABORT,
                "operation was cancelled",
            ));
        }

        Ok(())
    })();

    if result.is_err() && !normalized.exists() && !absolute.exists() {
        return Ok(());
    }

    result.map_err(Win32Error::from)
}

pub fn clear_console() -> Result<(), Win32Error> {
    // SAFETY: STD_OUTPUT_HANDLE retrieves the standard console handle without side effects.
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }?;

    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    // SAFETY: `handle` is queried above and `&mut info` points to a valid, aligned stack structure.
    unsafe { GetConsoleScreenBufferInfo(handle, &mut info) }?;

    let cells = (info.dwSize.X.max(0) as u32).saturating_mul(info.dwSize.Y.max(0) as u32);
    let home = COORD { X: 0, Y: 0 };
    let mut written = 0u32;

    // SAFETY: `handle` is a valid console handle; `home` and `&mut written` are stack references.
    unsafe { FillConsoleOutputCharacterW(handle, u16::from(b' '), cells, home, &mut written) }?;

    // SAFETY: `handle` is a valid console handle; `home` and `&mut written` are stack references.
    unsafe { FillConsoleOutputAttribute(handle, info.wAttributes.0, cells, home, &mut written) }?;

    // SAFETY: `handle` is a valid console handle and `home` is a valid COORD value.
    unsafe { SetConsoleCursorPosition(handle, home) }?;

    Ok(())
}

pub fn available_space(path: &Path) -> Result<u64, Win32Error> {
    let directory: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0u64;

    // SAFETY: `directory` is null-terminated and valid for reads; `available` is a valid stack variable.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(directory.as_ptr()), Some(&mut available), None, None) }?;

    Ok(available)
}

pub fn expand_env(value: &str) -> String {
    let source: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `source` is null-terminated UTF-16; passing None queries the required buffer size.
    let required = unsafe { ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), None) };
    if required == 0 {
        return value.to_string();
    }

    let mut buffer = vec![0u16; required as usize];
    // SAFETY: `source` is null-terminated; `buffer` has allocated capacity matching `required`.
    let written = unsafe { ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), Some(&mut buffer)) };
    if written == 0 || written > required {
        return value.to_string();
    }

    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

fn strip_extended_prefix(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    match components.next() {
        Some(Component::Prefix(prefix_comp)) => match prefix_comp.kind() {
            Prefix::VerbatimDisk(disk) => {
                let mut buf = PathBuf::from(format!("{}:", char::from(disk)));
                for comp in components {
                    buf.push(comp.as_os_str());
                }
                buf
            }
            Prefix::VerbatimUNC(server, share) => {
                let mut buf = PathBuf::from(format!(
                    r"\\{}\{}",
                    server.to_string_lossy(),
                    share.to_string_lossy()
                ));
                for comp in components {
                    buf.push(comp.as_os_str());
                }
                buf
            }
            Prefix::Verbatim(v) => {
                let mut buf = PathBuf::from(v);
                for comp in components {
                    buf.push(comp.as_os_str());
                }
                buf
            }
            _ => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

fn push_quoted(target: &mut Vec<u16>, value: &OsStr) {
    target.push(u16::from(b'"'));

    let mut backslashes = 0;
    for unit in value.encode_wide() {
        if unit == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }

        if unit == u16::from(b'"') {
            target.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
            target.push(u16::from(b'"'));
        } else {
            target.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            target.push(unit);
        }
        backslashes = 0;
    }

    target.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    target.push(u16::from(b'"'));
}

pub fn spawn_new_console(exe: &Path, args: &[&str]) -> Result<(), Win32Error> {
    spawn_console(exe, args, false)
}

pub fn spawn_hidden_console(exe: &Path, args: &[&str]) -> Result<(), Win32Error> {
    spawn_console(exe, args, true)
}

fn spawn_console(exe: &Path, args: &[&str], hidden: bool) -> Result<(), Win32Error> {
    let application: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut command_line: Vec<u16> = Vec::new();
    push_quoted(&mut command_line, exe.as_os_str());
    for arg in args {
        command_line.push(u16::from(b' '));
        push_quoted(&mut command_line, OsStr::new(arg));
    }
    command_line.push(0);

    let mut startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    if hidden {
        startup.dwFlags = STARTF_USESHOWWINDOW;
        startup.wShowWindow = SW_HIDE.0 as u16;
    }
    let mut process = PROCESS_INFORMATION::default();

    // SAFETY: `application` and `command_line` are null-terminated wide strings;
    // `startup` and `process` are properly initialized structs on the stack.
    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NEW_CONSOLE,
            None,
            PCWSTR::null(),
            &startup,
            &mut process,
        )
    }?;

    // SAFETY: `process.hProcess` is a valid handle returned by successful CreateProcessW.
    // INTENTIONAL: The child process was successfully spawned; closing handles detaches it.
    // Handle close failures are logged as warnings and cannot undo the process creation.
    if let Err(err) = unsafe { CloseHandle(process.hProcess) } {
        crate::output::warn(format!("failed to close process handle: {err}"));
    }
    // SAFETY: `process.hThread` is a valid handle returned by successful CreateProcessW.
    // INTENTIONAL: Child thread handle close failure is non-fatal and does not affect the spawned process.
    if let Err(err) = unsafe { CloseHandle(process.hThread) } {
        crate::output::warn(format!("failed to close thread handle: {err}"));
    }

    Ok(())
}

fn register_app_aumid() -> Result<(), Win32Error> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(APP_AUMID_KEY)
        .map_err(|err| {
            Win32Error::Registry(format!("could not register the notification app id: {err}"))
        })?;
    key.set_value("DisplayName", &crate::APP_NAME)
        .map_err(|err| {
            Win32Error::Registry(format!("could not set the notification app name: {err}"))
        })?;
    Ok(())
}

fn ensure_app_aumid() -> Result<(), Win32Error> {
    APP_AUMID_REGISTERED.get_or_init(register_app_aumid).clone()
}

fn create_toast_document(title: &str, message: &str) -> windows::core::Result<XmlDocument> {
    let document = XmlDocument::new()?;
    let toast = document.CreateElement(&HSTRING::from("toast"))?;
    let visual = document.CreateElement(&HSTRING::from("visual"))?;
    let binding = document.CreateElement(&HSTRING::from("binding"))?;
    binding.SetAttribute(&HSTRING::from("template"), &HSTRING::from("ToastGeneric"))?;

    for text in [title, message] {
        let node = document.CreateElement(&HSTRING::from("text"))?;
        node.SetInnerText(&HSTRING::from(text))?;
        binding.AppendChild(&node)?;
    }

    visual.AppendChild(&binding)?;
    toast.AppendChild(&visual)?;
    document.AppendChild(&toast)?;
    Ok(document)
}

pub fn notify(title: &str, message: &str) -> Result<(), Win32Error> {
    ensure_app_aumid()?;

    // SAFETY: Initializes COM for the current thread with multithreaded concurrency.
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let _com_guard = ComGuard(initialized);

    let document = create_toast_document(title, message)?;
    let toast = ToastNotification::CreateToastNotification(&document)?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_AUMID))?;
    notifier.Show(&toast)?;

    // WinRT ToastNotifier::Show transmits notifications via asynchronous RPC to the
    // Windows Notification Platform (Action Center). Allow an adequate teardown grace
    // period before uninitializing COM and terminating the process so the IPC payload
    // is reliably accepted and not dropped prematurely.
    std::thread::sleep(TOAST_TEARDOWN_GRACE_PERIOD);

    Ok(())
}

fn show_message_box(title: &str, text: &str, icon: MESSAGEBOX_STYLE) {
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let text: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: `text` and `title` are null-terminated wide strings that outlive the call.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | icon,
        );
    }
}

pub fn message_box(title: &str, text: &str) {
    show_message_box(title, text, MB_ICONERROR);
}

pub fn message_box_info(title: &str, text: &str) {
    show_message_box(title, text, MB_ICONINFORMATION);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(value: &str) -> String {
        let mut buffer = Vec::new();
        push_quoted(&mut buffer, OsStr::new(value));
        String::from_utf16(&buffer).unwrap()
    }

    #[test]
    fn quoting_matches_windows_rules() {
        assert_eq!(quoted("plain"), "\"plain\"");
        assert_eq!(quoted(r"C:\folder\"), "\"C:\\folder\\\\\"");
        assert_eq!(quoted("a\"b"), "\"a\\\"b\"");
        assert_eq!(
            quoted("a\\\\\"b"),
            format!("\"a{}{}b\"", "\\".repeat(5), '"')
        );
    }

    #[test]
    fn strip_extended_prefix_preserves_and_strips() {
        assert_eq!(
            strip_extended_prefix(Path::new(r"\\?\C:\foo\bar")),
            PathBuf::from(r"C:\foo\bar")
        );
        assert_eq!(
            strip_extended_prefix(Path::new(r"\\?\UNC\server\share\folder\file.txt")),
            PathBuf::from(r"\\server\share\folder\file.txt")
        );
        assert_eq!(
            strip_extended_prefix(Path::new(r"C:\foo\bar")),
            PathBuf::from(r"C:\foo\bar")
        );
        assert_eq!(
            strip_extended_prefix(Path::new(r"foo\bar")),
            PathBuf::from(r"foo\bar")
        );
    }

    #[test]
    fn quoting_preserves_non_unicode_paths() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let value = OsString::from_wide(&[0xD800]);
        let path = Path::new(&value);
        let mut buffer = Vec::new();
        push_quoted(&mut buffer, path.as_os_str());

        assert_eq!(buffer, vec![u16::from(b'"'), 0xD800, u16::from(b'"')]);
    }

    #[test]
    #[ignore]
    fn test_recycle_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("recycle_me.txt");
        std::fs::write(&test_file, "hello to recycle bin").unwrap();
        assert!(test_file.exists());

        let res = recycle_file(&test_file);
        assert!(res.is_ok());
        assert!(!test_file.exists());

        // Test non-existent file
        let non_existent = temp_dir.path().join("does_not_exist.txt");
        assert!(recycle_file(&non_existent).is_ok());

        // Test long path (> 260 characters)
        let mut deep_dir = temp_dir.path().to_path_buf();
        for i in 0..15 {
            deep_dir = deep_dir.join(format!("long_folder_name_level_{i}"));
        }
        std::fs::create_dir_all(&deep_dir).unwrap();
        let long_file = deep_dir.join("long_path_file_to_recycle.txt");
        std::fs::write(&long_file, "deep nested file").unwrap();
        let long_file_len = long_file.to_str().unwrap().len();
        assert!(long_file_len > 260);
        assert!(long_file.exists());

        let res_long = recycle_file(&long_file);
        assert!(res_long.is_ok());
        assert!(!long_file.exists());
    }

    #[test]
    #[ignore]
    fn test_notify() {
        let res = notify(
            "Faceit Demo Extractor Test",
            "Testing notification delivery",
        );
        assert!(res.is_ok());
    }
}

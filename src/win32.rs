use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;

use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, COORD, FillConsoleOutputAttribute, FillConsoleOutputCharacterW,
    GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleCursorPosition,
};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CreateProcessW, PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW,
};
use windows::Win32::UI::Shell::{FO_DELETE, SHFILEOPSTRUCTW, SHFileOperationW};
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MESSAGEBOX_STYLE, MessageBoxW, SW_HIDE,
};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR};

const FOF_WANTNUKEWARNING: u16 = 0x4000;
const FOF_FLAGS: u16 = 0x0004 | 0x0010 | 0x0040 | 0x0400 | FOF_WANTNUKEWARNING;

const APP_AUMID: &str = "FaceitDemoExtractor";
const APP_AUMID_KEY: &str = r"Software\Classes\AppUserModelId\FaceitDemoExtractor";
static APP_AUMID_REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();

pub fn recycle_file(path: &Path) -> Result<(), String> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut from: Vec<u16> = absolute
        .as_os_str()
        .encode_wide()
        .chain([0u16, 0u16])
        .collect();

    let mut operation = SHFILEOPSTRUCTW {
        hwnd: HWND::default(),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_mut_ptr()),
        pTo: PCWSTR::null(),
        fFlags: FOF_FLAGS,
        fAnyOperationsAborted: BOOL(0),
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: PCWSTR::null(),
    };

    // SAFETY: `operation` is a valid struct on the stack. `pFrom` points to `from`
    // which is guaranteed to be double-null terminated and lives for the duration of the call.
    let result = unsafe { SHFileOperationW(&mut operation) };

    if operation.fAnyOperationsAborted.as_bool() {
        return Err("operation was cancelled".to_string());
    }

    if result == 0 {
        return Ok(());
    }

    if !absolute.exists() {
        return Ok(());
    }

    Err(format!("SHFileOperationW returned {result}"))
}

pub fn clear_console() -> Result<(), String> {
    // SAFETY: STD_OUTPUT_HANDLE retrieves the standard console handle without side effects.
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.map_err(|err| err.to_string())?;

    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    // SAFETY: `handle` is queried above and `&mut info` points to a valid, aligned stack structure.
    unsafe { GetConsoleScreenBufferInfo(handle, &mut info) }.map_err(|err| err.to_string())?;

    let cells = (info.dwSize.X.max(0) as u32).saturating_mul(info.dwSize.Y.max(0) as u32);
    let home = COORD { X: 0, Y: 0 };
    let mut written = 0u32;

    // SAFETY: `handle` is a valid console handle; `home` and `&mut written` are stack references.
    unsafe { FillConsoleOutputCharacterW(handle, u16::from(b' '), cells, home, &mut written) }
        .map_err(|err| err.to_string())?;

    // SAFETY: `handle` is a valid console handle; `home` and `&mut written` are stack references.
    unsafe { FillConsoleOutputAttribute(handle, info.wAttributes.0, cells, home, &mut written) }
        .map_err(|err| err.to_string())?;

    // SAFETY: `handle` is a valid console handle and `home` is a valid COORD value.
    unsafe { SetConsoleCursorPosition(handle, home) }.map_err(|err| err.to_string())?;

    Ok(())
}

pub fn available_space(path: &Path) -> Result<u64, String> {
    let directory: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0u64;

    // SAFETY: `directory` is null-terminated and valid for reads; `available` is a valid stack variable.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(directory.as_ptr()), Some(&mut available), None, None) }
        .map_err(|err| err.to_string())?;

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

fn push_quoted(target: &mut Vec<u16>, value: &OsStr) {
    target.push('"' as u16);

    let mut backslashes = 0;
    for unit in value.encode_wide() {
        if unit == '\\' as u16 {
            backslashes += 1;
            continue;
        }

        if unit == '"' as u16 {
            target.extend(std::iter::repeat_n('\\' as u16, backslashes * 2 + 1));
            target.push('"' as u16);
        } else {
            target.extend(std::iter::repeat_n('\\' as u16, backslashes));
            target.push(unit);
        }
        backslashes = 0;
    }

    target.extend(std::iter::repeat_n('\\' as u16, backslashes * 2));
    target.push('"' as u16);
}

pub fn spawn_new_console(exe: &Path, args: &[&str]) -> Result<(), String> {
    spawn_console(exe, args, false)
}

pub fn spawn_hidden_console(exe: &Path, args: &[&str]) -> Result<(), String> {
    spawn_console(exe, args, true)
}

fn spawn_console(exe: &Path, args: &[&str], hidden: bool) -> Result<(), String> {
    let application: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut command_line: Vec<u16> = Vec::new();
    push_quoted(&mut command_line, exe.as_os_str());
    for arg in args {
        command_line.push(' ' as u16);
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
    }
    .map_err(|err| err.to_string())?;

    // SAFETY: `process.hProcess` is a valid handle returned by successful CreateProcessW.
    let _ = unsafe { CloseHandle(process.hProcess) };
    // SAFETY: `process.hThread` is a valid handle returned by successful CreateProcessW.
    let _ = unsafe { CloseHandle(process.hThread) };

    Ok(())
}

fn register_app_aumid() -> Result<(), String> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(APP_AUMID_KEY)
        .map_err(|err| format!("could not register the notification app id: {err}"))?;
    key.set_value("DisplayName", &crate::APP_NAME)
        .map_err(|err| format!("could not set the notification app name: {err}"))
}

fn ensure_app_aumid() -> Result<(), String> {
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

pub fn notify(title: &str, message: &str) -> Result<(), String> {
    ensure_app_aumid()?;

    // SAFETY: Initializes COM for the current thread with multithreaded concurrency.
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();

    let result = (|| -> windows::core::Result<()> {
        let document = create_toast_document(title, message)?;
        let toast = ToastNotification::CreateToastNotification(&document)?;
        let notifier =
            ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_AUMID))?;
        notifier.Show(&toast)?;
        std::thread::sleep(std::time::Duration::from_millis(10));
        Ok(())
    })();

    if initialized {
        // SAFETY: Balances the successful CoInitializeEx call on the same thread.
        unsafe { CoUninitialize() };
    }

    result.map_err(|err| err.to_string())
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
    fn quoting_preserves_non_unicode_paths() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let value = OsString::from_wide(&[0xD800]);
        let path = Path::new(&value);
        let mut buffer = Vec::new();
        push_quoted(&mut buffer, path.as_os_str());

        assert_eq!(buffer, vec!['"' as u16, 0xD800, '"' as u16]);
    }
}

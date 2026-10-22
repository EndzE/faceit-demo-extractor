use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::Shell::{FO_DELETE, SHFILEOPSTRUCTW, SHFileOperationW};
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MESSAGEBOX_STYLE, MessageBoxW,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

const FOF_FLAGS: u16 = 0x0004 | 0x0010 | 0x0040 | 0x0400;

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

    let result = unsafe { SHFileOperationW(&mut operation) };

    if result == 0 && !operation.fAnyOperationsAborted.as_bool() {
        return Ok(());
    }

    if !absolute.exists() {
        return Ok(());
    }

    Err(format!("SHFileOperationW returned {result}"))
}

pub fn available_space(path: &Path) -> Result<u64, String> {
    let directory: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0u64;

    unsafe {
        GetDiskFreeSpaceExW(PCWSTR(directory.as_ptr()), Some(&mut available), None, None)
            .map_err(|err| err.to_string())?;
    }

    Ok(available)
}

pub fn expand_env(value: &str) -> String {
    let source: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let required = unsafe { ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), None) };
    if required == 0 {
        return value.to_string();
    }

    let mut buffer = vec![0u16; required as usize];
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

    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();

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
        .map_err(|err| err.to_string())?;

        let _ = CloseHandle(process.hProcess);
        let _ = CloseHandle(process.hThread);
    }

    Ok(())
}

fn show_message_box(title: &str, text: &str, icon: MESSAGEBOX_STYLE) {
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let text: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();

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

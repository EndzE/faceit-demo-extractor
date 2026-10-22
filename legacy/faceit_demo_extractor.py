#!/usr/bin/env python3
r"""
FACEIT Demo Extractor - self-contained .dem.zst extractor with per-user context-menu setup.

WHAT IT DOES
    Decompresses a .zst archive (a FACEIT/CS2 demo, .dem.zst) straight into
    the game's csgo folder as a .dem file (1.dem by default), then either
    keeps, permanently deletes, or recycles the original archive depending
    on config.json.

Safety first:
    * every decompressed file is verified to be a real CS2 demo (magic
      header check) before anything is overwritten or deleted;
    * multiple archives dropped at once are extracted side by side as
      1_1.dem, 1_2.dem, ... instead of overwriting each other;
    * extraction is atomic (unique temp file + replace), so a failed
      run never corrupts an existing demo.

CONTEXT MENU
    The entry is registered per-user under:

        HKEY_CURRENT_USER\Software\Classes\SystemFileAssociations\.zst\shell\FaceitExtract

    No administrator rights are required.

    Because Windows may invoke a static context-menu verb once per selected
    file, the context-menu command uses a queue/worker model:

        "<pythonw.exe>" "<this script>" --context-menu "%1"

    Each invocation queues the file. The first invocation starts one worker
    console:

        "<python.exe>" "<this script>" --queue-worker

    The worker collects queued files and extracts them as one batch.

SETUP
    1. Put this file in its own folder, e.g. C:\Apps\faceit_demo_extractor\
    2. Create a venv there and install the dependency yourself:

           python -m venv .venv
           .venv\Scripts\pip install zstandard

    3. Run it with that venv's python:

           .venv\Scripts\python faceit_demo_extractor.py

       and pick option 1 to register the per-user context-menu entry.

USAGE
    faceit_demo_extractor.py                      interactive menu
    faceit_demo_extractor.py <file.zst> [...]     extract file(s) directly
    faceit_demo_extractor.py -- <file> [...]      same, for names starting with "-"
    faceit_demo_extractor.py --install            add per-user context-menu entry
    faceit_demo_extractor.py --remove             remove per-user context-menu entry
    faceit_demo_extractor.py --status             show current configuration
    faceit_demo_extractor.py --help               show help

CS2 FOLDER DETECTION
    1. HKLM\SOFTWARE\WOW6432Node\Valve\cs2
           value "installpath"
    2. HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Steam App 730
           value "InstallLocation"
       plus the WOW6432Node twin of that key as an extra fallback.

    "\game\csgo" is appended to the located install path automatically.

CONFIGURATION
    config.json is created with defaults next to this script on first start:

        {
          "target_filename": "1.dem",
          "keep_zst": false,
          "move_to_recycle_bin": false
        }

    - target_filename:
        Single-archive extractions write this name. A batch writes
        1_1.dem, 1_2.dem, ... using the first free slots.

    - keep_zst:
        When true, the original archive is kept instead of deleted.

    - move_to_recycle_bin:
        When true, and keep_zst is false, the original archive is moved to
        the Windows Recycle Bin. If this fails, the original is kept.

ZSTANDARD
    This script does NOT install zstandard. Install it yourself for the
    Python interpreter shown by --status / used by the context menu.
"""

import contextlib
import ctypes
import importlib.util
import io
import json
import ntpath
import os
import subprocess
import sys
import tempfile
import time
import unicodedata
import winreg
from collections.abc import Mapping
from pathlib import Path
from typing import TypedDict

# ---------------------------------------------------------------------------
# constants
# ---------------------------------------------------------------------------

APP_NAME = "FACEIT Demo Extractor"

SCRIPT_FILE = str(Path(__file__).resolve())
SCRIPT_DIR = str(Path(SCRIPT_FILE).parent)
CONFIG_FILE = str(Path(SCRIPT_DIR) / "config.json")

DEFAULT_TARGET_FILENAME = "1.dem"
DEFAULT_KEEP_ZST = False
DEFAULT_MOVE_TO_RECYCLE_BIN = False

# A decompressed CS2 demo must start with this magic ("PBDEMS2\0").
DEMO_MAGIC = b"PBDEMS2\x00"

# Longest file name Windows can store in a single directory entry.
MAX_FILENAME_LEN = 255

# CS2 install location lookup - checked in order.
CS2_REG_LOOKUP = (
    (r"SOFTWARE\WOW6432Node\Valve\cs2", "installpath"),
    (
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Steam App 730",
        "InstallLocation",
    ),
    (
        r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Steam App 730",
        "InstallLocation",
    ),
)
CS2_CSGO_REL = ("game", "csgo")

# Per-user context menu location. No UAC required.
CTX_KEY = r"Software\Classes\SystemFileAssociations\.zst\shell\FaceitExtract"
CTX_COMMAND_KEY = CTX_KEY + r"\command"
CTX_ROOT_DESC = "HKEY_CURRENT_USER\\" + CTX_KEY

INVALID_FILENAME_CHARS = '<>:"|?*'

_WINDOWS_RESERVED_NAMES = (
    {"CON", "PRN", "AUX", "NUL"}
    | {f"COM{i}" for i in range(1, 10)}
    | {f"LPT{i}" for i in range(1, 10)}
)

_WOW64_64KEY = winreg.KEY_WOW64_64KEY

# Queue used to coalesce multiple per-file context-menu invocations.
QUEUE_DIR = Path(tempfile.gettempdir()) / "faceit_demo_extractor.queue"
WORKER_LOCK = QUEUE_DIR / "worker.lock"


# Validated contents of config.json.
class Config(TypedDict):
    target_filename: str
    keep_zst: bool
    move_to_recycle_bin: bool


# Recycle Bin support via SHFileOperationW - no COM, no external deps.


class _SHFILEOPSTRUCTW(ctypes.Structure):
    _fields_ = [
        ("hwnd", ctypes.c_void_p),
        ("wFunc", ctypes.c_uint),
        ("pFrom", ctypes.c_wchar_p),
        ("pTo", ctypes.c_wchar_p),
        ("fFlags", ctypes.c_ushort),
        ("fAnyOperationsAborted", ctypes.c_int),
        ("hNameMappings", ctypes.c_void_p),
        ("lpszProgressTitle", ctypes.c_wchar_p),
    ]


_FO_DELETE = 3
_FOF_SILENT = 0x0004
_FOF_NOCONFIRMATION = 0x0010
_FOF_ALLOWUNDO = 0x0040
_FOF_NOERRORUI = 0x0400

_shell32 = ctypes.windll.shell32
_shell32.SHFileOperationW.argtypes = (ctypes.POINTER(_SHFILEOPSTRUCTW),)
_shell32.SHFileOperationW.restype = ctypes.c_int

# Worker-liveness check via OpenProcess.

_kernel32 = ctypes.windll.kernel32
_kernel32.OpenProcess.argtypes = (ctypes.c_uint, ctypes.c_int, ctypes.c_uint)
_kernel32.OpenProcess.restype = ctypes.c_void_p
_kernel32.CloseHandle.argtypes = (ctypes.c_void_p,)

_PROCESS_QUERY_LIMITED_INFORMATION = 0x1000


# ---------------------------------------------------------------------------
# console output helpers
# ---------------------------------------------------------------------------

def _stream_reconfigure_errors() -> None:
    """
    Make stdout/stderr survive non-ASCII paths and pythonw-like environments.
    """
    if sys.stdout is None:
        sys.stdout = open(os.devnull, "w", encoding="utf-8", errors="replace")
    if sys.stderr is None:
        sys.stderr = sys.stdout

    for stream in (sys.stdout, sys.stderr):
        if isinstance(stream, io.TextIOWrapper):
            with contextlib.suppress(OSError, ValueError):
                stream.reconfigure(errors="replace")


def _print_prefixed(label: str, message: object, stream=None) -> None:
    """
    Print `message` with a severity label; continuation lines are indented.
    """
    if stream is None:
        return

    lines = str(message).splitlines() or [""]

    for index, line in enumerate(lines):
        if index == 0:
            print(f"{label} {line}", file=stream)
        else:
            print(" " * (len(label) + 1) + line, file=stream)


def warn(message: object) -> None:
    _print_prefixed("WARNING:", message, sys.stderr)


def error(message: object) -> None:
    _print_prefixed("ERROR:", message, sys.stderr)


def pause(message: str = "Press Enter to close...") -> None:
    """
    Keep the console window open.
    """
    if sys.stdin is None:
        return

    try:
        input(message)
    except (EOFError, KeyboardInterrupt):
        pass


def _cleanup(path: str | Path) -> None:
    """
    Best-effort delete of a file that may not exist.
    """
    with contextlib.suppress(OSError):
        Path(path).unlink(missing_ok=True)


# ---------------------------------------------------------------------------
# input validation
# ---------------------------------------------------------------------------

def _utf16_len(value: str) -> int:
    """
    Windows filename limits are measured in UTF-16 code units.
    """
    try:
        return len(value.encode("utf-16-le")) // 2
    except UnicodeEncodeError:
        return MAX_FILENAME_LEN + 1


def is_valid_filename(name: object) -> bool:
    """
    True if `name` is a safe Windows file name, not a path.

    This is intentionally conservative because demo filenames do not need
    exotic Unicode or shell-hostile characters.
    """
    if not isinstance(name, str):
        return False

    if not name or name in (".", ".."):
        return False

    if _utf16_len(name) > MAX_FILENAME_LEN:
        return False

    if ntpath.basename(name) != name:
        return False

    if "/" in name or "\\" in name:
        return False

    if any(ch in name for ch in INVALID_FILENAME_CHARS):
        return False

    if name[-1] in ". ":
        return False

    for ch in name:
        code = ord(ch)

        # Control characters including DEL.
        if code < 32 or code == 127:
            return False

        # Cf: zero-width/bidi/format characters.
        # Cs: unpaired surrogates.
        if unicodedata.category(ch) in ("Cf", "Cs"):
            return False

    # Require a usable stem for `playdemo`.
    stem = name.split(".", 1)[0].strip()
    if not stem:
        return False

    return stem.upper() not in _WINDOWS_RESERVED_NAMES


# ---------------------------------------------------------------------------
# configuration
# ---------------------------------------------------------------------------

_config_warnings_shown: set[str] = set()


def default_config() -> Config:
    return {
        "target_filename": DEFAULT_TARGET_FILENAME,
        "keep_zst": DEFAULT_KEEP_ZST,
        "move_to_recycle_bin": DEFAULT_MOVE_TO_RECYCLE_BIN,
    }


def _warn_config_once(key: str, message: str) -> None:
    if key not in _config_warnings_shown:
        _config_warnings_shown.add(key)
        warn(message)


def _read_config_json() -> dict[str, object]:
    """
    Raw config.json contents as a dict; {} when absent/unreadable/invalid.

    Uses utf-8-sig so Notepad BOMs do not break parsing.
    """
    try:
        raw = Path(CONFIG_FILE).read_text(encoding="utf-8-sig")
    except OSError:
        return {}

    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        return {}

    return data if isinstance(data, dict) else {}


def _coerce_bool(data: dict[str, object], key: str, default: bool) -> bool:
    value = data.get(key, default)
    if isinstance(value, bool):
        return value
    _warn_config_once(
        key,
        f"invalid {key!r} value {value!r} in config.json - "
        f"using the default {default!r}",
    )
    return default


def load_config() -> Config:
    """
    Read config.json; always returns a fully valid config dict.
    """
    data = _read_config_json()

    name = data.get("target_filename", DEFAULT_TARGET_FILENAME)
    if not isinstance(name, str) or not is_valid_filename(name):
        _warn_config_once(
            "target_filename",
            f"invalid \"target_filename\" value {name!r} in config.json - "
            f"using the default {DEFAULT_TARGET_FILENAME!r}",
        )
        name = DEFAULT_TARGET_FILENAME

    return {
        "target_filename": name,
        "keep_zst": _coerce_bool(data, "keep_zst", DEFAULT_KEEP_ZST),
        "move_to_recycle_bin": _coerce_bool(
            data, "move_to_recycle_bin", DEFAULT_MOVE_TO_RECYCLE_BIN
        ),
    }


def save_config(cfg: Mapping[str, object]) -> tuple[bool, str | None]:
    """
    Persist config.json atomically.

    Unknown keys already present in the file are preserved.
    There is intentionally no LocalAppData fallback.
    """
    merged = _read_config_json()
    merged.update(cfg)

    try:
        fd, tmp_path = tempfile.mkstemp(
            dir=SCRIPT_DIR,
            prefix=".config-",
            suffix=".tmp",
        )
    except OSError as exc:
        return False, str(exc)

    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(merged, fh, indent=2, ensure_ascii=False)
            fh.write("\n")
            fh.flush()

            with contextlib.suppress(OSError):
                os.fsync(fh.fileno())

        os.replace(tmp_path, CONFIG_FILE)
        return True, None
    except (OSError, TypeError, ValueError) as exc:
        with contextlib.suppress(OSError):
            os.close(fd)
        _cleanup(tmp_path)
        return False, str(exc)


def ensure_config_file() -> tuple[bool, str | None]:
    """
    Create config.json on first start; add missing keys after upgrades.
    """
    try:
        raw = Path(CONFIG_FILE).read_text(encoding="utf-8-sig")
    except FileNotFoundError:
        ok, err = save_config(default_config())
        if ok:
            print(f"Created default config.json: {CONFIG_FILE}")
        return ok, err
    except OSError as exc:
        warn(f"could not read config.json ({exc}); using built-in defaults")
        return True, None

    try:
        data = json.loads(raw)
    except json.JSONDecodeError as exc:
        warn(f"config.json is not valid JSON ({exc}) - resetting it to defaults")
        return save_config(default_config())

    if not isinstance(data, dict):
        warn("config.json does not contain a JSON object - resetting it to defaults")
        return save_config(default_config())

    missing = [key for key in sorted(default_config()) if key not in data]
    if not missing:
        return True, None

    defaults = dict(default_config())
    ok, err = save_config({key: defaults[key] for key in missing})

    if ok:
        print("Updated config.json with missing setting(s): " + ", ".join(missing))

    return ok, err


# ---------------------------------------------------------------------------
# runtime helpers
# ---------------------------------------------------------------------------

def zstandard_available() -> bool:
    importlib.invalidate_caches()

    try:
        return importlib.util.find_spec("zstandard") is not None
    except (ImportError, ValueError):
        return False


def _as_python_exe(path: str) -> str:
    """
    If given pythonw.exe, prefer the adjacent python.exe.
    """
    p = Path(path)

    if p.name.lower() == "pythonw.exe":
        candidate = p.with_name("python.exe")
        if candidate.is_file():
            return str(candidate)

    return str(p)


def best_python_exe() -> str:
    """
    The python.exe used by the queue worker and recommended for extraction.
    """
    current = _as_python_exe(sys.executable)

    if sys.prefix != sys.base_prefix:
        return current

    for venv_name in (".venv", "venv"):
        candidate = Path(SCRIPT_DIR) / venv_name / "Scripts" / "python.exe"
        if candidate.is_file():
            return str(candidate)

    return current


def best_pythonw_exe() -> str:
    """
    The pythonw.exe used by the context menu command.

    pythonw keeps Explorer invocations silent. If pythonw.exe does not exist,
    fall back to python.exe.
    """
    exe = Path(best_python_exe())

    if exe.name.lower() == "python.exe":
        candidate = exe.with_name("pythonw.exe")
        if candidate.is_file():
            return str(candidate)

    return str(exe)


# ---------------------------------------------------------------------------
# CS2 folder detection
# ---------------------------------------------------------------------------

def _reg_read_str(subkey: str, value_name: str) -> str | None:
    """
    Return a string value from HKLM, or None. Never raises.
    """
    try:
        with winreg.OpenKey(
            winreg.HKEY_LOCAL_MACHINE,
            subkey,
            0,
            winreg.KEY_READ | _WOW64_64KEY,
        ) as key:
            value, vtype = winreg.QueryValueEx(key, value_name)
    except OSError:
        return None

    if not isinstance(value, str):
        return None

    if vtype == winreg.REG_EXPAND_SZ:
        value = os.path.expandvars(value)

    value = value.strip().strip('"').rstrip("\x00").strip()

    return value or None


def find_cs2_csgo_path() -> tuple[str | None, str]:
    """
    Locate "<CS2 install>\\game\\csgo" via the registry.

    Returns (path, source) on success, otherwise (None, reason).
    """
    attempts: list[str] = []

    for subkey, value_name in CS2_REG_LOOKUP:
        raw = _reg_read_str(subkey, value_name)

        if not raw:
            attempts.append(f"HKLM\\{subkey} -> value {value_name!r} not present")
            continue

        csgo = Path(raw.rstrip("\\/")).joinpath(*CS2_CSGO_REL)

        if csgo.is_dir():
            return str(csgo), f"HKLM\\{subkey} -> {value_name}"

        attempts.append(
            f"HKLM\\{subkey} -> {raw!r} points to a missing folder ({csgo})"
        )

    return None, "; ".join(attempts)


# ---------------------------------------------------------------------------
# context menu - per-user HKCU
# ---------------------------------------------------------------------------

def menu_label_for(target_filename: str) -> str:
    return f"Extract to CS2 folder ({target_filename})"


def build_context_menu_command() -> str:
    return f'"{best_pythonw_exe()}" "{SCRIPT_FILE}" --context-menu "%1"'


def _reg_default(subkey: str) -> str | None:
    """
    Default string value of an HKCU subkey, or None if absent.
    """
    try:
        with winreg.OpenKey(
            winreg.HKEY_CURRENT_USER,
            subkey,
            0,
            winreg.KEY_READ,
        ) as key:
            value, _ = winreg.QueryValueEx(key, None)
            return value if isinstance(value, str) else None
    except OSError:
        return None


def context_menu_command() -> str | None:
    return _reg_default(CTX_COMMAND_KEY)


def context_menu_label() -> str | None:
    return _reg_default(CTX_KEY)


def context_menu_installed() -> bool:
    try:
        with winreg.OpenKey(
            winreg.HKEY_CURRENT_USER,
            CTX_KEY,
            0,
            winreg.KEY_READ,
        ):
            return True
    except OSError:
        return False


def install_context_menu(label: str) -> tuple[bool, str | None]:
    command = build_context_menu_command()

    try:
        with winreg.CreateKeyEx(
            winreg.HKEY_CURRENT_USER,
            CTX_KEY,
            0,
            winreg.KEY_WRITE,
        ) as key:
            winreg.SetValueEx(key, None, 0, winreg.REG_SZ, label)

        with winreg.CreateKeyEx(
            winreg.HKEY_CURRENT_USER,
            CTX_COMMAND_KEY,
            0,
            winreg.KEY_WRITE,
        ) as key:
            winreg.SetValueEx(key, None, 0, winreg.REG_SZ, command)

        return True, None
    except OSError as exc:
        return False, str(exc)


def set_context_menu_label(label: str) -> tuple[bool, str | None]:
    try:
        with winreg.CreateKeyEx(
            winreg.HKEY_CURRENT_USER,
            CTX_KEY,
            0,
            winreg.KEY_WRITE,
        ) as key:
            winreg.SetValueEx(key, None, 0, winreg.REG_SZ, label)

        return True, None
    except OSError as exc:
        return False, str(exc)


def _delete_key_recursive(hive: int, subkey: str) -> None:
    """
    Delete `subkey` and everything below it.
    """
    try:
        with winreg.OpenKey(hive, subkey, 0, winreg.KEY_READ) as key:
            while True:
                try:
                    child = winreg.EnumKey(key, 0)
                except OSError:
                    break

                _delete_key_recursive(hive, f"{subkey}\\{child}")

        winreg.DeleteKey(hive, subkey)
    except FileNotFoundError:
        pass


def remove_context_menu() -> tuple[bool, str | None]:
    try:
        _delete_key_recursive(winreg.HKEY_CURRENT_USER, CTX_KEY)
        return True, None
    except OSError as exc:
        return False, str(exc)




# ---------------------------------------------------------------------------
# registry actions
# ---------------------------------------------------------------------------

def _perform_install() -> tuple[bool, str | None]:
    return install_context_menu(menu_label_for(load_config()["target_filename"]))


def _perform_remove() -> tuple[bool, str | None]:
    return remove_context_menu()


def _perform_update_label() -> tuple[bool, str | None]:
    if context_menu_command() is None:
        if context_menu_installed():
            return _perform_install()

        return False, "the context-menu entry is not installed (nothing to update)"

    return set_context_menu_label(menu_label_for(load_config()["target_filename"]))


def run_registry_action(flag: str) -> int:
    perform = {
        "--install": _perform_install,
        "--remove": _perform_remove,
        "--update-label": _perform_update_label,
    }[flag]

    print(f"--> Running {flag}")

    ok, err = perform()

    if ok:
        print("    done.")

        if flag == "--install":
            print("    right-click any .zst file to try it out")

        return 0

    error(err)
    pause()
    return 1


# ---------------------------------------------------------------------------
# deletion / recycle bin
# ---------------------------------------------------------------------------

def _delete_permanently(path: str) -> tuple[bool, str | None]:
    try:
        Path(path).unlink(missing_ok=True)
        return True, None
    except OSError as exc:
        return False, str(exc)


def _delete_to_recycle_bin(path: str) -> tuple[bool, str | None]:
    """
    Move a file to the Windows Recycle Bin using SHFileOperationW.

    No external dependencies. If the operation fails, the caller should
    treat the original file as kept.
    """
    op = _SHFILEOPSTRUCTW()
    op.hwnd = None
    op.wFunc = _FO_DELETE
    op.pFrom = path + "\0\0"
    op.pTo = None
    op.fFlags = _FOF_SILENT | _FOF_NOCONFIRMATION | _FOF_ALLOWUNDO | _FOF_NOERRORUI
    op.fAnyOperationsAborted = 0
    op.hNameMappings = None
    op.lpszProgressTitle = None

    try:
        result = _shell32.SHFileOperationW(ctypes.byref(op))
    except Exception as exc:  # ctypes raises exotic errors; never propagate
        return False, str(exc)

    if result == 0 and op.fAnyOperationsAborted == 0:
        return True, None

    if not os.path.exists(path):
        return True, None

    return False, f"SHFileOperationW returned {result}"


def delete_original(path: str, *, use_recycle_bin: bool) -> tuple[bool, str | None]:
    if use_recycle_bin:
        return _delete_to_recycle_bin(path)

    return _delete_permanently(path)


# ---------------------------------------------------------------------------
# extraction
# ---------------------------------------------------------------------------

def _next_free_name(folder: str, stem: str, ext: str) -> str:
    """
    First unused "<stem>_<n><ext>" name in `folder` (n starts at 1).
    """
    # Leave room for "_<n>" (up to 4 digits) and the timestamp fallback;
    # trim by UTF-16 units to match the Windows limit.
    while stem and _utf16_len(stem) + _utf16_len(ext) + 6 > MAX_FILENAME_LEN:
        stem = stem[:-1]

    for n in range(1, 10000):
        candidate = f"{stem}_{n}{ext}"
        if not os.path.exists(os.path.join(folder, candidate)):
            return candidate

    return f"{stem}_{int(time.time())}{ext}"


def extract_one(
    input_file: str,
    output_path: str,
    *,
    keep_zst: bool,
    move_to_recycle_bin: bool,
) -> bool:
    """
    Decompress one .zst archive to output_path atomically.

    The original archive is only removed after the decompressed payload has
    passed the CS2 demo magic check and the final file has been replaced.
    """
    if not os.path.isfile(input_file):
        error(f"input file not found: {input_file}")
        return False

    try:
        import zstandard as zstd
    except ImportError:
        error("the 'zstandard' module is not installed.")
        print(f'Install it yourself for: "{best_python_exe()}"')
        return False

    print("Extracting to: " + output_path)

    output_dir = os.path.dirname(output_path) or "."

    try:
        fd, tmp_path = tempfile.mkstemp(
            dir=output_dir,
            prefix=".faceit-",
            suffix=".partial",
        )
    except OSError as exc:
        error(f"cannot create temporary file in {output_dir}: {exc}")
        return False

    try:
        os.close(fd)
    except OSError as exc:
        _cleanup(tmp_path)
        error(f"cannot close temporary file handle: {exc}")
        return False

    try:
        decompressor = zstd.ZstdDecompressor()

        with open(input_file, "rb") as ifh, open(tmp_path, "wb") as ofh:
            decompressor.copy_stream(ifh, ofh)
            ofh.flush()

            with contextlib.suppress(OSError):
                os.fsync(ofh.fileno())

        with open(tmp_path, "rb") as fh:
            head = fh.read(len(DEMO_MAGIC))

        if head != DEMO_MAGIC:
            error("the decompressed data is not a CS2 demo - extraction aborted.")
            print(f"  header check: expected {DEMO_MAGIC!r}, got {head!r}")
            print("  nothing was overwritten and the original archive was kept.")
            return False

        os.replace(tmp_path, output_path)

    except PermissionError:
        error("permission denied.")
        print("  - close CS2 if it is currently reading the demo file")
        print(f"  - check that {output_dir} is writable")
        return False

    except (zstd.ZstdError, ValueError) as exc:
        error(f"zstandard decompression failed ({exc}).")
        print("  the input file is probably not a valid .zst archive; nothing")
        print("  was overwritten and the original archive was kept.")
        return False

    except OSError as exc:
        error(str(exc))
        return False

    finally:
        _cleanup(tmp_path)

    print("OK - extraction successful.")

    if keep_zst:
        print("Original archive kept (keep_zst is enabled).")
        return True

    ok, err = delete_original(
        input_file,
        use_recycle_bin=move_to_recycle_bin,
    )

    if ok:
        if move_to_recycle_bin:
            print("Original archive moved to the Recycle Bin.")
        else:
            print("Original archive permanently deleted.")
    else:
        if move_to_recycle_bin:
            warn(
                f"could not move the original file to the Recycle Bin ({err}); "
                "original kept."
            )
        else:
            warn(f"could not delete the original file: {err}")

    return True


def run_extract_mode(files: list[str], *, pause_after: bool = True) -> int:
    """
    Direct extraction entry point for drag & drop / command line / worker.
    """
    print(APP_NAME)

    if not files:
        return 0

    if not zstandard_available():
        error("the 'zstandard' module is not installed in this Python interpreter.")
        print(f'Current interpreter: "{sys.executable}"')
        print(f'Recommended interpreter: "{best_python_exe()}"')

        if pause_after:
            pause()

        return 1

    csgo, source = find_cs2_csgo_path()

    if not csgo:
        error("could not locate the CS2 game\\csgo folder.")
        print("  " + str(source))
        print("Checked (under HKEY_LOCAL_MACHINE):")

        for subkey, value_name in CS2_REG_LOOKUP:
            print(f"  - {subkey} -> {value_name}")

        if pause_after:
            pause()

        return 1

    cfg = load_config()
    target = cfg["target_filename"]
    keep_zst = cfg["keep_zst"]
    move_to_recycle_bin = cfg["move_to_recycle_bin"]

    print(f"CS2 folder: {csgo}   [{source}]")

    if keep_zst:
        print("Original archives will be kept (keep_zst = true).")
    elif move_to_recycle_bin:
        print("Original archives will be moved to the Recycle Bin.")
    else:
        print("Original archives will be permanently deleted.")

    total = len(files)
    stem, ext = os.path.splitext(target)

    extracted: list[str] = []
    failed = 0

    for index, path in enumerate(files, start=1):
        if total == 1:
            out_name = target
            print("Processing: " + path)
        else:
            out_name = _next_free_name(csgo, stem, ext)
            print(f"[{index}/{total}] {path}")

        output_path = str(Path(csgo) / out_name)

        if extract_one(
            path,
            output_path,
            keep_zst=keep_zst,
            move_to_recycle_bin=move_to_recycle_bin,
        ):
            extracted.append(out_name)
        else:
            failed += 1

        print()

    plural = "" if total == 1 else "s"
    print(f"Result: {len(extracted)} of {total} archive{plural} extracted.")

    if extracted:
        stems = [os.path.splitext(name)[0] for name in extracted]

        if len(stems) == 1:
            print(f'In the CS2 console type: playdemo "{stems[0]}"')
        else:
            preview = ", ".join(f'"{s}"' for s in stems[:5])
            if len(stems) > 5:
                preview += ", ..."

            print("Extracted demos - in the CS2 console, e.g.: playdemo " + preview)

    if pause_after:
        pause()

    return 0 if failed == 0 else 1


# ---------------------------------------------------------------------------
# context-menu queue / worker
# ---------------------------------------------------------------------------

def _ensure_queue_dir() -> None:
    QUEUE_DIR.mkdir(parents=True, exist_ok=True)


def _pid_alive(pid: int) -> bool:
    if pid <= 0:
        return False

    try:
        handle = _kernel32.OpenProcess(
            _PROCESS_QUERY_LIMITED_INFORMATION,
            False,
            pid,
        )

        if not handle:
            return False

        _kernel32.CloseHandle(handle)
        return True
    except Exception:
        # If we cannot determine liveness, assume the worker may still exist.
        return True


def _read_lock_pid() -> int:
    try:
        return int(WORKER_LOCK.read_text(encoding="ascii").strip() or "0")
    except OSError:
        return 0
    except ValueError:
        return 0


def _queue_lock_is_stale() -> bool:
    try:
        st = WORKER_LOCK.stat()
    except OSError:
        return False

    age = time.time() - st.st_mtime

    # Hard cap for pathological cases.
    if age > 12 * 3600:
        return True

    pid = _read_lock_pid()

    if pid:
        return not _pid_alive(pid)

    # Lock exists but no PID yet: allow a short startup grace period.
    return age > 30


def _try_create_queue_lock() -> bool:
    try:
        fd = os.open(WORKER_LOCK, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        os.close(fd)
        return True
    except FileExistsError:
        if _queue_lock_is_stale():
            _cleanup(WORKER_LOCK)

            try:
                fd = os.open(WORKER_LOCK, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
                os.close(fd)
                return True
            except OSError:
                return False

        return False
    except OSError:
        return False


def enqueue_paths(paths: list[str]) -> int:
    """
    Queue one or more paths for the worker. Returns number queued.
    """
    _ensure_queue_dir()

    queued = 0

    for path in paths:
        path = path.strip().strip('"').strip()
        if not path:
            continue

        try:
            fd, _ = tempfile.mkstemp(
                dir=QUEUE_DIR,
                prefix="",
                suffix=".path",
            )

            with os.fdopen(fd, "w", encoding="utf-8") as fh:
                fh.write(os.path.abspath(path))

            queued += 1
        except OSError:
            continue

    return queued


def _spawn_queue_worker() -> bool:
    cmd = [best_python_exe(), SCRIPT_FILE, "--queue-worker"]

    try:
        proc = subprocess.Popen(
            cmd,
            creationflags=subprocess.CREATE_NEW_CONSOLE,
            close_fds=True,
        )
    except OSError:
        _cleanup(WORKER_LOCK)
        return False

    with contextlib.suppress(OSError):
        WORKER_LOCK.write_text(str(proc.pid), encoding="ascii")

    return True


def handle_context_menu(paths: list[str]) -> int:
    """
    Context-menu entry point.

    Explorer may invoke this once per selected file. We queue the file(s)
    and let one worker process handle the batch.
    """
    if not paths:
        error("no file provided for --context-menu")
        return 2

    try:
        queued = enqueue_paths(paths)
    except OSError as exc:
        error(f"could not queue file(s): {exc}")
        return 1

    if queued == 0:
        error("no valid file(s) were queued")
        return 2

    if _try_create_queue_lock():
        _spawn_queue_worker()

    return 0


def _take_queue_files() -> list[str]:
    if not QUEUE_DIR.is_dir():
        return []

    entries: list[tuple[float, Path]] = []

    for entry in QUEUE_DIR.glob("*.path"):
        try:
            mtime = entry.stat().st_mtime
        except OSError:
            mtime = 0.0

        entries.append((mtime, entry))

    entries.sort(key=lambda item: (item[0], str(item[1])))

    paths: list[str] = []

    for _, entry in entries:
        try:
            raw = entry.read_text(encoding="utf-8-sig").strip()
        except OSError:
            raw = ""

        _cleanup(entry)

        if raw:
            paths.append(raw)

    return paths


def collect_queued_files(
    *,
    idle: float = 0.7,
    initial_timeout: float = 5.0,
    max_total: float = 60.0,
) -> list[str]:
    """
    Collect queued files until the queue stays idle for `idle` seconds.
    """
    collected: list[str] = []

    start = time.monotonic()
    last = time.monotonic()

    while True:
        batch = _take_queue_files()

        if batch:
            collected.extend(batch)
            last = time.monotonic()
            time.sleep(0.05)
            continue

        now = time.monotonic()

        if collected and now - last >= idle:
            break

        if not collected and now - start >= initial_timeout:
            break

        if now - start >= max_total:
            break

        time.sleep(0.05)

    return collected


def run_queue_worker() -> int:
    """
    Internal worker started by the first context-menu invocation.
    """
    print(APP_NAME)

    _ensure_queue_dir()

    with contextlib.suppress(OSError):
        WORKER_LOCK.write_text(str(os.getpid()), encoding="ascii")

    any_failed = False
    processed_any = False

    while True:
        with contextlib.suppress(OSError):
            WORKER_LOCK.touch()

        files = collect_queued_files()

        if not files:
            # Retire. Remove lock first, then grab any extremely late files.
            _cleanup(WORKER_LOCK)

            late = _take_queue_files()
            if not late:
                break

            processed_any = True
            code = run_extract_mode(late, pause_after=False)

            if code != 0:
                any_failed = True

            break

        processed_any = True
        code = run_extract_mode(files, pause_after=False)

        if code != 0:
            any_failed = True

    if not processed_any:
        print("No queued files were found.")

    pause()
    return 1 if any_failed else 0


# ---------------------------------------------------------------------------
# interactive menu - cleaned-up user-facing version
# ---------------------------------------------------------------------------

def menu_status_lines() -> list[str]:
    """
    Compact header for the interactive menu.

    Shows only useful paths and critical warnings. Current settings are shown
    on the menu buttons themselves, not duplicated here.
    """
    csgo, _ = find_cs2_csgo_path()

    width = 66

    lines = [
        "=" * width,
        f"  {APP_NAME}",
        "=" * width,
        f"  CS2 folder : {csgo or 'not found'}",
        f"  Config     : {CONFIG_FILE}",
        f"  Script     : {SCRIPT_FILE}",
    ]

    # Only show zstandard when it is missing, because there is no menu button
    # for it anymore.
    if not zstandard_available():
        lines.append(
            f"  zstandard  : missing - install it for {best_python_exe()}"
        )

    lines.append("-" * width)

    return lines


def menu_install() -> None:
    print("\nInstalling right-click menu...")

    ok, err = _perform_install()

    if ok:
        print("Done. Right-click any .zst file to extract it.")
    else:
        error(err)


def menu_remove() -> None:
    if not context_menu_installed():
        print("\nRight-click menu is not installed.")
        return

    print("\nRemoving right-click menu...")

    ok, err = _perform_remove()

    if ok:
        print("Done.")
    else:
        error(err)


def menu_change_filename() -> None:
    current = load_config()["target_filename"]

    print(f"\nCurrent demo filename: {current}")

    try:
        new = input("New demo filename (blank to cancel): ").strip().strip('"').strip()
    except (EOFError, KeyboardInterrupt):
        return

    if not new:
        print("Cancelled.")
        return

    if not is_valid_filename(new):
        print("That filename cannot be used on Windows.")
        return

    ok, err = save_config({"target_filename": new})

    if not ok:
        error(err)
        return

    print(f"Saved. Demos will now be extracted as: {new}")

    if context_menu_command() is not None:
        try:
            answer = input("Update the right-click menu text too? [y/N]: ").strip().lower()
        except (EOFError, KeyboardInterrupt):
            answer = ""

        if answer in ("y", "yes"):
            ok, err = _perform_update_label()

            if ok:
                print("Right-click menu text updated.")
            else:
                error(err)


def menu_toggle_keep_zst() -> None:
    keep = not load_config()["keep_zst"]

    ok, err = save_config({"keep_zst": keep})

    if not ok:
        error(err)
        return

    if keep:
        print("Original .zst files will be kept.")
    else:
        print("Original .zst files will be removed after extraction.")


def menu_toggle_recycle_bin() -> None:
    recycle = not load_config()["move_to_recycle_bin"]

    ok, err = save_config({"move_to_recycle_bin": recycle})

    if not ok:
        error(err)
        return

    if recycle:
        print("Removed .zst files will go to the Recycle Bin.")
    else:
        print("Removed .zst files will be permanently deleted.")


def run_menu() -> int:
    while True:
        for line in menu_status_lines():
            print(line)

        cfg = load_config()

        if context_menu_command():
            menu_state = "installed"
        elif context_menu_installed():
            menu_state = "needs repair"
        else:
            menu_state = "not installed"

        if menu_state == "installed":
            option_1 = "Reinstall right-click menu - installed"
        elif menu_state == "needs repair":
            option_1 = "Repair right-click menu - needs repair"
        else:
            option_1 = "Install right-click menu - not installed"

        print(f"""
  1. {option_1}
  2. Remove right-click menu
  3. Demo filename: {cfg['target_filename']}
  4. Keep original .zst: {'on' if cfg['keep_zst'] else 'off'}
  5. Use Recycle Bin: {'on' if cfg['move_to_recycle_bin'] else 'off'}
  0. Exit
""")

        try:
            choice = input("Choose an option: ").strip().lower()
        except (EOFError, KeyboardInterrupt):
            print()
            break

        match choice:
            case "0" | "q" | "quit" | "exit":
                break
            case "1":
                menu_install()
            case "2":
                menu_remove()
            case "3":
                menu_change_filename()
            case "4":
                menu_toggle_keep_zst()
            case "5":
                menu_toggle_recycle_bin()
            case _:
                print(f"Unknown option: {choice}")

    return 0


# ---------------------------------------------------------------------------
# command line plumbing
# ---------------------------------------------------------------------------

SCRIPT_BASENAME = os.path.basename(SCRIPT_FILE)

USAGE = f"""{APP_NAME}

Usage:
  python {SCRIPT_BASENAME}                       interactive menu
  python {SCRIPT_BASENAME} <file.zst> [...]      extract file(s) directly
  python {SCRIPT_BASENAME} -- <file> [...]       same, for names starting with "-"
  python {SCRIPT_BASENAME} --context-menu <file> queue a context-menu invocation
  python {SCRIPT_BASENAME} --queue-worker        internal worker for queued files
  python {SCRIPT_BASENAME} --install             add per-user context-menu entry
  python {SCRIPT_BASENAME} --remove              remove per-user context-menu entry
  python {SCRIPT_BASENAME} --update-label        update context-menu label
  python {SCRIPT_BASENAME} --status              show current configuration
  python {SCRIPT_BASENAME} --help                show this help

Notes:
  * The context-menu entry is per-user and does not require UAC.
  * zstandard is NOT installed by this script. Install it yourself for the
    interpreter shown in --status.
  * Context-menu invocations are queued briefly so multiple Explorer
    invocations can be processed by one worker as a batch.
  * config.json options:
      target_filename
      keep_zst
      move_to_recycle_bin
"""

REGISTRY_FLAGS = ("--install", "--remove", "--update-label")
HELP_FLAGS = ("-h", "--help")


def status_lines() -> list[str]:
    """
    Full state overview for --status: paths, settings, and menu state.
    """
    csgo, source = find_cs2_csgo_path()
    cfg = load_config()

    width = 66

    lines = [
        "=" * width,
        f"  {APP_NAME}",
        "=" * width,
        f"  Script        : {SCRIPT_FILE}",
        f"  Config        : {CONFIG_FILE}",
        f"  CS2 folder    : {csgo or 'not found'}",
    ]

    if csgo:
        lines.append(f"  CS2 source    : {source}")

    lines += [
        f"  Interpreter   : {sys.executable}",
        f"  Worker python : {best_python_exe()}",
        f"  Menu pythonw  : {best_pythonw_exe()}",
        f"  zstandard     : {'installed' if zstandard_available() else 'MISSING'}",
        "-" * width,
        f"  target_filename     : {cfg['target_filename']}",
        f"  keep_zst            : {'on' if cfg['keep_zst'] else 'off'}",
        f"  move_to_recycle_bin : {'on' if cfg['move_to_recycle_bin'] else 'off'}",
        "-" * width,
    ]

    if context_menu_installed():
        command = context_menu_command()
        lines.append(f"  Context menu  : {'installed' if command else 'needs repair'}")

        label = context_menu_label()
        if label:
            lines.append(f"  Menu label    : {label}")

        if command:
            lines.append(f"  Menu command  : {command}")
    else:
        lines.append("  Context menu  : not installed")

    lines.append("=" * width)

    return lines


def main(argv: list[str]) -> int:
    _stream_reconfigure_errors()

    args = argv[1:]

    if args and args[0] in HELP_FLAGS:
        print(USAGE)
        return 0

    ok, config_error = ensure_config_file()
    if not ok:
        warn(f"could not create or update config.json: {config_error}")

    if not args:
        return run_menu()

    first = args[0]

    if first in REGISTRY_FLAGS:
        return run_registry_action(first)

    if first == "--status":
        print("\n".join(status_lines()))
        return 0

    if first == "--context-menu":
        return handle_context_menu(args[1:])

    if first == "--queue-worker":
        return run_queue_worker()

    if first == "--":
        rest = args[1:]

        if not rest:
            error("no input files after --")
            print(USAGE)
            return 2

        return run_extract_mode(rest)

    if first.startswith("-") and not os.path.isfile(first):
        error(f"unknown option: {first}")
        print(USAGE)
        return 2

    return run_extract_mode(args)


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv))
    except KeyboardInterrupt:
        print("\nCancelled.")
        sys.exit(130)
    except Exception as exc:
        error(f"unexpected internal error: {exc}")
        pause()
        sys.exit(1)
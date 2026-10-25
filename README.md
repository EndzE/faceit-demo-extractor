# FACEIT Demo Extractor

> Seamlessly unpack and verify FACEIT (`.dem.zst`) and Valve (`.dem.bz2`) match demos straight into your Counter-Strike 2 folder. Launch them directly from the in-game developer console with zero manual folder navigation.

[![Platform](https://img.shields.io/badge/Platform-Windows%2010%20%2F%2011%20x64-blue.svg)](#system-requirements)
[![Formats](https://img.shields.io/badge/Archives-.dem.zst%20%7C%20.dem.bz2-green.svg)](#features)
[![License](https://img.shields.io/badge/License-MIT-lightgrey.svg)](LICENSE)
[![Interface](https://img.shields.io/badge/Interface-Interactive%20TUI%20%2B%20Context%20Menu-blueviolet.svg)](#how-to-use)

---

> [!IMPORTANT]
> ### Keep Both Executables in the Same Folder
> **`faceit-dem.exe` and `faceit-dem-quiet.exe` must always reside side-by-side in the same directory.**
>
> The right-click context menu calls `faceit-dem-quiet.exe` as a lightweight dispatch helper that batches multiple Explorer selections and forwards them to `faceit-dem.exe` behind the scenes.

---

## Features

- **Multi-Format Extraction:** Unpacks FACEIT Zstandard archives (`.dem.zst`) and Valve bzip2 archives (`.dem.bz2`, from `replayNNN.valve.net` demo links) seamlessly with automatic format detection.
- **1-Click Context Menu:** Integrates directly into the Windows Explorer right-click menu for `.dem.zst` and `.dem.bz2` files without opening a terminal.
- **Native Toast Notifications:** Optional background mode (`notifications_instead_of_console`) runs right-click jobs hidden and alerts you with the exact in-game `playdemo` command upon completion.
- **Atomic Safety & Checksum Verification:** Pre-validates archive headers, verifies built-in XXH64 / CRC32 checksums during decompression, writes to `.tmp` files first, and safely preserves existing demos if an archive is corrupted.
- **Batch & Conflict Management:** Automatically queues simultaneous multi-file selections, finds gap numbers (`1_1.dem`, `1_2.dem`), supports custom delimiters (`_`, `-`, or empty `""`), and allows preserving original match filenames (`rename_demo: Off`).
- **Clean Archive Lifecycle:** Automatically purges processed archives to save disk space, with configurable options to keep originals or move them to the Windows Recycle Bin.

---

## System Requirements

| Requirement | Details |
|---|---|
| **Operating System** | Windows 10 / 11 (64-bit) |
| **Game** | Counter-Strike 2 installed via Steam |
| **Dependencies** | None (100% self-contained native Rust binaries) |
| **Permissions** | Standard user permissions (writes context menu association to `HKCU`) |
| **Location** | Both binaries in a permanent folder (e.g. `C:\Tools\FaceitDemoExtractor`) |

---

## Installation Guide

### Step 1: Download & Place Binaries
Download the latest `faceit-demo-extractor-vX.X.zip` from [Releases](https://github.com/EndzE/faceit-demo-extractor/releases) and extract both binaries into a permanent folder.

> [!TIP]
> Place the folder somewhere permanent before installing the context menu (for example, `C:\Tools\FaceitDemoExtractor`). If you ever move the folder later, simply run `faceit-dem.exe` and select **Repair right-click menu** (or run `faceit-dem --install`).

### Step 2: Configure & Register Context Menu
1. Double-click **`faceit-dem.exe`**.
2. Verify that your **CS2 folder** path was automatically detected.
3. Select **`1 - Install right-click menu`** to register the Explorer context menu.

---

## How to Use

### 1. From the Windows Right-Click Menu
1. Download or locate your demo archives (`.dem.zst` or `.dem.bz2`).
2. Right-click the archive (or select multiple archives and right-click).
3. Select **Extract to CS2 folder (1.dem)**.
4. When finished, a confirmation (or native Windows toast notification) will display your extracted demo filename.

### 2. By Drag and Drop
Drag and drop one or more `.dem.zst` or `.dem.bz2` files directly onto `faceit-dem.exe`. The app will batch-extract all files into your CS2 folder.

### 3. From the Command Line (CLI)
You can invoke `faceit-dem.exe` from any terminal or PowerShell session:

| Command | Purpose |
|---|---|
| `faceit-dem` | Launches the interactive TUI configuration menu |
| `faceit-dem <archive> [...]` | Extracts one or more `.dem.zst` or `.dem.bz2` archives |
| `faceit-dem -- <archive> [...]` | Safely handles archive filenames that start with a dash (`-`) |
| `faceit-dem --status` | Prints current configuration, CS2 path, and context menu status |
| `faceit-dem --test <archive>` | Verifies archive integrity and checksums without writing any demo files |
| `faceit-dem --install` | Installs or repairs the Windows Explorer right-click context menu |
| `faceit-dem --remove` | Unregisters and cleans up the right-click context menu |
| `faceit-dem --update-label` | Updates the right-click menu label to match current settings |
| `faceit-dem --help` | Displays command-line usage and available flags |

---

## Playing Demos in CS2

1. Launch **Counter-Strike 2**.
2. Open the developer console (`~` by default (Should be enabled in: Settings → Game → Enable Developer Console (~) → Yes)).
3. Type the `playdemo` command matching your demo stem:
   ```text
   playdemo 1
   ```
   *(If the demo was saved as `1_1.dem`, type `playdemo 1_1`. If names contain spaces, wrap in quotes: `playdemo "my demo"`).*

### Where Demos Go
Extracted demos are placed directly into your CS2 game directory:
```text
<SteamLibrary>\steamapps\common\Counter-Strike Global Offensive\game\csgo\
```
- **Single-file extractions:** Writes directly to `1.dem` by default (replacing any previous `1.dem` so `playdemo 1` always loads your latest match).
- **Batch extractions:** Automatically scans existing files and writes to sequential gap slots (`1_1.dem`, `1_2.dem`, etc.) to prevent overwriting existing demos.

---

## Settings & Configuration

### Interactive Menu Options
Run `faceit-dem.exe` without arguments to access the interactive configuration menu:

| Option | Setting | Description |
|:---:|---|---|
| **`1`** | **Install / Reinstall / Repair menu** | Registers or repairs Windows Explorer context menu entries in `HKCU`. |
| **`2`** | **Remove right-click menu** | Unregisters the right-click context menu completely. |
| **`3`** | **Rename demo** (`On` / `Off`) | When `Off`, preserves original archive filenames (e.g. `match.dem.zst` → `match.dem`). |
| **`4`** | **Change demo filename** | Sets the default single-extraction target filename (e.g. `1.dem`). |
| **`5`** | **Change delimiter** | Customizes the separator string before batch numbers (default: `_`). |
| **`6`** | **Keep original archives** (`On` / `Off`) | Keeps `.zst` / `.bz2` downloads instead of deleting them after successful extraction. |
| **`7`** | **Use the Recycle Bin** (`On` / `Off`) | Sends deleted archives to the Windows Recycle Bin instead of permanent deletion. |
| **`8`** | **Notifications instead of console** (`On` / `Off`) | Hides the console on right-click extraction and displays native Windows toast notifications. |
| **`0`** | **Exit** | Saves settings and exits. |

### Configuration File (`config.json`)
Settings can also be modified in `config.json` next to `faceit-dem.exe`:

```json
{
  "rename_demo": true,
  "target_filename": "1.dem",
  "delimiter": "_",
  "keep_zst": false,
  "move_to_recycle_bin": false,
  "overwrite_single_file_extraction": true,
  "notifications_instead_of_console": false,
  "csgo_path": null
}
```

| Key | Type | Default | Details |
|---|:---:|:---:|---|
| `rename_demo` | `bool` | `true` | `false` keeps original archive names without renaming to `1.dem`. |
| `target_filename` | `string` | `"1.dem"` | Target filename for renamed demos (must end with `.dem`). |
| `delimiter` | `string` | `"_"` | Separator string before numbers for batches or duplicates. Can be empty (`""`) to produce `11.dem`. |
| `keep_zst` | `bool` | `false` | `true` keeps original `.dem.zst` and `.dem.bz2` archives after extraction. |
| `move_to_recycle_bin` | `bool` | `false` | `true` sends deleted archives to the Recycle Bin instead of permanent deletion. |
| `overwrite_single_file_extraction` | `bool` | `true` | `true` overwrites `1.dem` on single-file extraction; `false` finds the next free numbered name (`1_1.dem`).*(Advanced setting: edit via config.json only)* |
| `notifications_instead_of_console` | `bool` | `false` | `true` runs background extractions silently and reports via Windows toast notifications. |
| `csgo_path` | `string?` | `null` | Custom absolute or relative path to your CS2 `game\csgo` folder. |

---

## Troubleshooting

### CS2 Folder Not Detected
Start Counter-Strike 2 at least once so Steam and the game initialize the installation directories and Windows registry entries. If using a non-standard Steam library or custom install folder, open `config.json` next to `faceit-dem.exe` and specify your `csgo_path`:
```json
{
  "csgo_path": "D:\\SteamLibrary\\steamapps\\common\\Counter-Strike Global Offensive\\game\\csgo"
}
```
*An absolute path is recommended. A relative path resolves relative to the directory containing `faceit-dem.exe`.*

### Right-Click Option is Missing
Open `faceit-dem.exe` and choose **Repair right-click menu** (or run `faceit-dem --install`). Verify that `faceit-dem-quiet.exe` is still located in the exact same folder as `faceit-dem.exe`.

### Damaged or Truncated Archives
If the extractor reports that an archive is invalid or has a mismatched checksum, the download was likely interrupted or corrupted. The extractor checks magic headers and verifies checksums during streaming decompression, refusing to write incomplete or corrupted demos. Re-download the match demo from FACEIT or Valve.

### Preserving Existing Demos on Single Extraction
By default, extracting a single demo intentionally overwrites `1.dem` so that you can always type `playdemo 1` in the CS2 console. If you prefer keeping every match, set `"overwrite_single_file_extraction": false` in `config.json`. The app will allocate sequential numbered slots (`1_1.dem`, `1_2.dem`, etc.) automatically.

---

## Removal & Cleanup

1. Double-click `faceit-dem.exe` and select **`2 - Remove right-click menu`** (or run `faceit-dem --remove` from the terminal).
2. Delete the application folder containing `faceit-dem.exe` and `faceit-dem-quiet.exe`.

---

## License

Distributed under the MIT License. See [`LICENSE`](LICENSE) for details.

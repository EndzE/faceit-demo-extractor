# MOVE.md - Porting FACEIT Demo Extractor to Rust (rev 2)

## 0. What changed vs rev 1

| # | Change | Why |
|---|---|---|
| 1 | Quiet exe probes the worker lock and spawns a worker only if it is free | rev 1 spawned a console per selected file |
| 2 | Worker retirement re-scans the queue after releasing the lock | closes a lost-file window (also present in Python) |
| 3 | PID lives in `worker.pid`, not inside the locked `worker.lock` | `LockFileEx` is mandatory on Windows; `--status` could not read it |
| 4 | Ctrl+C sets a flag checked by a hand-written copy loop | `process::exit` skips `Drop`, leaving a `.partial` in the CS2 folder |
| 5 | `std::path::absolute`, never `canonicalize` | `\\?\` paths break `SHFileOperationW` |
| 6 | Queue files are written as `.tmp` then renamed to `.path` | worker could read a half-written file and drop it (Python bug) |
| 7 | Magic header is checked on the first decoded bytes, before writing the rest | wrong data never touches the CS2 folder |
| 8 | New integrity section (zstd frame checksum, truncation, size check) | see section 6.4 |
| 9 | Module layout instead of the 4-file rule | the port is ~1500+ lines; the old rule contradicted its own 500-line split |
| 10 | Crate versions resolved with `cargo add`; `ruzstd`/`thin`/MSI noise removed | versions in rev 1 were unverified |
| 11 | `sync_all` errors ignored | parity with Python's `suppress(OSError)` |
| 12 | Spike milestone first | the quiet exe + worker handoff is the riskiest part |

## 1. Goal

Replace `faceit_demo_extractor.py` and its `.venv` with a self-contained Windows
executable pair that keeps the same behavior and external contracts.

- No Python, pip, venv, or `zstandard` install requirement.
- Statically linked zstd decoding.
- Fast startup, two small exes, no installer needed.
- Same CLI flags, config schema, registry keys, queue directory, and exit codes.

Non-goals: cross-platform support, GUI, auto-update, Steam library scanning
beyond the existing registry keys, changing the `.zst` file association,
changing the `config.json` schema.

Windows-only. Guard with `#[cfg(not(windows))] compile_error!(...)`.

## 2. Behavior contract

Everything below must survive the port unchanged unless marked **(changed)**.

| Contract | Value |
|---|---|
| Config file | `config.json` next to the executable |
| Config keys | `target_filename`, `keep_zst`, `move_to_recycle_bin` |
| Demo magic | `b"PBDEMS2\x00"` |
| Single output | `target_filename` (default `1.dem`) |
| Batch output | `1_1.dem`, `1_2.dem`, ... first free slots |
| Context menu key | `HKCU\Software\Classes\SystemFileAssociations\.zst\shell\FaceitExtract` |
| Queue directory | `%TEMP%\faceit_demo_extractor.queue` |
| Queue files | `*.path`, absolute path as UTF-8 text |
| Worker lock **(changed)** | `worker.lock` is held with an OS file lock while a worker lives; PID text goes to `worker.pid` for `--status` |
| CS2 lookup | `HKLM\SOFTWARE\WOW6432Node\Valve\cs2` -> `installpath`; then both `Steam App 730` `InstallLocation` keys |
| Exit codes | `0` success, `1` failure, `2` usage, `130` Ctrl+C |
| Deletion order | Magic check + replace first, original archive last |
| Atomicity | temp file in output dir, then replace; failed run never corrupts a demo |

CLI surface:

```text
faceit-dem.exe                            interactive menu
faceit-dem.exe <file.zst> [...]           extract files
faceit-dem.exe -- <file> [...]            files starting with "-"
faceit-dem.exe --context-menu <file>      internal: queue for the worker
faceit-dem.exe --queue-worker             internal: batch worker
faceit-dem.exe --install                  add per-user context-menu entry
faceit-dem.exe --remove                   remove per-user context-menu entry
faceit-dem.exe --update-label             refresh context-menu label
faceit-dem.exe --status                   print configuration/state
faceit-dem.exe --help
```

The registry command must point at the quiet binary:

```text
"C:\...\faceit-dem-quiet.exe" --context-menu "%1"
```

`--install` must verify `faceit-dem-quiet.exe` exists next to the running exe
and fail with a clear message if it does not.

## 3. Crates

Do not hand-pin versions. Resolve them with `cargo add` and commit the lockfile.

```text
cargo add anyhow ctrlc tempfile unicode-general-category zstd winreg
cargo add serde_json --features preserve_order
cargo add windows --features Win32_Foundation,Win32_Security,Win32_System_Environment,Win32_System_Threading,Win32_UI_Shell,Win32_UI_WindowsAndMessaging
cargo add --dev assert_cmd predicates
cargo add --build embed-resource
```

| Need | Pick | Notes |
|---|---|---|
| zstd streaming decode | `zstd` | bundled libzstd, fastest, verifies frame checksums |
| JSON config | `serde_json` + `preserve_order` | keeps unknown keys and order; strip BOM manually |
| Registry | `winreg` | near 1:1 with Python `winreg`; `KEY_WOW64_64KEY`, `delete_subkey_all` |
| Win32 FFI | `windows` | `CreateProcessW`, `MessageBoxW`, `ExpandEnvironmentStringsW`, `SHFileOperationW` |
| Recycle Bin | direct `SHFileOperationW` | exact flag parity, no COM init; `trash` crate may show nuke-warning dialogs |
| Temp/atomic files | `tempfile` | `NamedTempFile` + `persist` (MoveFileEx replace), RAII cleanup |
| Errors | `anyhow` | app-level context only |
| Unicode categories | `unicode-general-category` | reject `Cf` in filenames |
| Ctrl+C | `ctrlc` | flag-based, see 6.10 |
| Worker lock | std `File::try_lock` | needs Rust 1.89+; OS releases it on crash |
| CLI parsing | hand-rolled | exact messages and exit codes |
| Manifest/icon | `embed-resource` | icon, version info, `asInvoker`, `longPathAware` |
| Tests | `assert_cmd`, `predicates` | CLI integration |

`Cargo.toml` skeleton (dependencies come from `cargo add`):

```toml
[package]
name = "faceit-demo-extractor"
version = "2.0.0"
edition = "2024"
rust-version = "1.89"

[lib]
name = "faceit_demo_extractor"
path = "src/lib.rs"

[[bin]]
name = "faceit-dem"
path = "src/main.rs"

[[bin]]
name = "faceit-dem-quiet"
path = "src/quiet.rs"

[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
strip = "symbols"
```

Keep `panic = "unwind"` so a top-level `catch_unwind` can print the unexpected
error message and exit 1, and so `Drop` cleanup runs.

## 4. Repository layout

```text
FaceitDemoExtractor/
  Cargo.toml
  build.rs                  # icon/manifest/version resource
  src/
    lib.rs                  # module decls, constants, Paths, run_console(), run_quiet()
    output.rs               # warn/error/pause, multiline prefixing
    validate.rs             # filename validation
    config.rs               # load/save/ensure
    cs2.rs                  # CS2 install discovery
    registry.rs             # context-menu install/remove/repair/label
    extract.rs              # delete, zstd extract, batch naming, run_extract_mode
    queue.rs                # enqueue, collect, lock, spawn, worker loop
    cli.rs                  # commands, menu, help/status, argv dispatch
    win32.rs                # the only unsafe/FFI surface
    main.rs                 # console binary: lib::run_console()
    quiet.rs                # windows-subsystem binary: queue + spawn worker
  tests/
    parity.rs
  assets/
    app.manifest
    app.ico
  legacy/
    faceit_demo_extractor.py  # kept only until cutover
  MOVE.md
```

`src/quiet.rs` starts with `#![windows_subsystem = "windows"]`.

Module to Python source map:

| Module | Contents | Python lines |
|---|---|---|
| `lib.rs` | constants, `Paths` | 115-162 |
| `output.rs` | output helpers | 212-269 |
| `validate.rs` | validation | 276-331 |
| `config.rs` | config | 341-482 |
| `cs2.rs` | CS2 discovery | 550-600 |
| `registry.rs` | context menu | 607-765 |
| `extract.rs` | delete + extract + batch | 772-1049 |
| `queue.rs` | queue + worker | 1053-1322 |
| `cli.rs` | commands, menu, CLI | 744-765, 1329-1658 |

Deleted outright: `zstandard_available`, `_as_python_exe`, `best_python_exe`,
`best_pythonw_exe`, `importlib`, `ctypes`, `_pid_alive`, stale-lock heuristics,
and the zstd-missing warning.

## 5. Design decisions and pitfalls

### 5.1 Two binaries replace pythonw.exe / python.exe

`faceit-dem-quiet.exe` handles only `--context-menu`, then starts
`faceit-dem.exe --queue-worker` with `CREATE_NEW_CONSOLE` (only if needed, see 5.2).

Do not use `std::process::Command` from the quiet bin. A windows-subsystem
process has invalid std handles, and Rust sets `STARTF_USESTDHANDLES`, which
produces a console window with no output (rust-lang/rust#101645). Call
`CreateProcessW` with a zeroed `STARTUPINFOW` and `CREATE_NEW_CONSOLE`.

The quiet bin has no console: queue failures use `MessageBoxW`, never `pause()`.

### 5.2 Worker lock and handoff protocol

Roles:

- **Quiet exe** (per selected file): enqueue first, then *probe* the lock
  (`try_lock`, and if it succeeded, unlock immediately). Lock free -> spawn a
  worker. Lock held -> do nothing and exit 0.
- **Worker**: `try_lock` on `worker.lock`. `WouldBlock` -> another worker is
  alive, exit 0 silently. Success -> write PID to `worker.pid`, hold the handle
  for its lifetime.
- **Retirement**: when the queue is idle, drop the lock, then re-scan the queue.
  If `.path` files exist, `try_lock` again; success -> keep working, failure ->
  another worker took over, exit. Otherwise delete `worker.pid` and exit.

Why no file is lost: the quiet exe enqueues *before* probing. If it probes and
sees the lock held, the worker has not released it yet, so the worker's
post-release re-scan will see the file. If it sees the lock free, it spawns its
own worker. A rare double spawn is harmless: the loser exits at once (one brief
console flash, only in a true race).

`--status`: probe the lock for "worker running"; read `worker.pid` for the PID.
Never read `worker.lock` itself (the whole-file lock blocks readers on Windows).

The OS releases the lock on process death, so the 12-hour cap, PID-liveness
check, and 30-second grace are gone. No `OpenProcess` FFI.

### 5.3 Queue

Keep timing: 50 ms poll, 0.7 s idle, 5 s initial wait, 60 s cap. Plain
`std::thread::sleep`, no async runtime.

- Enqueue: strip quotes/whitespace as Python does, make the path absolute with
  `std::path::absolute` (no filesystem access, no `\\?\`), write
  `<unique>.tmp`, then rename to `<unique>.path`. The worker only globs `*.path`,
  so it never sees a half-written entry.
- Sort entries by `(mtime, filename)` as before.
- Store UTF-8; use `args_os`/`PathBuf` upstream and `to_string_lossy` for the
  queue text. Read with BOM tolerance (`utf-8-sig` equivalent).

No global extraction mutex in this port. Direct CLI runs can already race in
the Python version; future hardening: reserve final names with `create_new`.

### 5.4 Atomic extraction with integrity checks

Steps for `extract_one`:

1. Reject a missing input.
2. Read the first 5 bytes of the archive: confirm the zstd magic `28 B5 2F FD`
   and note bit 2 (`0x04`) of the next byte (Content_Checksum_flag) for the
   no-checksum warning below. Not a zstd file -> same "not a valid .zst" error.
3. `tempfile::Builder`: prefix `.faceit-`, suffix `.partial`, in the output dir.
4. Wrap input in `BufReader`, feed `zstd::stream::read::Decoder` (multi-frame by
   default), output through a `BufWriter` (about 1 MiB).
5. Hand-written copy loop with a 1 MiB buffer:
   - after the first read that yields at least 8 bytes, compare with
     `PBDEMS2\0`; mismatch -> abort immediately with the existing message
     (`nothing was overwritten and the original archive was kept`);
   - check the cancel flag each iteration (5.10);
   - count bytes written.
6. A decoder error here covers truncation and checksum mismatch (see below).
7. `flush`, `sync_all` (ignore errors, as Python does).
8. Compare the temp file length with the byte counter; mismatch -> fail.
9. `persist` to the final path (`MoveFileEx` replace).
10. Only then keep/delete/recycle the original.

A failed run leaves cleanup to `NamedTempFile`'s `Drop`. A locked target
(CS2 reading it) becomes `PermissionDenied`; keep the specific hints from the
Python messages.

**What "not corrupted" can and cannot mean**

- zstd frames may carry an optional content checksum (XXH64, low 32 bits stored).
  libzstd verifies it automatically at the end of each frame, and a mismatch
  surfaces as a decoder error (`Restored data doesn't match checksum`). The
  `zstd` CLI writes it by default, but not every producer does.
- Truncated archives are caught even without a checksum: the stream ends inside
  a frame and the decoder reports an incomplete frame. Verify this with the
  truncation test; if the crate returns a clean EOF instead, add an explicit
  "frame finished" check.
- Without a checksum, a flipped byte inside a raw block cannot be detected by
  anyone. There is no external hash to compare against (FACEIT does not publish
  one), so the demo itself cannot be validated beyond the magic header.
- Confirmed: real FACEIT archives carry an XXH64 checksum (`zstd -lv` shows
  `XXH64` in the `Check` column). Corruption and truncation therefore fail the
  extraction in practice.
- The checksum is only verified at the end of each frame, so a corrupt archive
  is fully decoded into the `.partial` before it fails. That is safe (the
  replace happens only after the decoder returns success, and `Drop` removes
  the temp file) but means a bad archive takes the full decode time to be
  rejected.
- The first-bytes peek from step 2 is kept for one purpose only: if an archive
  ever arrives *without* the checksum flag, print a single warning line,
  `warning: archive has no checksum; only truncation and the demo header were
  verified`. Normal archives print nothing extra.

### 5.5 Filename validation parity

- Length in UTF-16 code units: `s.encode_utf16().count() <= 255`.
- Reject `<>:"|?*`, `/`, `\`, names `.` and `..`, trailing dot or space.
- Reject controls with `code < 32 || code == 127`. Do not use `char::is_control()`;
  it also rejects C1 (`U+0080..U+009F`), which Python allows.
- Reject general category `Cf`. (`Cs` cannot occur in a Rust `str`.)
- Stem is `name.split('.').next()` trimmed; it must be non-empty and not in
  `CON, PRN, AUX, NUL, COM1-9, LPT1-9` after Unicode uppercase.
- `_next_free_name` trims the stem while `stem + ext + 6 > 255` UTF-16 units,
  scans `1..10000`, then falls back to the Unix timestamp.

### 5.6 Config parity

- Read bytes, strip an optional `EF BB BF` BOM, parse into
  `serde_json::Map<String, Value>`.
- Invalid/missing/unreadable JSON -> defaults. `ensure_config_file` resets a
  non-object or invalid file to defaults and appends only missing keys.
- Unknown keys are preserved on save; `preserve_order` keeps file order.
- Invalid `target_filename` warns once and falls back to `1.dem`.
- Booleans are strict; non-bool warns once and uses the default.
- Save atomically: temp file in the exe directory, UTF-8 without BOM,
  `sync_all`, persist over `config.json`.
- No LocalAppData fallback.

### 5.7 CS2 discovery parity

`winreg` with `KEY_READ | KEY_WOW64_64KEY`. For `REG_EXPAND_SZ`, call
`ExpandEnvironmentStringsW` (`winreg` exposes the type but does not expand).
Strip surrounding quotes, trailing NUL, whitespace; reject empty values.
Append `game\csgo` and require `is_dir()`.

### 5.8 Registry parity

- Values are the default (unnamed) `REG_SZ` of `FaceitExtract` and `command`.
- Install creates both keys; remove uses `delete_subkey_all`.
- Repair when the parent key exists but the command value is missing.
- Changing `target_filename` prompts to update the label.
- No elevation; HKCU only.

### 5.9 Deletion parity

Permanent: `remove_file`, treating `NotFound` as success.

Recycle Bin: `SHFileOperationW` with
`FOF_SILENT | FOF_NOCONFIRMATION | FOF_ALLOWUNDO | FOF_NOERRORUI`, a
double-NUL-terminated `pFrom`, and the existing fallback: non-zero result but
the file no longer exists -> success. Pass an absolute path from
`std::path::absolute` (not `canonicalize`; `SHFileOperationW` rejects `\\?\` and
does not support long paths, so on failure the original is kept, as today).
Lives in `win32.rs` behind a safe wrapper.

### 5.10 Console, CLI, Ctrl+C

- `args_os` for file paths; flags compared via `to_str()`.
- First arg `--help`/`-h` prints usage and exits before config creation.
- Unknown `-...` that is not an existing file -> usage error, exit 2.
- `--` treats everything after it as files.
- Keep current help/status/menu text, replacing interpreter/venv/zstandard
  lines with executable path and `zstd: bundled`; add worker state to `--status`.
- Rust writes to the console with `WriteConsoleW`, so Unicode output needs no
  reconfiguration (Python needed `_stream_reconfigure_errors`).
- **Ctrl+C**: two atomics, `CANCEL` and `EXTRACTING`. The `ctrlc` handler sets
  `CANCEL`; if `EXTRACTING` is false (menu, prompts, queue wait) it prints
  `Cancelled.` and exits 130 immediately. During extraction the copy loop sees
  `CANCEL`, returns, `Drop` removes the partial file, then the process prints
  `Cancelled.` and exits 130. The original archive is never touched.
- `pause()` only when stdin is an interactive terminal; context-menu paths never pause.
- Top-level `catch_unwind` prints `ERROR: unexpected internal error: ...`,
  pauses if interactive, exits 1.

### 5.11 Paths and testing

```rust
struct Paths {
    exe: PathBuf,
    exe_dir: PathBuf,
    config: PathBuf,
    queue_dir: PathBuf,
}
```

`Paths::for_tests(tempdir)` lets config/queue tests avoid the real `%TEMP%` and
exe directory. No guid/uuid needed; `tempfile` generates unique names.

## 6. Milestones

| Milestone | Work | Verification |
|---|---|---|
| M0. Spike | scaffold crate and manifest; quiet exe that enqueues a path, probes the lock, spawns a console worker that prints and exits | trigger from Explorer with 1 and 10 files: exactly one console, no flashes |
| M1. Baseline | `git init`, commit Python + config, move Python to `legacy/` | `cargo build` makes both exes |
| M2. Foundations | `lib.rs`, `output`, `validate`, `config` | unit tests for validation/config |
| M3. Extraction | delete + extract: early magic check, copy loop, cancel flag, checksum/truncation | fixture tests incl. corrupted and truncated archives |
| M4. Windows integration | `cs2`, `registry` | manual registry inspection + `--status` |
| M5. Queue | full protocol from 5.2/5.3 | multi-select, kill-worker, retirement-race tests |
| M6. UX parity | `cli`: commands, menu, help/status | side-by-side output diff |
| M7. Hardening | integration tests, clippy, fmt, filename table | `cargo test`, `cargo clippy -- -D warnings` |
| M8. Cutover | package, install, verify Explorer, delete `legacy/` and `.venv` | acceptance matrix |
| M9. Post-cutover | additional features

## 7. Test plan

Unit tests (`tests/parity.rs`, temp dirs):

- Filename table: valid names, paths, reserved names, trailing dot/space,
  control chars, C1 chars (allowed), `Cf` chars, 255/256 UTF-16 units, empty stem.
- Config: BOM, invalid JSON, non-object, unknown-key preservation, missing-key
  migration, invalid filename/boolean fallback + one warning, atomic save.
- `_next_free_name`: gaps, 4-digit overflow, long stem trimming, timestamp fallback.
- Extraction:
  - valid `PBDEMS2\0` fixture, multi-frame archive;
  - wrong magic: aborts early, target untouched, original kept;
  - **truncated archive** (cut mid-frame): fails, nothing replaced;
  - **checksummed archive with one flipped byte** (build with the encoder's
    checksum option, corrupt after): fails, nothing replaced, original kept;
  - archive without checksum: succeeds and prints the one-line warning;
    checksummed archives print no warning;
  - trailing garbage after the last frame: compare Rust vs Python behavior;
  - not-a-zstd file; empty file;
  - existing target untouched on every failure; no `.partial` left behind;
  - cancel flag set mid-copy: partial removed, original kept;
  - keep/delete/recycle decisions.
- Queue: enqueue ordering, `.tmp` entries ignored, collect timing bounds,
  lock contention, probe-vs-held behavior, retirement race (file arrives after
  last scan), stale `.path` files.

Integration tests (`assert_cmd`):

- `--help`, `--status`, unknown option, `--` handling, missing file.
- Exit codes 0/1/2.
- `--install`/`--remove` against a test-only HKCU subkey with cleanup (ignored by default).
- Recycle Bin test with a temp file (ignored by default).

Manual matrix before cutover:

- One archive -> `1.dem`; multi-select -> `1_1.dem`, `1_2.dem`.
- Existing demo overwritten only after a valid magic check.
- Invalid `.zst`, wrong magic, locked target leave the original intact.
- Explorer multi-select of 10 files opens exactly one console, one batch.
- Kill the worker mid-run; the next invocation recovers.
- Ctrl+C mid-extraction: `Cancelled.`, exit 130, no `.partial` in the csgo folder.
- Unicode and long paths.
- Install, repair, remove, label update; no UAC prompt.
- Keep/delete/recycle modes.
- Run on a machine without Python installed.

Optional parity harness: run `legacy/python/faceit_demo_extractor.py --status`
and `faceit-dem --status`, normalize paths, diff. Never run `--install` from the
Python version during testing; it overwrites the same key.

## 8. Build and packaging

- `cargo build --release --target x86_64-pc-windows-msvc`.
- Ship `faceit-dem.exe`, `faceit-dem-quiet.exe`, optional default `config.json`,
  and a third-party notice for zstd (BSD-3-Clause), as a zip.
- Embed icon, version info, `longPathAware`, `asInvoker` via `build.rs`.
- The context menu is installed per-user at runtime by `--install`.
- Code signing is optional but recommended; an unsigned exe that writes into the
  CS2 folder may trigger SmartScreen/AV.

## 9. Migration / cutover

1. Back up the Python script and `config.json`.
2. Move the Python script to `legacy/`; leave the active `config.json` alone.
3. Build both release binaries into the app folder.
4. `faceit-dem.exe --status`; confirm CS2 path, config, menu state.
5. `faceit-dem.exe --install`; this rewrites the HKCU command to the Rust quiet
   exe and updates the label.
6. Verify Explorer with one and with several `.zst` files.
7. Restart Explorer if the old verb is cached.
8. Delete `legacy/` and `.venv`; update any README/shortcut that invoked `python`.
9. Old queue files remain compatible (`*.path`). A leftover Python-era
   `worker.lock` is harmless: the Rust probe takes an OS lock, which an
   unlocked leftover file does not block.

## 10. Risks

| Risk | Mitigation |
|---|---|
| Windows-subsystem parent spawns a blank console | `CreateProcessW` with zeroed `STARTUPINFOW`; test in M0 |
| Console flash per selected file | probe-then-spawn protocol (5.2); M0 test with 10 files |
| Lost queue entry at worker retirement | release, re-scan, re-lock loop (5.2) |
| `--status` cannot read a locked file | PID in separate `worker.pid` |
| Partial file left after Ctrl+C or crash | flag-based cancel + `Drop`; optional startup sweep of old `.faceit-*.partial` |
| Archive without checksum can hide corruption | FACEIT archives carry XXH64; warn only if the flag is absent; truncation + size + magic checks still apply |
| Recycle Bin behavior differs | direct `SHFileOperationW`, exact flags, absolute non-`\\?\` path |
| Registry tests pollute HKCU | test-only subkey + cleanup; real key tests are manual |
| `config.json` next to exe is read-only (Program Files) | document portable/user-writable install; keep no-fallback behavior |
| Non-UTF-8 paths | `args_os`/`OsString`; only queue text is lossy |
| Concurrent direct runs overwrite | existing behavior; future `create_new` reservation |
| Antivirus/SmartScreen | sign the binary; local-only behavior |
| Explorer caches the old menu | restart Explorer after `--install` |

## 11. Definition of done

- `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` pass.
- Both release binaries build on `x86_64-pc-windows-msvc`.
- No Python, pip, venv, or `zstandard` module needed at runtime.
- Acceptance matrix passes on a clean Windows account.
- `--install` points at `faceit-dem-quiet.exe`; `--remove` leaves no keys.
- Invalid, truncated, or checksum-failing archives never overwrite an existing
  demo, never leave a `.partial`, and never delete the original.
- Ctrl+C during extraction leaves the CS2 folder clean.
- 10-file Explorer multi-select opens exactly one worker console.



## 12. Post cut-over
 - Progress line and disk-space check. Demos are hundreds of MB, so decoding takes a few seconds with no feedback. The zstd frame header usually stores the decompressed size. That gives you a percent/MB display (only when the console is interactive) and a free-space check on the CS2 drive before writing anything. It’s cheap and fits the copy loop already planned.
 - --test <file.zst>. It decodes to nowhere and reports whether the archive passes its XXH64 checksum. You can check a download without touching the CS2 folder or deleting anything. It reuses the extraction code.
 - Optional csgo_path override. If the registry lookup fails (non-Steam install, moved library), you currently get an error and no way forward. An optional config key that takes priority over discovery fixes that. Unknown keys are already preserved, so old configs keep working. It does touch the “don’t change the config schema” non-goal, so it’s your call.
 - Failure list in the batch summary. After a multi-file run, print which files failed instead of only the count. It’s a few lines.
# FACEIT Demo Extractor

Get your FACEIT demos into CS2 without digging through folders.

FACEIT matches arrive as compressed `.dem.zst` archives, and demos downloaded directly from Valve's demo servers (a `replayNNN.valve.net/730/...` demo link) arrive as compressed `.dem.bz2` archives. This app unpacks both, checks that the download is intact, and places the finished `.dem` file in your CS2 `game\csgo` folder so you can play it right away.

## What you need

- Windows 10 or 11
- Counter-Strike 2 installed
- Both app files kept in the same folder:
  - `faceit-dem.exe` - the app you open
  - `faceit-dem-quiet.exe` - a small background helper used by the right-click menu

## First-time setup

1. Put the app folder somewhere permanent, such as `C:\Tools\FACEIT Demo Extractor`.
2. Double-click `faceit-dem.exe`.
3. Check that the app shows the correct **CS2 folder**. If it does, you are ready to go.
4. Optional but recommended: choose **1 - Install right-click menu**.

The right-click menu lets you extract demos directly from Explorer without opening the app first.

## Extract a demo

### From the right-click menu

1. Save or download the demo archive - `.dem.zst` from FACEIT, or `.dem.bz2` from a `replayNNN.valve.net` demo link.
2. Right-click the archive in Explorer.
3. Choose **Extract to CS2 folder (1.dem)**.
4. Wait for the confirmation message.

### By dragging and dropping

You can also drag one or more archives (`.dem.zst` or `.dem.bz2`) onto `faceit-dem.exe`. The app extracts them in one batch.

## Play the demo in CS2

1. Open CS2.
2. Open the developer console.
3. Type `playdemo 1` and press Enter.

The command matches the demo filename without `.dem`. If the app saved the demo as `1_1.dem`, use `playdemo 1_1`.

## Where demos go

Demos are saved in your CS2 installation under:

```text
Counter-Strike Global Offensive\game\csgo
```

By default, extracting a single demo writes directly to `1.dem` (replacing any previous demo with that name so `playdemo 1` always loads your latest match). When extracting multiple archives in a batch, existing files are preserved and assigned the next free slots (`1_1.dem`, `1_2.dem`, etc.).

## Original archives

By default, the original archive is permanently deleted after a successful extraction.

If you want to keep your downloads, open `faceit-dem.exe` and turn on **Keep original archives**. You can also turn on **Use the Recycle Bin** if you want deleted archives to be recoverable.

A failed extraction never removes the archive or replaces an existing demo.

## Settings

Open `faceit-dem.exe` to change interactive options:

- **Change demo filename** - choose the name used for extracted demos, such as `1.dem`
- **Keep original archives** - keep `.zst` and `.bz2` archives after extraction
- **Use the Recycle Bin** - move removed archives to the Recycle Bin

Additional options can be configured in `config.json`:

- **overwrite_single_file_extraction** - `true` (default) to overwrite `1.dem` on single-file extraction, or `false` to preserve existing files and pick the next free name (`1_1.dem`, etc.)
- **csgo_path** - custom path to your CS2 `game\csgo` folder

## Troubleshooting

**The app cannot find my CS2 folder**

Start CS2 once so the game creates its folders. If your game is installed in a custom location, open `config.json` next to `faceit-dem.exe` and set `csgo_path` to your CS2 `game\csgo` folder. An absolute path is recommended:

```json
{
  "csgo_path": "D:\\SteamLibrary\\steamapps\\common\\Counter-Strike Global Offensive\\game\\csgo"
}
```

A relative `csgo_path` is resolved from the folder containing `faceit-dem.exe`.

**The right-click option is missing**

Open `faceit-dem.exe` and choose **Install right-click menu** again. Make sure `faceit-dem-quiet.exe` is still in the same folder.

**The app says the archive is damaged**

Download the demo from FACEIT again. The app checks the archive before writing anything, so a damaged download is rejected safely.

**A demo with that name already exists**

Extracting a single demo overwrites `1.dem` by default so `playdemo 1` always works without checking file numbers. If you want single-file extractions to pick the next available name instead, set `"overwrite_single_file_extraction": false` in `config.json`. Batch extractions automatically find free names (`1_1.dem`, `1_2.dem`, etc.).

## Removing the app

Open `faceit-dem.exe` and choose **Remove right-click menu**, then delete the app folder.

## Safety

- Archives are checked for corruption and verified against their checksums when present.
- Demos are written to a temporary file first.
- Existing demos are only replaced after the new file passes validation.
- A failed extraction leaves your archive and existing demos untouched.

## License

MIT. See [LICENSE](LICENSE).

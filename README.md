# FACEIT Demo Extractor

Get your FACEIT demos into CS2 without digging through folders.

FACEIT downloads arrive as compressed `.dem.zst` archives. This app unpacks the demo, checks that the download is intact, and places the finished `.dem` file in your CS2 `game\csgo` folder so you can play it right away.

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

1. Save or download your FACEIT `.dem.zst` archive.
2. Right-click the archive in Explorer.
3. Choose **Extract to CS2 folder (1.dem)**.
4. Wait for the confirmation message.

### By dragging and dropping

You can also drag one or more `.dem.zst` archives onto `faceit-dem.exe`. The app extracts them in one batch.

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

If a demo with the chosen name already exists, the app automatically picks the next free name, such as `1_1.dem`.

## Original archives

By default, the original `.zst` archive is permanently deleted after a successful extraction.

If you want to keep your downloads, open `faceit-dem.exe` and turn on **Keep original archives**. You can also turn on **Use the Recycle Bin** if you want deleted archives to be recoverable.

A failed extraction never removes the archive or replaces an existing demo.

## Settings

Open `faceit-dem.exe` to change these options:

- **Change demo filename** - choose the name used for extracted demos, such as `1.dem`
- **Keep original archives** - keep `.zst` files after extraction
- **Use the Recycle Bin** - move removed archives to the Recycle Bin

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

The app picks the next available name automatically. You can also change the base filename in the app settings.

## Removing the app

Open `faceit-dem.exe` and choose **Remove right-click menu**, then delete the app folder.

## Safety

- Archives are checked for corruption and verified against their checksums when present.
- Demos are written to a temporary file first.
- Existing demos are only replaced after the new file passes validation.
- A failed extraction leaves your archive and existing demos untouched.

## License

MIT. See [LICENSE](LICENSE).

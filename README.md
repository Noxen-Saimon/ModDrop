# ModDrop

A simple Windows utility for downloading Steam Workshop items and putting them directly into your game's mod or vehicle directory without needing the Steam client open.

Built with Rust and Slint. Runs as a single portable program with no installation needed.

---

## Why this exists

Downloading mods or vehicles outside the Steam client is usually a hassle:
- You have to run SteamCMD through the terminal manually.
- You have to hunt down obscure download paths like `steamapps/workshop/content/<appid>/<modid>/`.
- Downloaded items often come with internal Steam files (`.vdf` files, manifests) that games do not need.
- For games like Stormworks, vehicle files often stay named `vehicle.xml` inside numbered folders instead of having their actual vehicle name directly inside your `data/vehicles` folder.

ModDrop handles this in a few seconds: paste the link, choose where you want your files once, and hit download.

---

## What it does

- **Clipboard detection:** If you copy a Steam Workshop link in your browser and open ModDrop, the link is detected and loaded automatically. You can also click the Paste button at any time.
- **Direct file placement:** Drops the files directly into your destination directory instead of wrapping them in extra subfolders. For vehicles, it names the `.xml` and `.png` files using the workshop item's actual title so the game picks them up immediately.
- **Remembers your folders per game:** You only have to select your game folder once. ModDrop remembers where each game's mods go (e.g., your Stormworks vehicle folder vs. your RimWorld mods folder) and switches automatically based on the game ID.
- **Cleans out Steam junk:** Strips `.vdf`, `.acf`, and other Steam metadata files so only clean mod assets end up on your drive.
- **Portable SteamCMD backend:** If SteamCMD is not already on your system PATH, ModDrop downloads a portable copy into `%LOCALAPPDATA%\ModDrop\bin` and runs it headlessly. It uses anonymous login, so you never need to enter your Steam account or password.
- **Open Folder button:** Once the download finishes, click Open Folder to view the files directly in Windows Explorer.
- **Live progress and speed:** Shows actual transfer velocity (MB/s) and download progress in real time.
- **Safe extraction:** Archive extraction guards against Zip-Slip path traversal to prevent files from ever extracting outside your target folder.

---

## How to use it

1. Copy any Steam Workshop link from your browser.
2. Open ModDrop (or click Paste if it is already open). The tool extracts the mod ID and detects the game automatically.
3. Click `...` to select your game's mod or vehicle folder (done once per game; ModDrop remembers it next time).
4. Click **Download & Deploy**.
5. Once complete, click **Open Folder** to view the deployed files.

---

## Building from source

If you prefer to compile it yourself:

### Requirements
- Windows 10 or 11 (64-bit)
- Rust 1.85 or newer (2024 edition)

### Build steps

```bash
# Clone the repository
git clone https://github.com/your-username/moddrop.git
cd moddrop

# Run the test suite
cargo test

# Compile release binary
cargo build --release
```

The compiled executable will be located at:
```text
target/release/moddrop.exe
```

---

## Configuration and files

ModDrop keeps its working files in standard user application directories and does not write to the Windows Registry:

- Saved folder preferences: `%LOCALAPPDATA%\ModDrop\config.json`
- Portable SteamCMD runtime: `%LOCALAPPDATA%\ModDrop\bin\`
- Staging directory: automatically removed after deployment completes.

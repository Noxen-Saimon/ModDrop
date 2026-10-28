# ModDrop | Native Steam Workshop Utility

<p align="center">
  <img src="assets/icon.png" alt="ModDrop Logo" width="96" height="96" />
</p>

<p align="center">
  <strong>A lightweight, blazing-fast native Windows desktop application for downloading Steam Workshop items and deploying them directly to your local game directories—without opening the Steam client.</strong>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Platform-Windows-0078D6?logo=windows&logoColor=white" alt="Platform: Windows" />
  <img src="https://img.shields.io/badge/Language-Rust%202024-dea584?logo=rust&logoColor=white" alt="Language: Rust" />
  <img src="https://img.shields.io/badge/UI-Slint%20Native-27ae60" alt="UI: Slint" />
  <img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT" />
</p>

---

## 💡 Overview

**ModDrop** solves a common problem for gamers and modders: getting mods and vehicles from the Steam Workshop into your game quickly and cleanly without wrestling with the heavy Steam desktop client, manual cache hunting, or nested folder clutter.

Built in **Rust** and **Slint UI**, ModDrop runs as a single, portable executable with sub-millisecond UI responsiveness, minimal memory usage, and zero background CPU or network consumption when idle.

---

## ✨ Features

- **🚀 Instant Clipboard Auto-Paste:**
  Copy a Steam Workshop link in your browser and open ModDrop—the link is automatically detected, pasted, and parsed on startup. A dedicated **Paste** button is also available for instant one-click input.

- **🧠 Smart Destination Path Memory & Per-Game Routing:**
  Never browse for the same mod folder twice. ModDrop remembers your destination directories across launches and automatically switches to the correct game directory when an AppID is detected (e.g., switches to your *RimWorld* mods directory or *Stormworks* vehicles directory automatically).

- **📂 Direct Individual File Deployment:**
  Downloads and organizes files cleanly without unnecessary wrapping subfolders. For vehicle mods (like *Stormworks*), it places the `.xml` vehicle file and `.png` preview directly into your vehicles folder with clean, automatically generated names matching the workshop title.

- **🧹 Automatic Steam Metadata Stripping:**
  Automatically removes Valve internal files (`.vdf`, `steam_autocloud.vdf`, `.acf`, `.manifest`) so your local game directory stays 100% clean and free of junk files.

- **🗂️ One-Click "Open Folder" Action:**
  View your installed files in Windows File Explorer (`explorer.exe`) immediately after deployment with a single click.

- **⚡ Portable Headless SteamCMD Backend:**
  Zero configuration required. If SteamCMD is not found in your system `PATH`, ModDrop automatically bootstraps a private, headless portable runtime in `%LOCALAPPDATA%\ModDrop\bin`.

- **🛡️ Zip-Slip Safe Extraction Engine:**
  Hardened archive extractor that blocks directory traversal attacks, ensuring files can never be extracted outside their intended destination.

- **📊 Real-Time Velocity & Job Telemetry:**
  Displays active download velocity (MB/s), a smooth progress bar, and visual job state badges (`READY`, `DOWNLOADING`, `DEPLOYING`, `COMPLETED`).

- **📜 Bounded Event & Telemetry Console:**
  Includes a 500-line bounded terminal log with category color tags (`INFO`, `OK`, `ERR`, `SCMD`, `PATH`) and dynamic highlight animations for the newest entry.

---

## 🖥️ User Interface Layout

ModDrop is built around an ergonomic, distraction-free 3-input workflow:

```text
┌────────────────────────────────────────────────────────────────────────┐
│  ModDrop  [WORKSHOP IO]                       [ ● READY    0.00 MB/s ] │
├────────────────────────────────────────────────────────────────────────┤
│  ID Game       [ Steam AppID or URL (e.g., 573090) ]   [ Stormworks  ] │
│  Link Mod      [ Steam Workshop URL or ID          ]   [ID: 12345] [Paste]│
│  Location Path [ C:\Games\Stormworks\data\vehicles ]   [ ... ]         │
│                                                                        │
│  [       DOWNLOAD & DEPLOY       ]   [ Open Folder ]   [ Clear Log ]   │
│  ════════════════════════════ Progress Bar ═══════════════════════════ │
├────────────────────────────────────────────────────────────────────────┤
│  EVENT & TELEMETRY CONSOLE                                 1 / 500 lines│
│  [15:30:12] [SYS]   ModDrop initialized. Ready for deployment.        │
│  [15:30:14] [CLIP]  Auto-loaded Workshop URL from clipboard.           │
│  [15:30:15] [OK]    Successfully deployed pure mod files.              │
└────────────────────────────────────────────────────────────────────────┘
```

---

## 🚀 How to Use

1. **Copy Mod Link:**
   Find any mod or vehicle on the Steam Workshop and copy its URL (e.g., `https://steamcommunity.com/sharedfiles/filedetails/?id=2876543210`).

2. **Open ModDrop:**
   ModDrop will automatically read the link from your clipboard and resolve the mod title and parent game. If ModDrop is already open, simply click **Paste**.

3. **Select Destination Folder (First Time Only):**
   Click **`...`** to pick your game's mod or vehicle directory. ModDrop automatically saves this path for future downloads for that game.

4. **Click "Download & Deploy":**
   ModDrop fetches the workshop package, removes Steam metadata, and places the clean asset files into your folder.

5. **Click "Open Folder":**
   Instantly open the destination directory to verify your files or jump straight into your game!

---

## 🛠️ Technology Stack

| Layer | Technology | Details |
| :--- | :--- | :--- |
| **Language** | [Rust](https://www.rust-lang.org/) (2024 Edition) | High-performance, memory-safe systems programming |
| **GUI Framework** | [Slint](https://slint.dev/) (v1.18+) | Native, GPU-accelerated lightweight desktop toolkit |
| **Async Runtime** | [Tokio](https://tokio.rs/) | Multi-threaded asynchronous I/O and process spawning |
| **Networking** | [Reqwest](https://docs.rs/reqwest) | Fast HTTP client with streaming download pipes |
| **Dialogs** | [RFD](https://docs.rs/rfd) | Native OS folder picker dialogs |
| **Archive Formats**| [Zip](https://docs.rs/zip) / [Tar](https://docs.rs/tar) / [Flate2](https://docs.rs/flate2) | Path-traversal protected archive extraction |

---

## ⚙️ Building from Source

### Prerequisites
- **Rust Toolchain:** Version `1.85+` (with Cargo)
- **Windows OS:** Windows 10 / 11 (x64)

### Build Commands

```bash
# Clone the repository
git clone https://github.com/your-username/moddrop.git
cd moddrop

# Run unit tests (all 11 tests)
cargo test

# Build debug binary
cargo build

# Build optimized release binary
cargo build --release
```

The compiled release executable will be available at:
```text
target/release/moddrop.exe
```

---

## 🔒 Configuration & Data Storage

ModDrop stores minimal configuration files strictly inside standard application data directories:

- **Path Memory & Config:** `%LOCALAPPDATA%\ModDrop\config.json`
- **Portable SteamCMD Binaries:** `%LOCALAPPDATA%\ModDrop\bin\`
- **Staging / Temporary Extraction:** `%LOCALAPPDATA%\Temp\moddrop_*` (automatically cleaned after deployment)

ModDrop does **not** modify system registries, does **not** install background services, and does **not** require administrator privileges.

---

## 📄 License

This project is licensed under the [MIT License](LICENSE).

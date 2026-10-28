#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod steamcmd;

use chrono::Local;
use futures_util::StreamExt;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

slint::include_modules!();

const MAX_LOG_LINES: usize = 500;
const STEAM_GET_DETAILS_URL: &str =
    "https://api.steampowered.com/ISteamRemoteStorage/GetPublishedFileDetails/v1/";
const STEAM_APP_DETAILS_URL: &str = "https://store.steampowered.com/api/appdetails";

static RE_PUBLISHED_FILE_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:[?&]id=|filedetails/\?id=|workshop/filedetails/\?id=|^)(\d{5,20})\b")
        .expect("Invalid PublishedFileId regex")
});

static RE_APP_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:[?&]appid=|/app/|/workshop/|^\s*)(\d{2,10})\b").expect("Invalid AppID regex")
});

#[derive(Debug, Clone, Copy)]
pub enum LogKind {
    Info = 0,
    Success = 1,
    Error = 2,
    Telemetry = 3,
}

#[derive(Debug, Deserialize)]
struct SteamResponseRoot {
    response: SteamPublishedFileResponse,
}

#[derive(Debug, Deserialize)]
struct SteamPublishedFileResponse {
    #[serde(default)]
    publishedfiledetails: Vec<PublishedFileDetail>,
}

#[derive(Debug, Deserialize, Clone)]
struct PublishedFileDetail {
    #[serde(default)]
    result: i32,
    #[serde(default)]
    publishedfileid: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    consumer_app_id: u64,
    #[serde(default)]
    creator_app_id: u64,
    #[serde(default)]
    file_url: String,
    #[serde(default)]
    filename: String,
    #[serde(default)]
    file_size: serde_json::Value,
    #[serde(default)]
    hcontent_file: String,
}

impl PublishedFileDetail {
    fn parsed_file_size(&self) -> Option<u64> {
        match &self.file_size {
            serde_json::Value::Number(n) => n.as_u64(),
            serde_json::Value::String(s) => s.parse::<u64>().ok(),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub last_destination: String,
    #[serde(default)]
    pub game_destinations: HashMap<String, String>,
}

impl AppConfig {
    pub fn config_path() -> Option<PathBuf> {
        #[cfg(target_os = "windows")]
        {
            if let Ok(appdata) = std::env::var("LOCALAPPDATA") {
                return Some(PathBuf::from(appdata).join("ModDrop").join("config.json"));
            }
            if let Ok(userprofile) = std::env::var("USERPROFILE") {
                return Some(
                    PathBuf::from(userprofile)
                        .join("AppData")
                        .join("Local")
                        .join("ModDrop")
                        .join("config.json"),
                );
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            if let Ok(home) = std::env::var("HOME") {
                return Some(
                    PathBuf::from(home)
                        .join(".config")
                        .join("moddrop")
                        .join("config.json"),
                );
            }
        }
        None
    }

    pub fn load() -> Self {
        if let Some(path) = Self::config_path() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(cfg) = serde_json::from_str::<AppConfig>(&content) {
                    return cfg;
                }
            }
        }
        AppConfig::default()
    }

    pub fn save(&self) {
        if let Some(path) = Self::config_path() {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string_pretty(self) {
                let _ = fs::write(path, json);
            }
        }
    }

    pub fn update_path(&mut self, game_id: &str, dest_path: &str) {
        let trimmed_path = dest_path.trim().to_string();
        if trimmed_path.is_empty() {
            return;
        }
        self.last_destination = trimmed_path.clone();
        let gid = game_id.trim();
        if !gid.is_empty() && gid != "0" {
            self.game_destinations.insert(gid.to_string(), trimmed_path);
        }
        self.save();
    }

    pub fn get_path_for_game(&self, game_id: &str) -> Option<String> {
        let gid = game_id.trim();
        if !gid.is_empty() && gid != "0" {
            if let Some(p) = self.game_destinations.get(gid) {
                if !p.is_empty() {
                    return Some(p.clone());
                }
            }
        }
        if !self.last_destination.is_empty() {
            return Some(self.last_destination.clone());
        }
        None
    }
}

/// Reads plain unicode text safely from the OS clipboard.
pub fn get_clipboard_text() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        use std::ffi::c_void;
        use std::ptr;

        const CF_UNICODETEXT: u32 = 13;

        unsafe extern "system" {
            fn OpenClipboard(hwnd: *mut c_void) -> i32;
            fn CloseClipboard() -> i32;
            fn GetClipboardData(u_format: u32) -> *mut c_void;
            fn GlobalLock(h_mem: *mut c_void) -> *mut c_void;
            fn GlobalUnlock(h_mem: *mut c_void) -> i32;
        }

        unsafe {
            if OpenClipboard(ptr::null_mut()) == 0 {
                return None;
            }
            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                CloseClipboard();
                return None;
            }
            let ptr = GlobalLock(handle) as *const u16;
            if ptr.is_null() {
                CloseClipboard();
                return None;
            }
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            let result = String::from_utf16(slice).ok();
            GlobalUnlock(handle);
            CloseClipboard();
            result.map(|s| s.trim().to_string())
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// Parses a Steam Workshop URL or raw numeric string into `(Option<PublishedFileId>, Option<AppId>)`.
fn parse_workshop_input(input: &str) -> (Option<String>, Option<String>) {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return (None, None);
    }

    let file_id = RE_PUBLISHED_FILE_ID
        .captures(trimmed)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string());

    let app_id = if trimmed.contains("appid=") || trimmed.contains("/app/") {
        RE_APP_ID
            .captures(trimmed)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
    } else {
        None
    };

    (file_id, app_id)
}

/// Parses an explicit Game ID input or extracts it if the user pasted a Steam store/app URL.
fn parse_game_id_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Some(trimmed.to_string());
    }
    RE_APP_ID
        .captures(trimmed)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Prevents catastrophic writes to filesystem roots or critical OS directories.
fn validate_destination_path(dest: &Path) -> Result<PathBuf, String> {
    if dest.as_os_str().is_empty() {
        return Err("Destination path cannot be empty.".to_string());
    }

    // Ensure directory exists or can be created
    if !dest.exists() {
        fs::create_dir_all(dest)
            .map_err(|e| format!("Failed to create destination folder '{}': {}", dest.display(), e))?;
    }

    let canonical = dest
        .canonicalize()
        .map_err(|e| format!("Unable to resolve destination path '{}': {}", dest.display(), e))?;

    if !canonical.is_dir() {
        return Err(format!("Destination '{}' is not a directory.", canonical.display()));
    }

    // Check component depth to avoid root paths like C:\ or \
    let normal_components: Vec<_> = canonical
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .collect();

    if normal_components.is_empty() {
        return Err(
            "Safety guard triggered: Refusing to deploy directly into drive root directory."
                .to_string(),
        );
    }

    // Block critical Windows system directories
    let lower_path = canonical.to_string_lossy().to_lowercase();
    let forbidden_fragments = [
        "\\windows\\system32",
        "\\windows\\syswow64",
        "c:\\windows",
        "\\program files\\windowsapps",
    ];
    for forbidden in forbidden_fragments {
        if lower_path.ends_with(forbidden) || lower_path.contains(&format!("{}\\", forbidden)) {
            return Err(format!(
                "Safety guard triggered: Refusing to write into protected system path '{}'.",
                canonical.display()
            ));
        }
    }

    Ok(canonical)
}

/// Sanitizes a filename so it cannot escape the target directory.
fn sanitize_filename(name: &str, fallback_id: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        format!("workshop_mod_{}.bin", fallback_id)
    } else {
        trimmed.to_string()
    }
}

/// Appends a log line onto the UI thread while enforcing the 500-line bounded capacity.
/// The newest entry is automatically marked `is_latest: true` with dynamic visual highlighting.
pub fn push_ui_log(
    ui_weak: &Weak<AppWindow>,
    level: &str,
    message: impl Into<String>,
    kind: LogKind,
) {
    let msg = message.into();
    let lvl = level.to_string();
    let ts = Local::now().format("%H:%M:%S").to_string();
    let kind_int = kind as i32;

    let _ = slint::invoke_from_event_loop({
        let ui_weak = ui_weak.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                let model = ui.get_logs();
                let mut entries: Vec<LogEntry> = model.iter().collect();

                // Unset is_latest from previous entries
                for entry in &mut entries {
                    entry.is_latest = false;
                }

                entries.push(LogEntry {
                    timestamp: SharedString::from(ts),
                    level: SharedString::from(lvl),
                    message: SharedString::from(msg),
                    kind: kind_int,
                    is_latest: true,
                });
                if entries.len() > MAX_LOG_LINES {
                    let overflow = entries.len() - MAX_LOG_LINES;
                    entries.drain(0..overflow);
                }
                ui.set_logs(ModelRc::from(Rc::new(VecModel::from(entries))));
            }
        }
    });
}

pub fn reset_ui_for_new_job(ui_weak: &Weak<AppWindow>, initial_msg: Option<&str>) {
    let ts = Local::now().format("%H:%M:%S").to_string();
    let msg = initial_msg
        .unwrap_or("Starting new workshop download & deployment job...")
        .to_string();
    let _ = slint::invoke_from_event_loop({
        let ui_weak = ui_weak.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                let reset = vec![LogEntry {
                    timestamp: SharedString::from(ts),
                    level: SharedString::from("INIT"),
                    message: SharedString::from(msg),
                    kind: LogKind::Info as i32,
                    is_latest: true,
                }];
                ui.set_logs(ModelRc::from(Rc::new(VecModel::from(reset))));
                ui.set_progress(0.0);
                ui.set_status_badge(SharedString::from("STARTING"));
                ui.set_velocity_text(SharedString::from("0.00 MB/s"));
            }
        }
    });
}

pub fn update_ui_telemetry(
    ui_weak: &Weak<AppWindow>,
    status: Option<&str>,
    velocity: Option<&str>,
    progress: Option<f32>,
    busy: Option<bool>,
) {
    let status_opt = status.map(|s| s.to_string());
    let vel_opt = velocity.map(|s| s.to_string());

    let _ = slint::invoke_from_event_loop({
        let ui_weak = ui_weak.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                if let Some(s) = status_opt {
                    ui.set_status_badge(SharedString::from(s));
                }
                if let Some(v) = vel_opt {
                    ui.set_velocity_text(SharedString::from(v));
                }
                if let Some(p) = progress {
                    ui.set_progress(p);
                }
                if let Some(b) = busy {
                    ui.set_is_busy(b);
                }
            }
        }
    });
}

/// Measures connection latency to Steam edge servers via TCP handshake (443) or GetServerInfo API.
pub async fn measure_steam_connection_latency(client: &Client) -> Option<u128> {
    // 1. Probe TCP handshake directly to Steam edge server (pure network RTT)
    let start_tcp = Instant::now();
    let tcp_future = tokio::net::TcpStream::connect("api.steampowered.com:443");
    if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(3000), tcp_future).await {
        return Some(start_tcp.elapsed().as_millis());
    }

    // 2. Fallback to lightweight HTTP GetServerInfo endpoint
    let start_http = Instant::now();
    let res = client
        .get("https://api.steampowered.com/ISteamWebAPIUtil/GetServerInfo/v1/")
        .timeout(Duration::from_millis(4000))
        .send()
        .await;

    if let Ok(resp) = res
        && resp.status().is_success()
    {
        return Some(start_http.elapsed().as_millis());
    }

    None
}

/// Queries the official Steam Web API (`ISteamRemoteStorage/GetPublishedFileDetails/v1/`)
/// for Workshop item metadata with automated retry on transient connection drops or timeouts.
async fn fetch_workshop_details(
    client: &Client,
    published_file_id: &str,
) -> Result<PublishedFileDetail, String> {
    let params = [
        ("itemcount", "1"),
        ("publishedfileids[0]", published_file_id),
    ];

    let mut last_error = String::new();
    for attempt in 1..=3 {
        match client.post(STEAM_GET_DETAILS_URL).form(&params).send().await {
            Ok(response) => {
                if !response.status().is_success() {
                    return Err(format!(
                        "Steam Web API returned HTTP status {}",
                        response.status()
                    ));
                }

                let root: SteamResponseRoot = response.json().await.map_err(|e| {
                    format!("Failed to parse Steam Web API JSON payload: {}", e)
                })?;

                let detail = root
                    .response
                    .publishedfiledetails
                    .into_iter()
                    .next()
                    .ok_or_else(|| "Steam Web API returned empty publishedfiledetails.".to_string())?;

                if detail.result != 1 {
                    return Err(format!(
                        "Steam Workshop item '{}' not found or private (result code {}).",
                        published_file_id, detail.result
                    ));
                }

                return Ok(detail);
            }
            Err(e) => {
                last_error = format!("Network request to Steam Web API failed: {}", e);
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
                }
            }
        }
    }

    Err(last_error)
}

/// Queries the official Steam Store API (`appdetails?appids=<appid>`) to verify a Game ID / AppID
/// and return its official title and type, with automated retry for transient connection drops.
async fn fetch_app_details(client: &Client, app_id: &str) -> Result<(String, String), String> {
    let url = format!("{}?appids={}", STEAM_APP_DETAILS_URL, app_id);
    let mut last_error = String::new();

    for attempt in 1..=3 {
        match client.get(&url).send().await {
            Ok(response) => {
                if !response.status().is_success() {
                    return Err(format!(
                        "Steam Store API returned HTTP status {}",
                        response.status()
                    ));
                }

                let root: serde_json::Value = response.json().await.map_err(|e| {
                    format!("Failed to parse Steam Store API JSON payload: {}", e)
                })?;

                let app_entry = root
                    .get(app_id)
                    .ok_or_else(|| format!("No entry returned for AppID '{}'.", app_id))?;

                let success = app_entry
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if !success {
                    return Err(format!(
                        "Steam AppID '{}' was not found or is not a valid public Steam application.",
                        app_id
                    ));
                }

                let data = app_entry
                    .get("data")
                    .ok_or_else(|| format!("Missing 'data' object for AppID '{}'.", app_id))?;

                let name = data
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown Steam App")
                    .to_string();

                let app_type = data
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("app")
                    .to_string();

                return Ok((name, app_type));
            }
            Err(e) => {
                last_error = format!("Network request to Steam Store API failed: {}", e);
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
                }
            }
        }
    }

    Err(last_error)
}

/// Checks if a file has a standard ZIP magic header (`PK\x03\x04`).
fn is_zip_archive(path: &Path) -> bool {
    use std::io::Read;
    if let Ok(mut f) = File::open(path) {
        let mut magic = [0u8; 4];
        if f.read_exact(&mut magic).is_ok() {
            return magic == [b'P', b'K', 0x03, 0x04];
        }
    }
    false
}

/// Safely extracts a ZIP archive into `target_dir` using `enclosed_name()` to prevent Zip-Slip traversal.
fn extract_zip_safely(archive_path: &Path, target_dir: &Path) -> Result<usize, String> {
    let file = File::open(archive_path)
        .map_err(|e| format!("Failed to open downloaded archive for extraction: {}", e))?;

    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("Invalid ZIP archive structure: {}", e))?;

    let mut extracted_files = 0usize;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Failed reading ZIP entry #{}: {}", i, e))?;

        let enclosed = match entry.enclosed_name() {
            Some(path) => path.to_path_buf(),
            None => {
                let raw_name = entry.name().unwrap_or_else(|_| "<invalid-utf8>".into());
                return Err(format!(
                    "Security guard triggered: Blocked suspicious relative path in archive entry '{}'",
                    raw_name
                ));
            }
        };

        if let Some(fname) = enclosed.file_name().and_then(|s| s.to_str())
            && steamcmd::is_steam_metadata_file(fname)
        {
            continue;
        }

        let out_path = target_dir.join(enclosed);

        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .map_err(|e| format!("Failed creating directory '{}': {}", out_path.display(), e))?;
        } else {
            if let Some(parent) = out_path.parent()
                && !parent.exists()
            {
                fs::create_dir_all(parent).map_err(|e| {
                    format!("Failed creating parent directory '{}': {}", parent.display(), e)
                })?;
            }
            let mut outfile = File::create(&out_path)
                .map_err(|e| format!("Failed creating file '{}': {}", out_path.display(), e))?;
            io::copy(&mut entry, &mut outfile)
                .map_err(|e| format!("Failed extracting file '{}': {}", out_path.display(), e))?;
            outfile
                .flush()
                .map_err(|e| format!("Failed flushing file '{}': {}", out_path.display(), e))?;
            extracted_files += 1;
        }
    }

    Ok(extracted_files)
}

/// Recursively traverses an extracted folder and renames mod IDs and standard vehicle/preview files to `custom_name`.
fn post_process_extracted_folder(folder: &Path, mod_id: &str, custom_name: &str) {
    let has_vehicle_xml = folder.join("vehicle.xml").is_file();
    if let Ok(entries) = fs::read_dir(folder) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                let renamed = if has_vehicle_xml && name.eq_ignore_ascii_case("vehicle.xml") {
                    Some(format!("{}.xml", custom_name))
                } else if has_vehicle_xml
                    && (name.eq_ignore_ascii_case("workshop_preview.png")
                        || name.eq_ignore_ascii_case("preview.png"))
                {
                    Some(format!("{}.png", custom_name))
                } else if has_vehicle_xml && name.contains(mod_id) {
                    Some(name.replace(mod_id, custom_name))
                } else {
                    None
                };

                if let Some(new_name) = renamed {
                    let new_path = folder.join(new_name);
                    let _ = fs::rename(&path, &new_path);
                }
            } else if path.is_dir() {
                post_process_extracted_folder(&path, mod_id, custom_name);
            }
        }
    }
}

/// Sanitizes a user-specified custom folder / mod name, removing filesystem-illegal characters.
pub fn sanitize_custom_folder_name(input: &str, fallback_id: &str) -> String {
    let forbidden = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let cleaned: String = input
        .chars()
        .map(|c| if forbidden.contains(&c) || c.is_control() { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        format!("mod_{}", fallback_id)
    } else {
        trimmed.to_string()
    }
}

/// Executes the end-to-end asynchronous Workshop metadata resolution, SteamCMD / CDN acquisition,
/// metadata stripping, and clean asset deployment into a dedicated custom folder.
async fn execute_download_and_deploy(
    ui_weak: Weak<AppWindow>,
    client: Client,
    app_config: Arc<Mutex<AppConfig>>,
    raw_game_id: String,
    raw_mod_link: String,
    raw_dest_path: String,
) {
    // 1. Parse Mod Link / PublishedFileId
    let (file_id_opt, link_app_id_opt) = parse_workshop_input(&raw_mod_link);
    let published_file_id = match file_id_opt {
        Some(id) => id,
        None => {
            push_ui_log(
                &ui_weak,
                "ERR",
                "Invalid Mod Link or PublishedFileId. Provide a valid Steam Workshop URL (e.g. ?id=123456789) or numeric ID.",
                LogKind::Error,
            );
            update_ui_telemetry(&ui_weak, Some("ERROR"), Some("0.00 MB/s"), Some(0.0), Some(false));
            return;
        }
    };

    // Automatically clear log console and reset telemetry for the new mod deployment job
    reset_ui_for_new_job(
        &ui_weak,
        Some(&format!(
            "Starting new deployment job for Workshop Item #{}...",
            published_file_id
        )),
    );
    update_ui_telemetry(&ui_weak, Some("RESOLVING"), Some("0.00 MB/s"), Some(0.05), Some(true));

    // 2. Validate Destination Path & Write Permissions safely
    let dest_path = match validate_destination_path(Path::new(raw_dest_path.trim())) {
        Ok(p) => p,
        Err(e) => {
            push_ui_log(&ui_weak, "ERR", e, LogKind::Error);
            update_ui_telemetry(&ui_weak, Some("ERROR"), Some("0.00 MB/s"), Some(0.0), Some(false));
            return;
        }
    };

    if let Err(perm_err) = steamcmd::verify_directory_writable(&dest_path) {
        push_ui_log(&ui_weak, "ERR", perm_err, LogKind::Error);
        update_ui_telemetry(&ui_weak, Some("ERROR"), Some("0.00 MB/s"), Some(0.0), Some(false));
        return;
    }

    push_ui_log(
        &ui_weak,
        "INFO",
        format!(
            "Querying Steam RemoteStorage API for PublishedFileId [{}]...",
            published_file_id
        ),
        LogKind::Info,
    );

    // 3. Query Steam Web API (non-fatal if item metadata is restricted; falls back to user AppID + SteamCMD)
    let detail_opt = match fetch_workshop_details(&client, &published_file_id).await {
        Ok(d) => Some(d),
        Err(e) => {
            push_ui_log(
                &ui_weak,
                "WARN",
                format!(
                    "{} — Proceeding directly with autonomous SteamCMD runner.",
                    e
                ),
                LogKind::Info,
            );
            None
        }
    };

    let user_app_id = parse_game_id_input(&raw_game_id).or(link_app_id_opt);

    let resolved_app_id = if let Some(ref detail) = detail_opt {
        if let Some(ref explicit) = user_app_id {
            explicit.clone()
        } else if detail.consumer_app_id > 0 {
            detail.consumer_app_id.to_string()
        } else if detail.creator_app_id > 0 {
            detail.creator_app_id.to_string()
        } else {
            "0".to_string()
        }
    } else {
        user_app_id.unwrap_or_else(|| "0".to_string())
    };

    let title_display = detail_opt
        .as_ref()
        .filter(|d| !d.title.is_empty())
        .map(|d| d.title.clone())
        .unwrap_or_else(|| format!("Workshop Item #{}", published_file_id));

    // Determine clean custom folder and asset name automatically
    let clean_custom_name = sanitize_custom_folder_name(&title_display, &published_file_id);

    // Auto-populate Game ID, Resolved File ID, and Mod Title on the UI if empty or updated
    let _ = slint::invoke_from_event_loop({
        let ui_weak = ui_weak.clone();
        let resolved_app_id = resolved_app_id.clone();
        let fid = published_file_id.clone();
        let title = title_display.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                if ui.get_game_id().trim().is_empty() && resolved_app_id != "0" {
                    ui.set_game_id(SharedString::from(resolved_app_id));
                }
                ui.set_resolved_file_id(SharedString::from(fid));
                ui.set_mod_title(SharedString::from(title));
            }
        }
    });

    // Persist destination path in configuration for this Game ID and as default
    app_config
        .lock()
        .unwrap()
        .update_path(&resolved_app_id, &dest_path.to_string_lossy());

    if let Some(ref detail) = detail_opt {
        let depot_info = if detail.hcontent_file.is_empty() {
            "none"
        } else {
            &detail.hcontent_file
        };
        push_ui_log(
            &ui_weak,
            "META",
            format!(
                "Resolved '{}' (FileID: {}, AppID: {}, File: '{}', Depot: {})",
                title_display,
                if detail.publishedfileid.is_empty() {
                    &published_file_id
                } else {
                    &detail.publishedfileid
                },
                resolved_app_id,
                if detail.filename.is_empty() {
                    "steam_depot_ugc"
                } else {
                    &detail.filename
                },
                depot_info
            ),
            LogKind::Telemetry,
        );
    }

    push_ui_log(
        &ui_weak,
        "JOB",
        format!(
            "Deployment destination: '{}' (saving individual files named '{}.*')",
            dest_path.display(),
            clean_custom_name
        ),
        LogKind::Info,
    );

    update_ui_telemetry(&ui_weak, Some("DOWNLOADING"), Some("0.00 MB/s"), Some(0.15), None);

    // 4. Autonomous SteamCMD Pipeline for SteamPipe Depot items (e.g., Stormworks AppID 573090)
    // or whenever direct CDN `file_url` is absent.
    let direct_file_url = detail_opt
        .as_ref()
        .map(|d| d.file_url.trim().to_string())
        .unwrap_or_default();

    if direct_file_url.is_empty() {
        push_ui_log(
            &ui_weak,
            "SCMD",
            format!(
                "Item '{}' uses SteamPipe UGC depot. Launching autonomous SteamCMD pipeline (AppID: {}, ModID: {})...",
                title_display, resolved_app_id, published_file_id
            ),
            LogKind::Info,
        );

        let start_instant = Instant::now();
        match steamcmd::run_steamcmd_download_and_deploy(
            &ui_weak,
            &client,
            &resolved_app_id,
            &published_file_id,
            &dest_path,
            &clean_custom_name,
        )
        .await
        {
            Ok(report) => {
                let elapsed = start_instant.elapsed().as_secs_f64().max(0.001);
                let mb_total = report.total_bytes as f64 / (1024.0 * 1024.0);
                let vel_str = format!("{:.2} MB/s", mb_total / elapsed);

                push_ui_log(
                    &ui_weak,
                    "OK",
                    format!(
                        "Successfully deployed {} mod file(s) ({}.xml / {}.png) directly to '{}' ({:.2} MB, stripped {} Steam metadata/VDF file(s)).",
                        report.copied_files,
                        clean_custom_name,
                        clean_custom_name,
                        dest_path.display(),
                        mb_total,
                        report.stripped_files
                    ),
                    LogKind::Success,
                );
                update_ui_telemetry(
                    &ui_weak,
                    Some("COMPLETED"),
                    Some(&vel_str),
                    Some(1.0),
                    Some(false),
                );
            }
            Err(err_msg) => {
                push_ui_log(&ui_weak, "ERR", err_msg, LogKind::Error);
                update_ui_telemetry(
                    &ui_weak,
                    Some("ERROR"),
                    Some("0.00 MB/s"),
                    Some(0.0),
                    Some(false),
                );
            }
        }
        return;
    }

    let detail = detail_opt.expect("detail_opt guaranteed when direct_file_url is non-empty");

    // 5. Stream download from `detail.file_url` to a temporary file inside a secure temp directory
    push_ui_log(
        &ui_weak,
        "NET",
        format!("Streaming package from Steam CDN: {}", detail.file_url),
        LogKind::Info,
    );

    let response = match client.get(&detail.file_url).send().await {
        Ok(resp) => {
            if !resp.status().is_success() {
                push_ui_log(
                    &ui_weak,
                    "ERR",
                    format!("Steam CDN returned HTTP {}", resp.status()),
                    LogKind::Error,
                );
                update_ui_telemetry(&ui_weak, Some("ERROR"), Some("0.00 MB/s"), Some(0.0), Some(false));
                return;
            }
            resp
        }
        Err(e) => {
            push_ui_log(
                &ui_weak,
                "ERR",
                format!("Failed connecting to Steam CDN: {}", e),
                LogKind::Error,
            );
            update_ui_telemetry(&ui_weak, Some("ERROR"), Some("0.00 MB/s"), Some(0.0), Some(false));
            return;
        }
    };

    let total_bytes = response
        .content_length()
        .or_else(|| detail.parsed_file_size())
        .unwrap_or(0);

    let temp_dir = match tempfile::Builder::new().prefix("moddrop_stage_").tempdir() {
        Ok(d) => d,
        Err(e) => {
            push_ui_log(
                &ui_weak,
                "ERR",
                format!("Failed creating staging temporary directory: {}", e),
                LogKind::Error,
            );
            update_ui_telemetry(&ui_weak, Some("ERROR"), Some("0.00 MB/s"), Some(0.0), Some(false));
            return;
        }
    };

    let safe_name = sanitize_filename(&detail.filename, &published_file_id);
    let staging_file_path = temp_dir.path().join(&safe_name);

    let mut async_file = match tokio::fs::File::create(&staging_file_path).await {
        Ok(f) => f,
        Err(e) => {
            push_ui_log(
                &ui_weak,
                "ERR",
                format!("Failed creating temporary staging file: {}", e),
                LogKind::Error,
            );
            update_ui_telemetry(&ui_weak, Some("ERROR"), Some("0.00 MB/s"), Some(0.0), Some(false));
            return;
        }
    };

    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let start_instant = Instant::now();
    let mut last_speed_instant = Instant::now();
    let mut last_speed_bytes: u64 = 0;
    let mut ema_download_speed: f64 = 0.0;
    let mut last_telemetry_update = Instant::now();

    while let Some(chunk_res) = stream.next().await {
        let chunk = match chunk_res {
            Ok(c) => c,
            Err(e) => {
                push_ui_log(
                    &ui_weak,
                    "ERR",
                    format!("Stream interrupted during download: {}", e),
                    LogKind::Error,
                );
                update_ui_telemetry(&ui_weak, Some("ERROR"), Some("0.00 MB/s"), Some(0.0), Some(false));
                return;
            }
        };

        if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut async_file, &chunk).await {
            push_ui_log(
                &ui_weak,
                "ERR",
                format!("Disk write error in staging area: {}", e),
                LogKind::Error,
            );
            update_ui_telemetry(&ui_weak, Some("ERROR"), Some("0.00 MB/s"), Some(0.0), Some(false));
            return;
        }

        downloaded += chunk.len() as u64;

        if last_telemetry_update.elapsed() >= Duration::from_millis(150) {
            let dt = last_speed_instant.elapsed().as_secs_f64().max(0.01);
            let delta = (downloaded - last_speed_bytes) as f64;
            let inst_speed = (delta / (1024.0 * 1024.0)) / dt;
            ema_download_speed = if ema_download_speed == 0.0 {
                inst_speed
            } else {
                0.70 * inst_speed + 0.30 * ema_download_speed
            };
            let vel_str = format!("{:.2} MB/s", ema_download_speed);

            let prog = if total_bytes > 0 {
                0.15 + 0.70 * ((downloaded as f32 / total_bytes as f32).clamp(0.0, 1.0))
            } else {
                0.50
            };

            update_ui_telemetry(&ui_weak, Some("DOWNLOADING"), Some(&vel_str), Some(prog), None);
            last_speed_bytes = downloaded;
            last_speed_instant = Instant::now();
            last_telemetry_update = Instant::now();
        }
    }

    if let Err(e) = tokio::io::AsyncWriteExt::flush(&mut async_file).await {
        push_ui_log(
            &ui_weak,
            "ERR",
            format!("Failed flushing downloaded staging file: {}", e),
            LogKind::Error,
        );
        update_ui_telemetry(&ui_weak, Some("ERROR"), Some("0.00 MB/s"), Some(0.0), Some(false));
        return;
    }
    drop(async_file);

    let elapsed_secs = start_instant.elapsed().as_secs_f64().max(0.001);
    let avg_mb_s = (downloaded as f64 / (1024.0 * 1024.0)) / elapsed_secs;
    let final_vel_str = format!("{:.2} MB/s", avg_mb_s);

    push_ui_log(
        &ui_weak,
        "TELEM",
        format!(
            "Download complete: {:.2} MB transferred in {:.2}s (avg {})",
            downloaded as f64 / (1024.0 * 1024.0),
            elapsed_secs,
            final_vel_str
        ),
        LogKind::Telemetry,
    );

    update_ui_telemetry(&ui_weak, Some("DEPLOYING"), Some(&final_vel_str), Some(0.90), None);

    // 6. Deploy / Extract pure mod assets directly into destination folder
    let deploy_result = tokio::task::spawn_blocking({
        let staging_file_path = staging_file_path.clone();
        let dest_path_clone = dest_path.clone();
        let mod_id_clone = published_file_id.clone();
        let custom_name_clone = clean_custom_name.clone();
        move || -> Result<String, String> {
            if is_zip_archive(&staging_file_path) {
                let count = extract_zip_safely(&staging_file_path, &dest_path_clone)?;
                post_process_extracted_folder(&dest_path_clone, &mod_id_clone, &custom_name_clone);
                Ok(format!(
                    "Successfully deployed {} asset(s) directly to '{}' with automatically generated names.",
                    count,
                    dest_path_clone.display()
                ))
            } else {
                let ext = Path::new(&safe_name)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("bin");
                let target_file = dest_path_clone.join(format!("{}.{}", custom_name_clone, ext));
                fs::copy(&staging_file_path, &target_file).map_err(|e| {
                    format!(
                        "Failed deploying package to '{}': {}",
                        target_file.display(),
                        e
                    )
                })?;
                Ok(format!(
                    "Deployed '{}'",
                    target_file.display()
                ))
            }
        }
    })
    .await;

    match deploy_result {
        Ok(Ok(summary)) => {
            push_ui_log(&ui_weak, "OK", summary, LogKind::Success);
            update_ui_telemetry(&ui_weak, Some("COMPLETED"), Some(&final_vel_str), Some(1.0), Some(false));
        }
        Ok(Err(e)) => {
            push_ui_log(&ui_weak, "ERR", e, LogKind::Error);
            update_ui_telemetry(&ui_weak, Some("ERROR"), Some("0.00 MB/s"), Some(0.0), Some(false));
        }
        Err(e) => {
            push_ui_log(
                &ui_weak,
                "ERR",
                format!("Deployment worker panicked: {}", e),
                LogKind::Error,
            );
            update_ui_telemetry(&ui_weak, Some("ERROR"), Some("0.00 MB/s"), Some(0.0), Some(false));
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    let http_client = Client::builder()
        .user_agent("ModDrop-Native-Utility/0.1.0 (Windows; Rust; Slint)")
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(10)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(600))
        .build()
        .expect("Failed to initialize HTTP client");

    let app_config = Arc::new(Mutex::new(AppConfig::load()));

    // Restore last-used destination directory on launch
    {
        let cfg = app_config.lock().unwrap();
        if !cfg.last_destination.is_empty() {
            let last_dest = cfg.last_destination.clone();
            ui.set_location_path(SharedString::from(&last_dest));
            push_ui_log(
                &ui.as_weak(),
                "CFG",
                format!("Restored last-used destination directory: {}", last_dest),
                LogKind::Info,
            );
        }
    }

    let check_seq = Arc::new(AtomicU64::new(0));

    let run_auto_detect_game_id: Arc<dyn Fn(Weak<AppWindow>, String, u64, bool) + Send + Sync> = {
        let check_seq = check_seq.clone();
        let client = http_client.clone();
        Arc::new(move |ui_weak: Weak<AppWindow>, app_id: String, debounce_ms: u64, is_explicit: bool| {
            let seq = check_seq.fetch_add(1, Ordering::SeqCst) + 1;
            let check_seq_task = check_seq.clone();
            let client_task = client.clone();
            let ui_weak_task = ui_weak.clone();

            tokio::spawn(async move {
                if debounce_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(debounce_ms)).await;
                }

                if check_seq_task.load(Ordering::SeqCst) != seq {
                    return;
                }

                let _ = slint::invoke_from_event_loop({
                    let ui_weak = ui_weak_task.clone();
                    move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_is_checking_game(true);
                            ui.set_game_name(SharedString::from(""));
                        }
                    }
                });

                let res = fetch_app_details(&client_task, &app_id).await;

                if check_seq_task.load(Ordering::SeqCst) != seq {
                    let _ = slint::invoke_from_event_loop({
                        let ui_weak = ui_weak_task.clone();
                        move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_is_checking_game(false);
                            }
                        }
                    });
                    return;
                }

                match res {
                    Ok((name, app_type)) => {
                        push_ui_log(
                            &ui_weak_task,
                            "OK",
                            format!("Auto-detected AppID [{}]: '{}' (type: {})", app_id, name, app_type),
                            LogKind::Success,
                        );
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_task.upgrade() {
                                ui.set_is_checking_game(false);
                                ui.set_game_name(SharedString::from(name));
                            }
                        });
                    }
                    Err(err_msg) => {
                        if is_explicit {
                            push_ui_log(&ui_weak_task, "ERR", err_msg, LogKind::Error);
                        }
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_task.upgrade() {
                                ui.set_is_checking_game(false);
                                ui.set_game_name(SharedString::from(""));
                            }
                        });
                    }
                }
            });
        })
    };

    let handle_mod_link_change = {
        let client = http_client.clone();
        let detector = run_auto_detect_game_id.clone();
        let config = app_config.clone();
        move |ui_weak: Weak<AppWindow>, val: String| {
            let (file_id_opt, app_id_opt) = parse_workshop_input(&val);
            if let Some(ui) = ui_weak.upgrade() {
                if let Some(ref fid) = file_id_opt {
                    ui.set_resolved_file_id(SharedString::from(fid));
                } else {
                    ui.set_resolved_file_id(SharedString::from(""));
                    ui.set_mod_title(SharedString::from(""));
                }

                if let Some(ref aid) = app_id_opt {
                    if ui.get_game_id().trim().is_empty() {
                        ui.set_game_id(SharedString::from(aid));
                        push_ui_log(
                            &ui_weak,
                            "PARSE",
                            format!("Detected AppID [{}] from Workshop URL.", aid),
                            LogKind::Telemetry,
                        );
                        detector(ui_weak.clone(), aid.clone(), 0, false);
                    }

                    if let Some(saved_path) = config.lock().unwrap().get_path_for_game(aid) {
                        if ui.get_location_path().trim().is_empty() {
                            ui.set_location_path(SharedString::from(&saved_path));
                            push_ui_log(
                                &ui_weak,
                                "CFG",
                                format!("Auto-selected destination path for AppID [{}]: {}", aid, saved_path),
                                LogKind::Info,
                            );
                        }
                    }
                }
            }

            if let Some(fid) = file_id_opt {
                let ui_weak_clone = ui_weak.clone();
                let client_clone = client.clone();
                let detector_clone = detector.clone();
                let config_clone = config.clone();
                tokio::spawn(async move {
                    if let Ok(detail) = fetch_workshop_details(&client_clone, &fid).await {
                        let detected_app = if detail.consumer_app_id > 0 {
                            Some(detail.consumer_app_id.to_string())
                        } else if detail.creator_app_id > 0 {
                            Some(detail.creator_app_id.to_string())
                        } else {
                            None
                        };

                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_clone.upgrade() {
                                if ui.get_resolved_file_id().as_str() == fid {
                                    if let Some(ref app_id) = detected_app {
                                        if ui.get_game_id().trim().is_empty() {
                                            ui.set_game_id(SharedString::from(app_id));
                                            detector_clone(ui_weak_clone.clone(), app_id.clone(), 0, false);
                                        }

                                        if let Some(saved_path) = config_clone.lock().unwrap().get_path_for_game(app_id) {
                                            if ui.get_location_path().trim().is_empty() {
                                                ui.set_location_path(SharedString::from(&saved_path));
                                                push_ui_log(
                                                    &ui_weak_clone,
                                                    "CFG",
                                                    format!("Auto-selected destination path for AppID [{}]: {}", app_id, saved_path),
                                                    LogKind::Info,
                                                );
                                            }
                                        }
                                    }
                                    if !detail.title.is_empty() {
                                        ui.set_mod_title(SharedString::from(&detail.title));
                                    }
                                }
                            }
                        });
                    }
                });
            }
        }
    };

    // Auto-detect Workshop URL on clipboard on startup
    if let Some(clip_text) = get_clipboard_text() {
        let (fid, _) = parse_workshop_input(&clip_text);
        if fid.is_some() {
            ui.set_mod_link(SharedString::from(&clip_text));
            push_ui_log(
                &ui.as_weak(),
                "CLIP",
                format!("Auto-loaded Workshop URL from clipboard: {}", clip_text),
                LogKind::Success,
            );
            handle_mod_link_change(ui.as_weak(), clip_text);
        }
    }

    // Callback: Game ID field edited (auto-detects game when typed or pasted)
    ui.on_game_id_edited({
        let ui_weak = ui.as_weak();
        let check_seq = check_seq.clone();
        let detector = run_auto_detect_game_id.clone();
        let config = app_config.clone();
        move |val| {
            let input = val.as_str();
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };

            ui.set_game_name(SharedString::from(""));

            let is_url = input.contains("http") || input.contains("/app/") || input.contains("appid=");
            let parsed = parse_game_id_input(input);

            if let Some(ref extracted_app_id) = parsed {
                if let Some(saved_path) = config.lock().unwrap().get_path_for_game(extracted_app_id) {
                    if ui.get_location_path().trim().is_empty() {
                        ui.set_location_path(SharedString::from(&saved_path));
                        push_ui_log(
                            &ui_weak,
                            "CFG",
                            format!("Auto-selected destination path for AppID [{}]: {}", extracted_app_id, saved_path),
                            LogKind::Info,
                        );
                    }
                }

                if is_url {
                    ui.set_game_id(SharedString::from(extracted_app_id));
                    push_ui_log(
                        &ui_weak,
                        "PARSE",
                        format!("Auto-extracted Steam AppID [{}] from pasted URL.", extracted_app_id),
                        LogKind::Telemetry,
                    );
                    detector(ui_weak.clone(), extracted_app_id.clone(), 150, false);
                } else if extracted_app_id.len() >= 3 {
                    detector(ui_weak.clone(), extracted_app_id.clone(), 600, false);
                } else {
                    check_seq.fetch_add(1, Ordering::SeqCst);
                    ui.set_is_checking_game(false);
                }
            } else {
                check_seq.fetch_add(1, Ordering::SeqCst);
                ui.set_is_checking_game(false);
            }
        }
    });

    // Callback: Check Game ID (invoked when user hits Enter inside Game ID input)
    ui.on_check_game_id({
        let ui_weak = ui.as_weak();
        let detector = run_auto_detect_game_id.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            if ui.get_is_busy() {
                return;
            }

            let raw_input = ui.get_game_id().to_string();
            let Some(app_id) = parse_game_id_input(&raw_input) else {
                if !raw_input.trim().is_empty() {
                    push_ui_log(
                        &ui_weak,
                        "ERR",
                        "Cannot check AppID: Please enter a valid numeric Steam AppID or Store URL in 'ID Game'.",
                        LogKind::Error,
                    );
                }
                return;
            };

            if raw_input.trim() != app_id {
                ui.set_game_id(SharedString::from(&app_id));
            }

            detector(ui_weak.clone(), app_id, 0, true);
        }
    });

    // Callback: Mod Link field edited
    ui.on_mod_link_edited({
        let ui_weak = ui.as_weak();
        let handler = handle_mod_link_change.clone();
        move |val| {
            handler(ui_weak.clone(), val.to_string());
        }
    });

    // Callback: Paste Mod Link button
    ui.on_paste_mod_link({
        let ui_weak = ui.as_weak();
        let handler = handle_mod_link_change.clone();
        move || {
            if let Some(text) = get_clipboard_text() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_mod_link(SharedString::from(trimmed));
                    }
                    push_ui_log(
                        &ui_weak,
                        "CLIP",
                        format!("Pasted Workshop URL from clipboard: {}", trimmed),
                        LogKind::Success,
                    );
                    handler(ui_weak.clone(), trimmed.to_string());
                } else {
                    push_ui_log(&ui_weak, "CLIP", "Clipboard is empty.", LogKind::Info);
                }
            } else {
                push_ui_log(
                    &ui_weak,
                    "CLIP",
                    "Clipboard does not contain readable text.",
                    LogKind::Info,
                );
            }
        }
    });

    // Callback: Location Path field edited
    ui.on_location_path_edited({
        let ui_weak = ui.as_weak();
        let config = app_config.clone();
        move |val| {
            let current_game = ui_weak
                .upgrade()
                .map(|u| u.get_game_id().to_string())
                .unwrap_or_default();
            config.lock().unwrap().update_path(&current_game, val.as_str());
        }
    });

    // Callback: Browse Folder (`rfd` native OS folder dialog offloaded from UI thread)
    ui.on_browse_folder({
        let ui_weak = ui.as_weak();
        let config = app_config.clone();
        move || {
            let ui_weak_clone = ui_weak.clone();
            let config_clone = config.clone();
            let current_dir = ui_weak
                .upgrade()
                .map(|u| u.get_location_path().to_string())
                .unwrap_or_default();

            tokio::task::spawn_blocking(move || {
                let mut dialog = rfd::FileDialog::new()
                    .set_title("Select Destination Local Game Mod Folder");

                let trimmed = current_dir.trim();
                if !trimmed.is_empty() {
                    let p = Path::new(trimmed);
                    if p.exists() && p.is_dir() {
                        dialog = dialog.set_directory(p);
                    }
                }

                if let Some(folder) = dialog.pick_folder() {
                    let folder_str = folder.to_string_lossy().to_string();
                    let _ = slint::invoke_from_event_loop({
                        let ui_weak_inner = ui_weak_clone.clone();
                        let folder_str_clone = folder_str.clone();
                        move || {
                            if let Some(ui) = ui_weak_inner.upgrade() {
                                ui.set_location_path(SharedString::from(folder_str_clone));
                            }
                        }
                    });

                    let current_game = ui_weak_clone
                        .upgrade()
                        .map(|u| u.get_game_id().to_string())
                        .unwrap_or_default();
                    config_clone.lock().unwrap().update_path(&current_game, &folder_str);

                    push_ui_log(
                        &ui_weak_clone,
                        "PATH",
                        format!("Selected destination folder: {}", folder_str),
                        LogKind::Info,
                    );
                }
            });
        }
    });

    // Callback: Open Destination Folder in File Explorer
    ui.on_open_folder({
        let ui_weak = ui.as_weak();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return; };
            let path_str = ui.get_location_path().to_string();
            let trimmed = path_str.trim();
            if trimmed.is_empty() {
                push_ui_log(
                    &ui_weak,
                    "ERR",
                    "Cannot open folder: No destination path specified.",
                    LogKind::Error,
                );
                return;
            }
            let path = Path::new(trimmed);
            if path.exists() {
                #[cfg(target_os = "windows")]
                {
                    let _ = std::process::Command::new("explorer").arg(path).spawn();
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
                }
                push_ui_log(
                    &ui_weak,
                    "OPEN",
                    format!("Opened destination folder in File Explorer: {}", trimmed),
                    LogKind::Info,
                );
            } else {
                push_ui_log(
                    &ui_weak,
                    "ERR",
                    format!("Destination folder does not exist on disk: {}", trimmed),
                    LogKind::Error,
                );
            }
        }
    });

    // Callback: Clear Logs
    ui.on_clear_logs({
        let ui_weak = ui.as_weak();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                let ts = Local::now().format("%H:%M:%S").to_string();
                let reset = vec![LogEntry {
                    timestamp: SharedString::from(ts),
                    level: SharedString::from("SYS"),
                    message: SharedString::from("Console buffer cleared."),
                    kind: LogKind::Info as i32,
                    is_latest: true,
                }];
                ui.set_logs(ModelRc::from(Rc::new(VecModel::from(reset))));
                ui.set_progress(0.0);
                ui.set_status_badge(SharedString::from("READY"));
                ui.set_velocity_text(SharedString::from("0.00 MB/s"));
            }
        }
    });

    // Callback: Download & Deploy Trigger
    ui.on_download_and_deploy({
        let ui_weak = ui.as_weak();
        let client = http_client.clone();
        let config = app_config.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                if ui.get_is_busy() {
                    return;
                }
                let game_id = ui.get_game_id().to_string();
                let mod_link = ui.get_mod_link().to_string();
                let location_path = ui.get_location_path().to_string();

                let ui_weak_task = ui_weak.clone();
                let client_task = client.clone();
                let config_task = config.clone();

                tokio::spawn(async move {
                    execute_download_and_deploy(
                        ui_weak_task,
                        client_task,
                        config_task,
                        game_id,
                        mod_link,
                        location_path,
                    )
                    .await;
                });
            }
        }
    });

    ui.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_workshop_url_and_raw_id() {
        let (fid, aid) = parse_workshop_input(
            "https://steamcommunity.com/sharedfiles/filedetails/?id=2876543210&searchtext=",
        );
        assert_eq!(fid.as_deref(), Some("2876543210"));
        assert_eq!(aid, None);

        let (fid2, aid2) = parse_workshop_input(
            "https://steamcommunity.com/workshop/filedetails/?id=123456789&appid=294100",
        );
        assert_eq!(fid2.as_deref(), Some("123456789"));
        assert_eq!(aid2.as_deref(), Some("294100"));

        let (fid3, _) = parse_workshop_input("3344556677");
        assert_eq!(fid3.as_deref(), Some("3344556677"));
    }

    #[test]
    fn test_parse_game_id_input() {
        assert_eq!(
            parse_game_id_input("https://store.steampowered.com/app/294100/RimWorld/").as_deref(),
            Some("294100")
        );
        assert_eq!(parse_game_id_input("108600").as_deref(), Some("108600"));
    }

    #[test]
    fn test_sanitize_filename_prevents_traversal() {
        assert_eq!(
            sanitize_filename("../../evil/path/mod*name?.zip", "999"),
            "mod_name_.zip"
        );
        assert_eq!(sanitize_filename("...", "12345"), "workshop_mod_12345.bin");
    }

    #[test]
    fn test_latency_classification() {
        let classify = |ms: u128| -> i32 {
            if ms < 120 { 1 } else if ms < 250 { 2 } else { 3 }
        };
        assert_eq!(classify(45), 1);
        assert_eq!(classify(180), 2);
        assert_eq!(classify(350), 3);
    }

    #[test]
    fn test_sanitize_custom_folder_name() {
        assert_eq!(sanitize_custom_folder_name("Targa 46", "999"), "Targa 46");
        assert_eq!(
            sanitize_custom_folder_name("Targa: 46 / Special?", "999"),
            "Targa_ 46 _ Special_"
        );
        assert_eq!(sanitize_custom_folder_name("", "999"), "mod_999");
        assert_eq!(sanitize_custom_folder_name("...", "999"), "mod_999");
    }

    #[test]
    fn test_post_process_extracted_folder_renames() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path();

        fs::write(path.join("vehicle.xml"), b"<v/>").unwrap();
        fs::write(path.join("workshop_preview.png"), b"png").unwrap();
        fs::write(path.join("3771890254_desc.txt"), b"desc").unwrap();

        post_process_extracted_folder(path, "3771890254", "Targa 46");

        assert!(path.join("Targa 46.xml").is_file());
        assert!(path.join("Targa 46.png").is_file());
        assert!(path.join("Targa 46_desc.txt").is_file());
        assert!(!path.join("vehicle.xml").exists());
        assert!(!path.join("workshop_preview.png").exists());
    }

    #[test]
    fn test_app_config_persistence_and_game_routing() {
        let mut cfg = AppConfig::default();
        assert_eq!(cfg.get_path_for_game("294100"), None);

        cfg.update_path("294100", "C:\\Games\\RimWorld\\Mods");
        assert_eq!(
            cfg.get_path_for_game("294100"),
            Some("C:\\Games\\RimWorld\\Mods".to_string())
        );
        assert_eq!(cfg.last_destination, "C:\\Games\\RimWorld\\Mods");

        // Game with no specific saved path falls back to last_destination
        assert_eq!(
            cfg.get_path_for_game("573090"),
            Some("C:\\Games\\RimWorld\\Mods".to_string())
        );

        cfg.update_path("573090", "C:\\Stormworks\\data\\vehicles");
        assert_eq!(
            cfg.get_path_for_game("573090"),
            Some("C:\\Stormworks\\data\\vehicles".to_string())
        );
        // Previously saved game path remains intact
        assert_eq!(
            cfg.get_path_for_game("294100"),
            Some("C:\\Games\\RimWorld\\Mods".to_string())
        );
    }
}

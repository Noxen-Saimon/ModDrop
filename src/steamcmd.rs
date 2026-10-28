use crate::{AppWindow, LogKind, push_ui_log, update_ui_telemetry};
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use regex::Regex;
use reqwest::Client;
use slint::Weak;
use std::env;
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const STEAMCMD_WIN_URL: &str = "https://steamcdn-a.akamaihd.net/client/installer/steamcmd.zip";
const STEAMCMD_LINUX_URL: &str =
    "https://steamcdn-a.akamaihd.net/client/installer/steamcmd_linux.tar.gz";
const STEAMCMD_MACOS_URL: &str =
    "https://steamcdn-a.akamaihd.net/client/installer/steamcmd_osx.tar.gz";

static RE_STEAMCMD_PROGRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"progress:\s*(\d+(?:\.\d+)?)\s*\((\d+)\s*/\s*(\d+)\)")
        .expect("Invalid SteamCMD progress regex")
});

#[derive(Debug, Clone)]
pub struct SteamCmdInstall {
    /// Path to the `steamcmd` executable (`steamcmd.exe` on Windows, `steamcmd.sh` or `steamcmd` on Linux/macOS).
    pub executable: PathBuf,
    /// Root directory where SteamCMD stores `steamapps/workshop/content/<AppID>/<ModID>/`.
    pub root_dir: PathBuf,
}

#[derive(Debug, Default)]
pub struct CleanDeployReport {
    pub copied_files: usize,
    pub stripped_files: usize,
    pub total_bytes: u64,
}

/// Resolves the OS-appropriate application data directory for ModDrop's portable SteamCMD installation:
/// - Windows: `%LOCALAPPDATA%\ModDrop\bin`
/// - Linux/Unix: `$XDG_DATA_HOME/moddrop/bin` or `~/.local/share/moddrop/bin`
pub fn resolve_moddrop_bin_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    {
        if let Ok(local_app_data) = env::var("LOCALAPPDATA")
            && !local_app_data.trim().is_empty()
        {
            return Ok(PathBuf::from(local_app_data).join("ModDrop").join("bin"));
        }
        if let Ok(user_profile) = env::var("USERPROFILE")
            && !user_profile.trim().is_empty()
        {
            return Ok(PathBuf::from(user_profile)
                .join("AppData")
                .join("Local")
                .join("ModDrop")
                .join("bin"));
        }
        Err("Unable to resolve %LOCALAPPDATA% or %USERPROFILE% on Windows.".to_string())
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(xdg_data) = env::var("XDG_DATA_HOME")
            && !xdg_data.trim().is_empty()
        {
            return Ok(PathBuf::from(xdg_data).join("moddrop").join("bin"));
        }
        if let Ok(home) = env::var("HOME")
            && !home.trim().is_empty()
        {
            return Ok(PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("moddrop")
                .join("bin"));
        }
        Err("Unable to resolve $XDG_DATA_HOME or $HOME for ModDrop data directory.".to_string())
    }
}

/// Checks the system `PATH` for an existing `steamcmd` binary.
pub fn find_steamcmd_in_path() -> Option<SteamCmdInstall> {
    let path_var = env::var_os("PATH")?;
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["steamcmd.exe", "steamcmd.cmd", "steamcmd.bat"]
    } else {
        &["steamcmd", "steamcmd.sh"]
    };

    for dir in env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for candidate in candidates {
            let full_path = dir.join(candidate);
            if full_path.is_file() {
                let resolved = full_path.canonicalize().unwrap_or(full_path);
                let root_dir = resolved
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| dir.clone());
                return Some(SteamCmdInstall {
                    executable: resolved,
                    root_dir,
                });
            }
        }
    }
    None
}

/// Checks if SteamCMD is already provisioned inside ModDrop's local `bin` directory.
fn find_provisioned_steamcmd(bin_dir: &Path) -> Option<SteamCmdInstall> {
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["steamcmd.exe"]
    } else {
        &["steamcmd.sh", "steamcmd"]
    };

    for candidate in candidates {
        let exe = bin_dir.join(candidate);
        if exe.is_file() {
            return Some(SteamCmdInstall {
                executable: exe,
                root_dir: bin_dir.to_path_buf(),
            });
        }
    }
    None
}

/// Ensures executable permissions (`chmod 0755`) on Unix/Linux systems for SteamCMD scripts & binaries.
fn ensure_executable_permissions(bin_dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let targets = [
            bin_dir.join("steamcmd.sh"),
            bin_dir.join("steamcmd"),
            bin_dir.join("linux32").join("steamcmd"),
            bin_dir.join("linux64").join("steamcmd"),
        ];
        for target in targets {
            if target.is_file() {
                let mut perms = fs::metadata(&target)
                    .map_err(|e| format!("Failed reading permissions for '{}': {}", target.display(), e))?
                    .permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&target, perms).map_err(|e| {
                    format!("Failed setting executable permissions on '{}': {}", target.display(), e)
                })?;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = bin_dir;
    }
    Ok(())
}

/// Extracts a `.zip` archive safely into `target_dir` using `enclosed_name()` to prevent path traversal.
fn extract_steamcmd_zip(
    ui_weak: &Weak<AppWindow>,
    archive_path: &Path,
    target_dir: &Path,
) -> Result<(), String> {
    push_ui_log(
        ui_weak,
        "BOOT",
        format!(
            "Unpacking SteamCMD archive into '{}'...",
            target_dir.display()
        ),
        LogKind::Info,
    );

    let file = File::open(archive_path)
        .map_err(|e| format!("Failed opening SteamCMD archive '{}': {}", archive_path.display(), e))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("Corrupt SteamCMD ZIP archive: {}", e))?;

    let total = archive.len();
    for i in 0..total {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Failed reading SteamCMD ZIP entry #{}: {}", i, e))?;
        let Some(enclosed) = entry.enclosed_name().map(|p| p.to_path_buf()) else {
            return Err("Security guard triggered: Blocked unsafe relative path in SteamCMD archive.".to_string());
        };
        let out_path = target_dir.join(enclosed);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .map_err(|e| format!("Failed creating dir '{}': {}", out_path.display(), e))?;
        } else {
            if let Some(parent) = out_path.parent()
                && !parent.exists()
            {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed creating parent dir '{}': {}", parent.display(), e))?;
            }
            let mut outfile = File::create(&out_path)
                .map_err(|e| format!("Failed creating file '{}': {}", out_path.display(), e))?;
            io::copy(&mut entry, &mut outfile)
                .map_err(|e| format!("Failed extracting '{}': {}", out_path.display(), e))?;
            outfile
                .flush()
                .map_err(|e| format!("Failed flushing '{}': {}", out_path.display(), e))?;
        }
    }

    push_ui_log(
        ui_weak,
        "BOOT",
        format!("ZIP extraction complete ({} items unpacked).", total),
        LogKind::Info,
    );
    Ok(())
}

/// Extracts a `.tar.gz` archive safely into `target_dir` for Linux/macOS SteamCMD packages.
fn extract_steamcmd_tar_gz(
    ui_weak: &Weak<AppWindow>,
    archive_path: &Path,
    target_dir: &Path,
) -> Result<(), String> {
    push_ui_log(
        ui_weak,
        "BOOT",
        format!(
            "Unpacking SteamCMD tar.gz into '{}'...",
            target_dir.display()
        ),
        LogKind::Info,
    );
    let file = File::open(archive_path).map_err(|e| {
        format!(
            "Failed opening SteamCMD tar.gz '{}': {}",
            archive_path.display(),
            e
        )
    })?;
    let gz = GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    archive
        .unpack(target_dir)
        .map_err(|e| format!("Failed unpacking SteamCMD tar.gz archive: {}", e))?;

    push_ui_log(
        ui_weak,
        "BOOT",
        "tar.gz extraction complete.",
        LogKind::Info,
    );
    Ok(())
}

/// Handles a single line of SteamCMD output (from stdout or console_log.txt),
/// cleaning ANSI codes, parsing progress percentages, updating UI telemetry,
/// and returning `(is_success, option_error)`.
fn process_steamcmd_line(
    ui_weak: &Weak<AppWindow>,
    raw_line: &str,
    success_detected: &mut bool,
    error_detected: &mut Option<String>,
) {
    let cleaned = sanitize_console_line(raw_line);
    if cleaned.is_empty() {
        return;
    }

    // Telemetry progress parser: "progress: 45.2 (12345 / 67890)"
    if let Some(caps) = RE_STEAMCMD_PROGRESS.captures(&cleaned) {
        let pct: f32 = caps.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0.0);
        let cur_bytes: u64 = caps.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
        let tot_bytes: u64 = caps.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
        let ui_prog = 0.25 + 0.60 * (pct / 100.0).clamp(0.0, 1.0);
        let mb_done = cur_bytes as f64 / (1024.0 * 1024.0);
        let mb_tot = tot_bytes as f64 / (1024.0 * 1024.0);
        let status_vel = format!("{:.1}/{:.1}M", mb_done, mb_tot);
        update_ui_telemetry(ui_weak, Some("DOWNLOAD"), Some(&status_vel), Some(ui_prog), None);
    }

    let lower = cleaned.to_ascii_lowercase();
    if lower.contains("success. downloaded item") {
        *success_detected = true;
        *error_detected = None;
        push_ui_log(ui_weak, "SCMD", &cleaned, LogKind::Success);
    } else if lower.contains("verifying installation...ok") {
        push_ui_log(ui_weak, "SCMD", &cleaned, LogKind::Success);
    } else if lower.contains("error!") || lower.contains("failed") || lower.contains("no subscription") || lower.contains("access denied") || lower.contains("invalid password") {
        *error_detected = Some(cleaned.clone());
        push_ui_log(ui_weak, "SCMD", &cleaned, LogKind::Error);
    } else {
        push_ui_log(ui_weak, "SCMD", &cleaned, LogKind::Telemetry);
    }
}

/// Computes total size of all files in a path recursively with zero-overhead file metadata polling.
fn get_path_size(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    if path.is_file() {
        return fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(m) = entry.metadata() {
                if m.is_file() {
                    total += m.len();
                } else if m.is_dir() {
                    total += get_path_size(&entry.path());
                }
            }
        }
    }
    total
}

/// Spawns SteamCMD and streams output simultaneously from both stdout/stderr pipes
/// AND by tailing `logs/console_log.txt` in real time, computing real-time download speeds.
async fn run_steamcmd_process(
    ui_weak: &Weak<AppWindow>,
    install: &SteamCmdInstall,
    args: &[&str],
    item_ctx: Option<(&str, &str)>,
    timeout_duration: Duration,
) -> Result<(bool, Option<String>), String> {
    // Record current end position of console_log.txt so we only read new lines from this run
    let console_log_path = install.root_dir.join("logs").join("console_log.txt");
    let mut initial_log_offset: u64 = 0;
    if let Ok(meta) = fs::metadata(&console_log_path) {
        initial_log_offset = meta.len();
    }

    let mut cmd = Command::new(&install.executable);
    cmd.current_dir(&install.root_dir);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn().map_err(|e| {
        format!(
            "Failed spawning SteamCMD process '{}': {}",
            install.executable.display(),
            e
        )
    })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed capturing SteamCMD stdout pipe.".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed capturing SteamCMD stderr pipe.".to_string())?;

    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();

    let mut success_detected = false;
    let mut error_detected: Option<String> = None;
    let mut log_offset = initial_log_offset;

    let stream_future = async {
        let mut log_poll_interval = tokio::time::interval(Duration::from_millis(150));
        let mut child_exited = false;

        let mut last_disk_check = Instant::now();
        let mut last_disk_bytes: u64 = 0;
        let mut ema_disk_speed: f64 = 0.0;
        let staging_paths = item_ctx.map(|(aid, mid)| {
            (
                install.root_dir.join("steamapps").join("workshop").join("downloads").join(aid).join(mid),
                install.root_dir.join("steamapps").join("workshop").join("content").join(aid).join(mid),
            )
        });

        while !child_exited {
            tokio::select! {
                line_res = stdout_lines.next_line() => {
                    match line_res {
                        Ok(Some(raw_line)) => {
                            process_steamcmd_line(ui_weak, &raw_line, &mut success_detected, &mut error_detected);
                        }
                        Ok(None) => {},
                        Err(_) => {},
                    }
                }
                err_res = stderr_lines.next_line() => {
                    if let Ok(Some(raw_line)) = err_res {
                        let cleaned = sanitize_console_line(&raw_line);
                        if !cleaned.is_empty() {
                            push_ui_log(ui_weak, "SCMD", &cleaned, LogKind::Info);
                        }
                    }
                }
                _ = log_poll_interval.tick() => {
                    // Check if process has terminated
                    if let Ok(Some(_)) = child.try_wait() {
                        child_exited = true;
                    }

                    // Compute real-time download speed from disk staging delta using minimal CPU
                    if let Some((ref dl_dir, ref ct_dir)) = staging_paths {
                        let dt = last_disk_check.elapsed().as_secs_f64();
                        if dt >= 1.50 && !success_detected {
                            let cur_bytes = get_path_size(dl_dir) + get_path_size(ct_dir);
                            if cur_bytes > 0 && cur_bytes >= last_disk_bytes {
                                let delta = (cur_bytes - last_disk_bytes) as f64;
                                let inst_speed = (delta / (1024.0 * 1024.0)) / dt;
                                if inst_speed > 0.01 {
                                    ema_disk_speed = if ema_disk_speed == 0.0 {
                                        inst_speed
                                    } else {
                                        0.70 * inst_speed + 0.30 * ema_disk_speed
                                    };
                                    let vel_str = format!("{:.2} MB/s", ema_disk_speed);
                                    update_ui_telemetry(ui_weak, Some("DOWNLOADING"), Some(&vel_str), None, None);
                                }
                            }
                            last_disk_bytes = cur_bytes;
                            last_disk_check = Instant::now();
                        }
                    }

                    // Tail new lines from console_log.txt
                    if let Ok(mut file) = File::open(&console_log_path)
                        && let Ok(curr_len) = file.metadata().map(|m| m.len())
                        && curr_len > log_offset
                        && file.seek(SeekFrom::Start(log_offset)).is_ok()
                    {
                        use std::io::BufRead;
                        let reader = std::io::BufReader::new(file);
                        for line in reader.lines().map_while(Result::ok) {
                            let payload = if let Some(idx) = line.find(']') {
                                line[idx + 1..].trim()
                            } else {
                                line.trim()
                            };
                            if !payload.is_empty() {
                                process_steamcmd_line(ui_weak, payload, &mut success_detected, &mut error_detected);
                            }
                        }
                        log_offset = curr_len;
                    }
                }
            }
        }

        // Final log tail drain
        if let Ok(mut file) = File::open(&console_log_path)
            && let Ok(curr_len) = file.metadata().map(|m| m.len())
            && curr_len > log_offset
            && file.seek(SeekFrom::Start(log_offset)).is_ok()
        {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(file);
            for line in reader.lines().map_while(Result::ok) {
                let payload = if let Some(idx) = line.find(']') {
                    line[idx + 1..].trim()
                } else {
                    line.trim()
                };
                if !payload.is_empty() {
                    process_steamcmd_line(ui_weak, payload, &mut success_detected, &mut error_detected);
                }
            }
        }

        let _ = child.wait().await;
        Ok::<(), String>(())
    };

    match tokio::time::timeout(timeout_duration, stream_future).await {
        Ok(res) => {
            res?;
            Ok((success_detected, error_detected))
        }
        Err(_) => {
            let _ = child.kill().await;
            Err(format!(
                "SteamCMD process timed out after {}s.",
                timeout_duration.as_secs()
            ))
        }
    }
}

/// Runs `steamcmd +quit` once during setup to let SteamCMD perform and finish its mandatory self-update.
async fn run_initial_self_update(
    ui_weak: &Weak<AppWindow>,
    install: &SteamCmdInstall,
) -> Result<(), String> {
    push_ui_log(
        ui_weak,
        "BOOT",
        "Running SteamCMD initial self-update cycle (+quit)...",
        LogKind::Info,
    );
    update_ui_telemetry(ui_weak, Some("UPDATING"), Some("0.00 MB/s"), Some(0.20), None);

    let (succeeded, err_opt) = run_steamcmd_process(
        ui_weak,
        install,
        &["+quit"],
        None,
        Duration::from_secs(180), // SteamCMD self-update can download 60-100MB of package components
    )
    .await?;

    let _ = succeeded;
    if let Some(err) = err_opt {
        push_ui_log(
            ui_weak,
            "WARN",
            format!("SteamCMD self-update reported: {}", err),
            LogKind::Info,
        );
    }

    push_ui_log(
        ui_weak,
        "OK",
        "SteamCMD self-update completed successfully.",
        LogKind::Success,
    );
    Ok(())
}

/// Auto-provisions SteamCMD with explicit progress logging, timeouts, and self-update initialization:
/// 1. Checks `PATH` for `steamcmd`.
/// 2. Checks OS app data dir (`%LOCALAPPDATA%\ModDrop\bin` or `~/.local/share/moddrop/bin`).
/// 3. Downloads and extracts the official portable Valve SteamCMD binary if missing.
/// 4. Executes `steamcmd +quit` to finalize its initial self-update cycle cleanly.
pub async fn ensure_steamcmd(
    ui_weak: &Weak<AppWindow>,
    client: &Client,
) -> Result<SteamCmdInstall, String> {
    if let Some(install) = find_steamcmd_in_path() {
        push_ui_log(
            ui_weak,
            "SCMD",
            format!(
                "Detected system SteamCMD in PATH: '{}'",
                install.executable.display()
            ),
            LogKind::Telemetry,
        );
        return Ok(install);
    }

    let bin_dir = resolve_moddrop_bin_dir()?;

    // Check if fully updated and provisioned already (steamclient64.dll or steamclient.dll present along with steamcmd.exe)
    if let Some(install) = find_provisioned_steamcmd(&bin_dir) {
        let has_steamclient = bin_dir.join("steamclient.dll").exists()
            || bin_dir.join("steamclient64.dll").exists()
            || bin_dir.join("linux32").join("steamclient.so").exists();

        if has_steamclient {
            let _ = ensure_executable_permissions(&bin_dir);
            push_ui_log(
                ui_weak,
                "SCMD",
                format!(
                    "Using provisioned portable SteamCMD at '{}'",
                    install.executable.display()
                ),
                LogKind::Telemetry,
            );
            return Ok(install);
        } else {
            // Found raw un-updated binary; run self-update
            let _ = ensure_executable_permissions(&bin_dir);
            run_initial_self_update(ui_weak, &install).await?;
            return Ok(install);
        }
    }

    // Create bin_dir and verify write permissions
    fs::create_dir_all(&bin_dir).map_err(|e| {
        format!(
            "Failed creating SteamCMD directory '{}' (check write permissions): {}",
            bin_dir.display(),
            e
        )
    })?;

    let download_url = if cfg!(target_os = "windows") {
        STEAMCMD_WIN_URL
    } else if cfg!(target_os = "macos") {
        STEAMCMD_MACOS_URL
    } else {
        STEAMCMD_LINUX_URL
    };

    push_ui_log(
        ui_weak,
        "BOOT",
        format!(
            "SteamCMD not found in PATH. Downloading official portable archive from '{}'...",
            download_url
        ),
        LogKind::Info,
    );
    update_ui_telemetry(ui_weak, Some("BOOTSTRAP"), Some("0.00 MB/s"), Some(0.10), None);

    let download_future = async {
        let response = client
            .get(download_url)
            .send()
            .await
            .map_err(|e| format!("Failed connecting to SteamCMD CDN: {}", e))?;

        if !response.status().is_success() {
            return Err(format!(
                "SteamCMD CDN returned HTTP status {}",
                response.status()
            ));
        }

        let total_bytes = response.content_length().unwrap_or(0);
        push_ui_log(
            ui_weak,
            "BOOT",
            format!(
                "Downloading SteamCMD package ({:.2} MB)...",
                total_bytes as f64 / (1024.0 * 1024.0)
            ),
            LogKind::Info,
        );

        let temp_dir = tempfile::Builder::new()
            .prefix("moddrop_steamcmd_boot_")
            .tempdir()
            .map_err(|e| format!("Failed creating temporary bootstrap folder: {}", e))?;

        let archive_name = if cfg!(target_os = "windows") {
            "steamcmd.zip"
        } else {
            "steamcmd.tar.gz"
        };
        let archive_path = temp_dir.path().join(archive_name);

        let mut async_file = tokio::fs::File::create(&archive_path)
            .await
            .map_err(|e| format!("Failed creating bootstrap archive file: {}", e))?;

        let mut stream = response.bytes_stream();
        let mut downloaded: u64 = 0;
        let start_time = Instant::now();
        let mut last_log_time = Instant::now();

        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res
                .map_err(|e| format!("Stream interrupted while downloading SteamCMD: {}", e))?;
            tokio::io::AsyncWriteExt::write_all(&mut async_file, &chunk)
                .await
                .map_err(|e| format!("Failed writing SteamCMD archive chunk: {}", e))?;
            downloaded += chunk.len() as u64;

            if last_log_time.elapsed() >= Duration::from_millis(400) {
                let pct = if total_bytes > 0 {
                    (downloaded as f32 / total_bytes as f32 * 100.0).clamp(0.0, 100.0)
                } else {
                    0.0
                };
                let elapsed_secs = start_time.elapsed().as_secs_f64().max(0.001);
                let vel = (downloaded as f64 / (1024.0 * 1024.0)) / elapsed_secs;
                let vel_str = format!("{:.2} MB/s", vel);
                let prog = 0.10 + 0.10 * (pct / 100.0);
                update_ui_telemetry(ui_weak, Some("BOOTSTRAP"), Some(&vel_str), Some(prog), None);
                last_log_time = Instant::now();
            }
        }
        tokio::io::AsyncWriteExt::flush(&mut async_file)
            .await
            .map_err(|e| format!("Failed flushing SteamCMD archive: {}", e))?;
        drop(async_file);

        push_ui_log(
            ui_weak,
            "BOOT",
            format!(
                "SteamCMD archive download complete ({:.2} MB). Extracting...",
                downloaded as f64 / (1024.0 * 1024.0)
            ),
            LogKind::Info,
        );

        // Extract archive on blocking thread
        let bin_dir_clone = bin_dir.clone();
        let archive_path_clone = archive_path.clone();
        let ui_weak_clone = ui_weak.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            if cfg!(target_os = "windows") {
                extract_steamcmd_zip(&ui_weak_clone, &archive_path_clone, &bin_dir_clone)?;
            } else {
                extract_steamcmd_tar_gz(&ui_weak_clone, &archive_path_clone, &bin_dir_clone)?;
            }
            ensure_executable_permissions(&bin_dir_clone)?;
            Ok(())
        })
        .await
        .map_err(|e| format!("SteamCMD extraction worker panicked: {}", e))??;

        Ok::<(), String>(())
    };

    tokio::time::timeout(Duration::from_secs(90), download_future)
        .await
        .map_err(|_| "Downloading and extracting SteamCMD timed out after 90 seconds.".to_string())??;

    let install = find_provisioned_steamcmd(&bin_dir).ok_or_else(|| {
        format!(
            "SteamCMD archive extracted to '{}', but executable was not found.",
            bin_dir.display()
        )
    })?;

    push_ui_log(
        ui_weak,
        "OK",
        format!(
            "Portable SteamCMD binary provisioned at '{}'.",
            install.executable.display()
        ),
        LogKind::Success,
    );

    // Run initial self-update cycle
    run_initial_self_update(ui_weak, &install).await?;

    Ok(install)
}

/// Evaluates whether a file or directory inside a SteamCMD workshop payload is Steam metadata
/// that should be stripped before deploying pure mod assets.
///
/// Rules:
/// - Any file ending in `.vdf` (case-insensitive) such as `workshop_item.vdf`, `manifest.vdf`, etc.
/// - Known Steam/depot metadata files (`steam_autocloud.vdf`, `.steam_workshop_manifest`, `appworkshop_*.acf`).
pub fn is_steam_metadata_file(file_name: &str) -> bool {
    let lower = file_name.trim().to_ascii_lowercase();
    if lower.ends_with(".vdf") || lower.ends_with(".acf") || lower.ends_with(".manifest") {
        return true;
    }
    matches!(
        lower.as_str(),
        "steam_autocloud.vdf" | ".ds_store" | "thumbs.db"
    )
}

/// Locates the Workshop content directory produced by SteamCMD:
/// Checks `<steamcmd_root>/steamapps/workshop/content/<AppID>/<ModID>` as well as platform-specific
/// secondary roots (e.g. `linux32/steamapps/...` or `~/.local/share/Steam/steamapps/...` on Linux),
/// and checks the staging downloads folder `<steamcmd_root>/steamapps/workshop/downloads/<AppID>/<ModID>`.
pub fn locate_workshop_content_dir(
    install: &SteamCmdInstall,
    app_id: &str,
    mod_id: &str,
) -> Option<PathBuf> {
    let rel_tail = Path::new("steamapps")
        .join("workshop")
        .join("content")
        .join(app_id)
        .join(mod_id);

    let rel_downloads = Path::new("steamapps")
        .join("workshop")
        .join("downloads")
        .join(app_id)
        .join(mod_id);

    #[cfg_attr(target_os = "windows", allow(unused_mut))]
    let mut candidates = vec![
        install.root_dir.join(&rel_tail),
        install.root_dir.join(&rel_downloads),
        install.root_dir.join("linux32").join(&rel_tail),
        install.root_dir.join("linux32").join(&rel_downloads),
    ];

    #[cfg(not(target_os = "windows"))]
    if let Ok(home) = env::var("HOME") {
        let home_path = PathBuf::from(home);
        candidates.push(home_path.join("Steam").join(&rel_tail));
        candidates.push(
            home_path
                .join(".local")
                .join("share")
                .join("Steam")
                .join(&rel_tail),
        );
        candidates.push(home_path.join(".steam").join("steam").join(&rel_tail));
    }

    candidates.into_iter().find(|p| p.is_dir())
}

/// Recursively copies pure mod assets from `src_dir` to `dest_dir`, stripping out `*.vdf` and
/// Steam metadata manifests, creating folders, and renaming mod IDs and vehicle/preview files to `custom_name`.
fn copy_and_strip_recursive(
    src_dir: &Path,
    dest_dir: &Path,
    mod_id: &str,
    custom_name: &str,
    report: &mut CleanDeployReport,
) -> Result<(), String> {
    if !dest_dir.exists() {
        fs::create_dir_all(dest_dir).map_err(|e| {
            format!(
                "Failed creating destination directory '{}' (check permissions): {}",
                dest_dir.display(),
                e
            )
        })?;
    }

    let has_vehicle_xml = src_dir.join("vehicle.xml").is_file();
    let entries = fs::read_dir(src_dir).map_err(|e| {
        format!(
            "Failed reading workshop content directory '{}': {}",
            src_dir.display(),
            e
        )
    })?;

    for entry_res in entries {
        let entry = entry_res
            .map_err(|e| format!("Error reading directory entry in '{}': {}", src_dir.display(), e))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("Failed inspecting file type: {}", e))?;

        // Never follow symlinks outside the workshop content tree
        if file_type.is_symlink() {
            continue;
        }

        let file_name_os = entry.file_name();
        let file_name = file_name_os.to_string_lossy();
        let src_path = entry.path();

        if file_type.is_dir() {
            let sub_name = if file_name == mod_id {
                custom_name.to_string()
            } else {
                file_name.to_string()
            };
            let target_subdir = dest_dir.join(&sub_name);
            copy_and_strip_recursive(&src_path, &target_subdir, mod_id, custom_name, report)?;
        } else if file_type.is_file() {
            if is_steam_metadata_file(&file_name) {
                // Delete metadata from staging folder and exclude from destination
                let _ = fs::remove_file(&src_path);
                report.stripped_files += 1;
                continue;
            }

            // Rename vehicle/preview files to custom name only if vehicle.xml is present (e.g. Stormworks)
            let renamed_file = if has_vehicle_xml && file_name.eq_ignore_ascii_case("vehicle.xml") {
                format!("{}.xml", custom_name)
            } else if has_vehicle_xml
                && (file_name.eq_ignore_ascii_case("workshop_preview.png")
                    || file_name.eq_ignore_ascii_case("preview.png"))
            {
                format!("{}.png", custom_name)
            } else if has_vehicle_xml && file_name.contains(mod_id) {
                file_name.replace(mod_id, custom_name)
            } else {
                file_name.to_string()
            };

            let target_file = dest_dir.join(&renamed_file);
            let bytes = fs::copy(&src_path, &target_file).map_err(|e| {
                format!(
                    "Failed copying asset '{}' to '{}' (check write permissions): {}",
                    src_path.display(),
                    target_file.display(),
                    e
                )
            })?;
            report.copied_files += 1;
            report.total_bytes += bytes;
        }
    }

    Ok(())
}

/// Verifies that the destination directory is writable by creating and removing a temporary probe file.
pub fn verify_directory_writable(dest_dir: &Path) -> Result<(), String> {
    let probe_path = dest_dir.join(".moddrop_write_probe.tmp");
    match File::create(&probe_path) {
        Ok(mut f) => {
            let _ = f.write_all(b"ok");
            drop(f);
            let _ = fs::remove_file(&probe_path);
            Ok(())
        }
        Err(e) => Err(format!(
            "Missing write permissions for destination folder '{}': {}",
            dest_dir.display(),
            e
        )),
    }
}

/// Runs `steamcmd +login anonymous +workshop_download_item <AppID> <ModID> +quit` asynchronously,
/// streams real-time stdout/stderr lines unbuffered to the Slint UI log console, strips Steam metadata (`*.vdf`),
/// creates a custom folder for the downloaded files, and renames mod IDs to `custom_name`.
pub async fn run_steamcmd_download_and_deploy(
    ui_weak: &Weak<AppWindow>,
    client: &Client,
    app_id: &str,
    mod_id: &str,
    dest_path: &Path,
    custom_name: &str,
) -> Result<CleanDeployReport, String> {
    if app_id.trim().is_empty() || app_id.trim() == "0" || !app_id.chars().all(|c| c.is_ascii_digit())
    {
        return Err(
            "Valid numeric Game ID (AppID, e.g., 573090 for Stormworks) is required for SteamCMD deployment."
                .to_string(),
        );
    }

    if mod_id.trim().is_empty() || !mod_id.chars().all(|c| c.is_ascii_digit()) {
        return Err("Valid numeric Workshop Mod ID (PublishedFileId) is required.".to_string());
    }

    // Verify write permission on the destination path up-front
    verify_directory_writable(dest_path)?;

    // 1. Auto-provision, verify, and complete self-update of SteamCMD
    let install = ensure_steamcmd(ui_weak, client).await?;

    const MAX_RETRIES: usize = 3;
    let mut last_error: Option<String> = None;
    let mut content_dir_opt: Option<PathBuf> = None;

    for attempt in 1..=MAX_RETRIES {
        // Clean any stale workshop item cache before downloading so we get a fresh state
        if let Some(existing_dir) = locate_workshop_content_dir(&install, app_id, mod_id) {
            let _ = fs::remove_dir_all(&existing_dir);
        }

        if attempt == 1 {
            push_ui_log(
                ui_weak,
                "SCMD",
                format!(
                    "Spawning: steamcmd +login anonymous +workshop_download_item {} {} +quit",
                    app_id, mod_id
                ),
                LogKind::Info,
            );
        } else {
            push_ui_log(
                ui_weak,
                "SCMD",
                format!(
                    "Retrying (attempt {}/{}): steamcmd +login anonymous +workshop_download_item {} {} +quit",
                    attempt, MAX_RETRIES, app_id, mod_id
                ),
                LogKind::Info,
            );
        }
        update_ui_telemetry(ui_weak, Some("STEAMCMD"), Some("0.00 MB/s"), Some(0.25), None);

        // 2. Execute the workshop download command with real-time streaming and delta speed tracking
        let (success_detected, error_opt) = run_steamcmd_process(
            ui_weak,
            &install,
            &["+login", "anonymous", "+workshop_download_item", app_id, mod_id, "+quit"],
            Some((app_id, mod_id)),
            Duration::from_secs(600),
        )
        .await?;

        content_dir_opt = locate_workshop_content_dir(&install, app_id, mod_id);

        if success_detected || content_dir_opt.is_some() {
            break;
        }

        if let Some(err) = error_opt {
            let friendly = explain_steamcmd_error(&err, app_id, mod_id);
            let lower = err.to_ascii_lowercase();

            // Permanent errors where retrying will not help
            if lower.contains("no subscription")
                || lower.contains("access denied")
                || lower.contains("invalid password")
            {
                last_error = Some(friendly);
                break;
            }

            if attempt < MAX_RETRIES {
                push_ui_log(
                    ui_weak,
                    "RETRY",
                    format!(
                        "Transient Steam communication issue encountered ({}). Retrying download (attempt {} of {})...",
                        friendly,
                        attempt + 1,
                        MAX_RETRIES
                    ),
                    LogKind::Telemetry,
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            last_error = Some(friendly);
        } else if attempt < MAX_RETRIES {
            push_ui_log(
                ui_weak,
                "RETRY",
                format!(
                    "Workshop content directory not created. Retrying download (attempt {} of {})...",
                    attempt + 1,
                    MAX_RETRIES
                ),
                LogKind::Telemetry,
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    let content_dir = match content_dir_opt {
        Some(dir) => dir,
        None => {
            if let Some(err) = last_error {
                return Err(err);
            }
            return Err(format!(
                "SteamCMD exited without producing workshop files at steamapps/workshop/content/{}/{}. Verify that AppID {} supports anonymous Workshop downloads.",
                app_id,
                mod_id,
                app_id
            ));
        }
    };

    push_ui_log(
        ui_weak,
        "DEPLOY",
        format!(
            "Deploying mod assets directly to '{}' as '{}.xml' and '{}.png'...",
            dest_path.display(),
            custom_name,
            custom_name
        ),
        LogKind::Info,
    );
    update_ui_telemetry(ui_weak, Some("DEPLOYING"), Some("0.00 MB/s"), Some(0.92), None);

    // 3. Strip metadata & deploy pure mod assets on blocking worker directly into dest_path
    let content_dir_clone = content_dir.clone();
    let dest_path_clone = dest_path.to_path_buf();
    let mod_id_owned = mod_id.to_string();
    let custom_name_owned = custom_name.to_string();

    let report = tokio::task::spawn_blocking(move || -> Result<CleanDeployReport, String> {
        let mut rep = CleanDeployReport::default();
        copy_and_strip_recursive(
            &content_dir_clone,
            &dest_path_clone,
            &mod_id_owned,
            &custom_name_owned,
            &mut rep,
        )?;
        // Clean up the downloaded workshop staging cache after successful deployment
        let _ = fs::remove_dir_all(&content_dir_clone);
        Ok(rep)
    })
    .await
    .map_err(|e| format!("Asset deployment task panicked: {}", e))??;

    Ok(report)
}

/// Cleans ANSI escape codes and control characters from SteamCMD terminal output.
fn sanitize_console_line(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Skip ANSI CSI sequence
            if chars.peek() == Some(&'[') {
                let _ = chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        if !ch.is_control() {
            out.push(ch);
        }
    }
    out.trim().to_string()
}

/// Translates raw SteamCMD errors into clear, actionable UX log messages.
fn explain_steamcmd_error(line: &str, app_id: &str, mod_id: &str) -> String {
    let lower = line.to_ascii_lowercase();
    if lower.contains("no subscription") || lower.contains("access denied") {
        format!(
            "{} — Steam anonymous login lacks subscription access to download Mod {} for AppID {} (game publisher restricts anonymous depot access).",
            line, mod_id, app_id
        )
    } else if lower.contains("timeout") {
        format!(
            "{} — Steam server timed out while processing AppID {} metadata or downloading Mod {}. (Steam network delay)",
            line, app_id, mod_id
        )
    } else if lower.contains("disk write failure") {
        format!(
            "{} — SteamCMD encountered a disk write failure (check free space and directory permissions).",
            line
        )
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_stripping_filter() {
        assert!(is_steam_metadata_file("workshop_item.vdf"));
        assert!(is_steam_metadata_file("MANIFEST.VDF"));
        assert!(is_steam_metadata_file("appworkshop_573090.acf"));
        assert!(is_steam_metadata_file("steam_autocloud.vdf"));
        assert!(!is_steam_metadata_file("vehicle.xml"));
        assert!(!is_steam_metadata_file("playlist.lua"));
        assert!(!is_steam_metadata_file("preview.png"));
    }

    #[test]
    fn test_copy_and_strip_removes_vdf_and_keeps_assets() {
        let src = tempfile::tempdir().expect("temp src");
        let dst = tempfile::tempdir().expect("temp dst");

        // Create pure mod assets and Steam metadata files
        fs::write(src.path().join("vehicle_data.xml"), b"<vehicle/>").unwrap();
        fs::write(src.path().join("workshop_meta.vdf"), b"\"workshop\"{}").unwrap();
        fs::write(src.path().join("appworkshop_573090.acf"), b"\"AppWorkshop\"{}").unwrap();

        let nested = src.path().join("meshes");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("hull.mesh"), b"mesh_binary").unwrap();
        fs::write(nested.join("depot_info.VDF"), b"vdf_meta").unwrap();

        let mut report = CleanDeployReport::default();
        copy_and_strip_recursive(src.path(), dst.path(), "573090", "CustomBoat", &mut report).expect("copy and strip");

        assert_eq!(report.copied_files, 2);
        assert_eq!(report.stripped_files, 3);
        assert!(dst.path().join("vehicle_data.xml").is_file());
        assert!(dst.path().join("meshes").join("hull.mesh").is_file());
        assert!(!dst.path().join("workshop_meta.vdf").exists());
        assert!(!dst.path().join("appworkshop_573090.acf").exists());
        assert!(!dst.path().join("meshes").join("depot_info.VDF").exists());
    }

    #[test]
    fn test_copy_and_strip_renames_two_downloaded_files_and_mod_id() {
        let src = tempfile::tempdir().expect("temp src");
        let dst = tempfile::tempdir().expect("temp dst");

        // Create the two downloaded files commonly found in Stormworks and games
        fs::write(src.path().join("vehicle.xml"), b"<vehicle/>").unwrap();
        fs::write(src.path().join("workshop_preview.png"), b"png_data").unwrap();
        fs::write(src.path().join("3771890254_manual.txt"), b"guide").unwrap();
        fs::write(src.path().join("workshop_item.vdf"), b"\"vdf\"").unwrap();

        let mut report = CleanDeployReport::default();
        copy_and_strip_recursive(src.path(), dst.path(), "3771890254", "Targa 46", &mut report)
            .expect("copy and strip");

        assert_eq!(report.copied_files, 3);
        assert_eq!(report.stripped_files, 1);
        assert!(dst.path().join("Targa 46.xml").is_file());
        assert!(dst.path().join("Targa 46.png").is_file());
        assert!(dst.path().join("Targa 46_manual.txt").is_file());
        assert!(!dst.path().join("vehicle.xml").exists());
        assert!(!dst.path().join("workshop_preview.png").exists());
        assert!(!dst.path().join("workshop_item.vdf").exists());
    }

    #[test]
    fn test_explain_steamcmd_error_timeout_and_permissions() {
        let err_timeout = explain_steamcmd_error(
            "ERROR! Info request for AppId 573090 returned error Timeout.",
            "573090",
            "3771890254",
        );
        assert!(err_timeout.contains("timed out"));
        assert!(err_timeout.contains("573090"));

        let err_sub = explain_steamcmd_error(
            "ERROR! Download item 12345 failed (No Subscription).",
            "573090",
            "12345",
        );
        assert!(err_sub.contains("subscription access"));
    }
}

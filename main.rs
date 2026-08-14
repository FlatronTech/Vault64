// MADE BY FLATRONIX :D //
#![windows_subsystem = "windows"]

use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{copy, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zip::ZipArchive;

const APP_VERSION: &str = "v3.0";

// Automatic updater's script
const PS_UPDATER_SCRIPT: &str = r#"
param(
    [Parameter(Mandatory=$true)][string]$Source,
    [Parameter(Mandatory=$true)][string]$Dest,
    [Parameter(Mandatory=$true)][string]$AppExe,
    [Parameter(Mandatory=$true)][string]$Cleanup,
    [Parameter(Mandatory=$true)][string]$Mode
)

$ErrorActionPreference = "Continue"
$log = Join-Path $env:TEMP "Vault64_updater.log"

function Log([string]$msg) {
    ("[{0}] {1}" -f (Get-Date -Format "HH:mm:ss"), $msg) | Out-File -FilePath $log -Append -Encoding utf8
}

Log "=== Vault64 updater started (Mode=$Mode) ==="
Log "Source: $Source"
Log "Dest:   $Dest"
Log "Exe:    $AppExe"

$exeBase = [System.IO.Path]::GetFileNameWithoutExtension($AppExe)

# 1) Wait for the launcher to close
$waited = 0
while ((Get-Process -Name $exeBase -ErrorAction SilentlyContinue) -and ($waited -lt 30)) {
    Start-Sleep -Milliseconds 500
    $waited += 0.5
}
Start-Sleep -Seconds 1
Log "App process closed (waited $waited s)"

# 2) Copy files
try {
    Copy-Item -Path (Join-Path $Source "*") -Destination $Dest -Recurse -Force -ErrorAction Stop
    Log "Copy OK"
} catch {
    Log "COPY FAILED: $($_.Exception.Message)"
    exit 1
}

# 3) delete old files
if ($Mode -eq "archive") {
    $protectedDirs = @("games", "config", "AppData", "logs")
    Get-ChildItem -Path $Dest -Recurse -File -Force -ErrorAction SilentlyContinue | ForEach-Object {
        $rel = $_.FullName.Substring($Dest.Length).Trim([System.IO.Path]::DirectorySeparatorChar)
        $parts = $rel.Split([System.IO.Path]::DirectorySeparatorChar)
        $isProtected = ($parts.Count -gt 1) -and ($protectedDirs -contains $parts[0])
        $inUpdate = Test-Path -LiteralPath (Join-Path $Source $rel)
        if ((-not $isProtected) -and (-not $inUpdate)) {
            Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue
        }
    }
    Log "Purge of old app files done"
}

# 4) Delete update temp folder
if (Test-Path -LiteralPath $Cleanup) {
    Remove-Item -LiteralPath $Cleanup -Recurse -Force -ErrorAction SilentlyContinue
    Log "Cleanup removed"
}

# 5) Launch the updated app
$newExe = Join-Path $Dest $AppExe
if (Test-Path -LiteralPath $newExe) {
    Start-Process -FilePath $newExe -WorkingDirectory $Dest
    Log "App restarted"
} else {
    Log "ERROR: $newExe not found after update!"
    exit 1
}

Log "=== Update finished OK ==="

# 6) Delete this script
Remove-Item -LiteralPath $PSCommandPath -Force -ErrorAction SilentlyContinue
exit 0
"#;

#[derive(Deserialize, Clone)]
struct GithubRelease {
    name: Option<String>,
    tag_name: String,
    body: Option<String>,
    assets: Vec<GithubAsset>,
    #[allow(dead_code)]
    html_url: Option<String>,
}

#[derive(Deserialize, Clone)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Default)]
enum AppTheme {
    #[default]
    Dark,
    Light,
    Neon,
}

#[derive(Serialize, Deserialize, Default)]
struct AppConfig {
    remembered_username: Option<String>,
    #[serde(default)]
    library: Vec<String>,
    #[serde(default)]
    theme: AppTheme,
}

/// Update plan given to powershell
struct UpdatePlan {
    script: PathBuf,
    source: PathBuf,
    dest: PathBuf,
    exe: String,
    cleanup: PathBuf,
    mode: String,
}

enum AppMsg {
    ReleasesFetched(Result<Vec<GithubRelease>, String>),
    AppUpdateFetched(Result<GithubRelease, String>),
    AppUpdateProgress(f32, String),
    AppUpdateReady(UpdatePlan),
    AppUpdateError(String),
    DownloadProgress(f32, String),
    DownloadComplete(String),
    DownloadError(String),
}

#[derive(PartialEq)]
enum AppState {
    Login,
    Launcher,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum LauncherTab {
    Store,
    Library,
    GameDetails,
    Settings,
}

struct UserDatabase;

impl UserDatabase {
    fn verify(username: &str, password: &str) -> bool {
        let users = vec![("guest", "1234")];
        users.iter().any(|(u, p)| *u == username && *p == password)
    }
}

struct Vault64App {
    state: AppState,
    tab: LauncherTab,
    previous_tab: LauncherTab,
    tab_switch_counter: u64,
    selected_release_idx: Option<usize>,

    login_username: String,
    login_password: String,
    remember_me: bool,
    login_error: Option<String>,

    config_dir: PathBuf,
    games_dir: PathBuf,
    config: AppConfig,

    releases: Option<Vec<GithubRelease>>,
    fetch_error: Option<String>,

    latest_app_release: Option<GithubRelease>,
    show_update_modal: bool,
    checking_update: bool,
    app_update_downloading: bool,
    app_update_progress: f32,
    app_update_status: String,
    app_update_info: String,

    search_query: String,

    tx: Sender<AppMsg>,
    rx: Receiver<AppMsg>,

    is_downloading: bool,
    download_progress: f32,
    download_status: String,
}

impl Vault64App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = unbounded();

        let proj_dirs = directories::ProjectDirs::from("com", "FlatronTech", "Vault64")
            .expect("Could not find suitable home directory");

        let config_dir = proj_dirs.config_dir().to_path_buf();
        let games_dir = proj_dirs.data_dir().join("games");

        fs::create_dir_all(&config_dir).ok();
        fs::create_dir_all(&games_dir).ok();

        let config_path = config_dir.join("config.json");
        let mut config = AppConfig::default();
        let mut state = AppState::Login;
        let mut login_username = String::new();
        let mut remember_me = false;

        if let Ok(file_content) = fs::read_to_string(&config_path) {
            if let Ok(parsed) = serde_json::from_str::<AppConfig>(&file_content) {
                config = parsed;
                if let Some(user) = &config.remembered_username {
                    login_username = user.clone();
                    remember_me = true;
                    state = AppState::Launcher;
                }
            }
        }

        Self::apply_theme(&config.theme, &cc.egui_ctx);

        let mut app = Self {
            state,
            tab: LauncherTab::Store,
            previous_tab: LauncherTab::Store,
            tab_switch_counter: 0,
            selected_release_idx: None,
            login_username,
            login_password: String::new(),
            remember_me,
            login_error: None,
            config_dir,
            games_dir,
            config,
            releases: None,
            fetch_error: None,
            latest_app_release: None,
            show_update_modal: false,
            checking_update: false,
            app_update_downloading: false,
            app_update_progress: 0.0,
            app_update_status: String::new(),
            app_update_info: String::new(),
            search_query: String::new(),
            tx,
            rx,
            is_downloading: false,
            download_progress: 0.0,
            download_status: String::new(),
        };

        if app.state == AppState::Launcher {
            app.fetch_releases();
            app.check_app_updates();
        }

        app
    }

    fn apply_theme(theme: &AppTheme, ctx: &egui::Context) {
        let mut visuals = match theme {
            AppTheme::Dark => {
                let mut v = egui::Visuals::dark();
                v.panel_fill = egui::Color32::from_rgb(14, 15, 20);
                v.window_fill = egui::Color32::from_rgb(18, 19, 25);
                v.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(24, 26, 34);
                v.widgets.inactive.bg_fill = egui::Color32::from_rgb(34, 37, 48);
                v.widgets.hovered.bg_fill = egui::Color32::from_rgb(48, 52, 68);
                v.widgets.active.bg_fill = egui::Color32::from_rgb(0, 120, 255);
                v.selection.bg_fill = egui::Color32::from_rgb(0, 120, 255);
                v
            }
            AppTheme::Light => {
                let mut v = egui::Visuals::light();
                v.panel_fill = egui::Color32::from_rgb(236, 240, 245);
                v.window_fill = egui::Color32::from_rgb(255, 255, 255);
                v.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(255, 255, 255);
                v.widgets.inactive.bg_fill = egui::Color32::from_rgb(225, 230, 236);
                v.widgets.hovered.bg_fill = egui::Color32::from_rgb(205, 215, 228);
                v.widgets.active.bg_fill = egui::Color32::from_rgb(0, 102, 220);
                v.selection.bg_fill = egui::Color32::from_rgb(0, 120, 255);
                v
            }
            AppTheme::Neon => {
                let mut v = egui::Visuals::dark();
                v.panel_fill = egui::Color32::from_rgb(9, 4, 16);
                v.window_fill = egui::Color32::from_rgb(14, 7, 24);
                v.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(22, 10, 38);
                v.widgets.inactive.bg_fill = egui::Color32::from_rgb(38, 16, 62);
                v.widgets.hovered.bg_fill = egui::Color32::from_rgb(64, 24, 104);
                v.widgets.active.bg_fill = egui::Color32::from_rgb(255, 0, 128);
                v.selection.bg_fill = egui::Color32::from_rgb(255, 0, 128);
                v
            }
        };

        visuals.window_rounding = egui::Rounding::same(16.0);
        visuals.menu_rounding = egui::Rounding::same(12.0);
        visuals.widgets.noninteractive.rounding = egui::Rounding::same(10.0);
        visuals.widgets.inactive.rounding = egui::Rounding::same(10.0);
        visuals.widgets.hovered.rounding = egui::Rounding::same(10.0);
        visuals.widgets.active.rounding = egui::Rounding::same(10.0);

        ctx.set_visuals(visuals);
    }

    fn is_dark(&self) -> bool {
        self.config.theme != AppTheme::Light
    }

    fn accent(&self) -> egui::Color32 {
        match self.config.theme {
            AppTheme::Dark => egui::Color32::from_rgb(80, 160, 255),
            AppTheme::Light => egui::Color32::from_rgb(0, 102, 220),
            AppTheme::Neon => egui::Color32::from_rgb(255, 0, 128),
        }
    }

    fn text_color(&self) -> egui::Color32 {
        if self.is_dark() {
            egui::Color32::from_rgb(240, 242, 248)
        } else {
            egui::Color32::from_rgb(18, 20, 26)
        }
    }

    fn subtle_text(&self) -> egui::Color32 {
        if self.is_dark() {
            egui::Color32::from_rgb(145, 150, 165)
        } else {
            egui::Color32::from_rgb(90, 95, 110)
        }
    }

    fn panel_bg(&self) -> egui::Color32 {
        if self.is_dark() {
            egui::Color32::from_rgb(13, 14, 19)
        } else {
            egui::Color32::from_rgb(228, 232, 238)
        }
    }

    fn card_bg(&self) -> egui::Color32 {
        if self.is_dark() {
            egui::Color32::from_rgb(26, 28, 36)
        } else {
            egui::Color32::WHITE
        }
    }

    fn card_hover_bg(&self) -> egui::Color32 {
        if self.is_dark() {
            egui::Color32::from_rgb(35, 39, 52)
        } else {
            egui::Color32::from_rgb(238, 244, 252)
        }
    }

    fn success_color(&self) -> egui::Color32 {
        egui::Color32::from_rgb(80, 200, 120)
    }

    fn danger_color(&self) -> egui::Color32 {
        egui::Color32::from_rgb(255, 90, 90)
    }

    fn fade(color: egui::Color32, alpha: f32) -> egui::Color32 {
        color.linear_multiply(alpha.clamp(0.0, 1.0))
    }

    fn mix_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
        let t = t.clamp(0.0, 1.0);
        egui::Color32::from_rgb(
            (a.r() as f32 * (1.0 - t) + b.r() as f32 * t) as u8,
            (a.g() as f32 * (1.0 - t) + b.g() as f32 * t) as u8,
            (a.b() as f32 * (1.0 - t) + b.b() as f32 * t) as u8,
        )
    }

    fn safe_name(name: &str) -> String {
        name.replace(
            |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != ' ',
            "_",
        )
    }

    fn color_from_hue(hue: f32, sat: f32, val: f32) -> egui::Color32 {
        let h = hue.clamp(0.0, 1.0) * 6.0;
        let i = h.floor() as i32 % 6;
        let f = h - h.floor();

        let p = val * (1.0 - sat);
        let q = val * (1.0 - f * sat);
        let t = val * (1.0 - (1.0 - f) * sat);

        let (r, g, b) = match i {
            0 => (val, t, p),
            1 => (q, val, p),
            2 => (p, val, t),
            3 => (p, q, val),
            4 => (t, p, val),
            _ => (val, p, q),
        };

        egui::Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
    }

    fn hash_color(name: &str) -> egui::Color32 {
        let mut hash: u32 = 0;
        for b in name.bytes() {
            hash = hash.wrapping_mul(31).wrapping_add(b as u32);
        }
        let hue = (hash % 360) as f32 / 360.0;
        Self::color_from_hue(hue, 0.42, 0.92)
    }

    fn version_tuple(v: &str) -> (u32, u32, u32) {
        let cleaned = v.trim().trim_start_matches('v');
        let mut parts = cleaned.split('.');

        let major = parts
            .next()
            .and_then(|s| s.split('-').next())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);

        let minor = parts
            .next()
            .and_then(|s| s.split('-').next())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);

        let patch = parts
            .next()
            .and_then(|s| s.split('-').next())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);

        (major, minor, patch)
    }

    fn is_newer_version(latest: &str, current: &str) -> bool {
        let l = Self::version_tuple(latest);
        let c = Self::version_tuple(current);

        if l != (0, 0, 0) || c != (0, 0, 0) {
            l > c
        } else {
            latest.trim_start_matches('v') != current.trim_start_matches('v')
        }
    }

    fn find_update_asset(release: &GithubRelease) -> Option<GithubAsset> {
        release
            .assets
            .iter()
            .find(|a| a.name.to_lowercase().ends_with(".7z"))
            .cloned()
            .or_else(|| {
                release
                    .assets
                    .iter()
                    .find(|a| a.name.to_lowercase().ends_with(".zip"))
                    .cloned()
            })
            .or_else(|| {
                release
                    .assets
                    .iter()
                    .find(|a| a.name.to_lowercase().ends_with(".exe"))
                    .cloned()
            })
    }

    fn matches_release(release: &GithubRelease, query: &str) -> bool {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return true;
        }

        let name = release
            .name
            .clone()
            .unwrap_or_else(|| release.tag_name.clone())
            .to_lowercase();

        let body = release.body.clone().unwrap_or_default().to_lowercase();

        name.contains(&q) || release.tag_name.to_lowercase().contains(&q) || body.contains(&q)
    }

    fn change_tab(&mut self, new_tab: LauncherTab) {
        if self.tab != new_tab {
            self.previous_tab = self.tab;
            self.tab = new_tab;
            self.tab_switch_counter += 1;
        }
    }

    fn save_config(&self) {
        let config_path = self.config_dir.join("config.json");
        if let Ok(json) = serde_json::to_string_pretty(&self.config) {
            fs::write(config_path, json).ok();
        }
    }

    fn check_app_updates(&mut self) {
        if self.checking_update {
            return;
        }

        self.checking_update = true;
        self.app_update_info = "Checking for updates...".to_string();

        let tx = self.tx.clone();
        thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .user_agent("Vault64-Launcher")
                .build()
                .unwrap();

            let url = "https://api.github.com/repos/FlatronTech/Vault64/releases/latest";

            match client.get(url).send() {
                Ok(response) => {
                    if response.status().is_success() {
                        match response.json::<GithubRelease>() {
                            Ok(release) => {
                                let _ = tx.send(AppMsg::AppUpdateFetched(Ok(release)));
                            }
                            Err(e) => {
                                let _ = tx.send(AppMsg::AppUpdateFetched(Err(e.to_string())));
                            }
                        }
                    } else {
                        let _ = tx.send(AppMsg::AppUpdateFetched(Err(format!(
                            "Status: {}",
                            response.status()
                        ))));
                    }
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::AppUpdateFetched(Err(e.to_string())));
                }
            }
        });
    }

    fn download_app_update(&mut self, asset: GithubAsset) {
        self.app_update_downloading = true;
        self.app_update_progress = 0.0;
        self.app_update_status = "Preparing update...".to_string();

        let tx = self.tx.clone();

        thread::spawn(move || {
            match Self::download_and_prepare_update(&asset, &tx) {
                Ok(plan) => {
                    let _ = tx.send(AppMsg::AppUpdateReady(plan));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::AppUpdateError(e));
                }
            }
        });
    }

    // downloads the package
    fn download_and_prepare_update(
        asset: &GithubAsset,
        tx: &Sender<AppMsg>,
    ) -> Result<UpdatePlan, String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);

        let work_dir =
            std::env::temp_dir().join(format!("Vault64_update_{}_{}", std::process::id(), stamp));

        fs::create_dir_all(&work_dir)
            .map_err(|e| format!("Could not create temporary update folder: {}", e))?;

        let ext = Path::new(&asset.name)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();

        let exe_path = std::env::current_exe().map_err(|e| e.to_string())?;
        let dest_dir = exe_path
            .parent()
            .ok_or("Could not find program directory")?
            .to_path_buf();

        let exe_name = exe_path
            .file_name()
            .ok_or("Could not find executable name")?
            .to_string_lossy()
            .to_string();

        // script goes to the temp folder so its not removed halfway
        let script_path = std::env::temp_dir().join(format!(
            "Vault64_updater_{}_{}.ps1",
            std::process::id(),
            stamp
        ));

        fs::write(&script_path, PS_UPDATER_SCRIPT)
            .map_err(|e| format!("Could not write updater script: {}", e))?;

        if ext == "exe" {
            let update_file = work_dir.join("Vault64_update.exe");

            Self::download_to_file(
                &asset.browser_download_url,
                &update_file,
                asset.size,
                tx,
                0.0,
                0.85,
                "Downloading update",
            )?;

            
            let final_exe = work_dir.join(&exe_name);
            let _ = fs::remove_file(&final_exe);
            fs::rename(&update_file, &final_exe)
                .map_err(|e| format!("Could not prepare update executable: {}", e))?;

            let _ = tx.send(AppMsg::AppUpdateProgress(
                0.98,
                "Restarting...".into(),
            ));

            return Ok(UpdatePlan {
                script: script_path,
                source: work_dir.clone(),
                dest: dest_dir,
                exe: exe_name,
                cleanup: work_dir,
                mode: "exe".to_string(),
            });
        }

        if ext == "7z" || ext == "zip" {
            let archive_path = work_dir.join(format!("update.{}", ext));

            Self::download_to_file(
                &asset.browser_download_url,
                &archive_path,
                asset.size,
                tx,
                0.0,
                0.55,
                "Downloading update package",
            )?;

            let extract_dir = work_dir.join("extracted");
            fs::create_dir_all(&extract_dir)
                .map_err(|e| format!("Could not create extraction folder: {}", e))?;

            let _ = tx.send(AppMsg::AppUpdateProgress(
                0.60,
                "Extracting update files...".into(),
            ));

            match ext.as_str() {
                "7z" => {
                    sevenz_rust::decompress_file(&archive_path, &extract_dir)
                        .map_err(|e| format!("7z extraction failed: {}", e))?;
                }
                "zip" => {
                    Self::extract_zip(&archive_path, &extract_dir)?;
                }
                _ => {
                    return Err("Unsupported archive format".into());
                }
            }

            let source_root = Self::resolve_extract_root(&extract_dir);

            if !source_root.exists() {
                return Err("Extracted update folder was not found".into());
            }

            let mut entries = fs::read_dir(&source_root)
                .map_err(|e| format!("Could not read extracted update folder: {}", e))?;

            if entries.next().is_none() {
                return Err("Update archive is empty".into());
            }

            let _ = tx.send(AppMsg::AppUpdateProgress(
                0.90,
                "Preparing updater's script...".into(),
            ));

            let _ = tx.send(AppMsg::AppUpdateProgress(
                0.98,
                "Restarting...".into(),
            ));

            return Ok(UpdatePlan {
                script: script_path,
                source: source_root,
                dest: dest_dir,
                exe: exe_name,
                cleanup: work_dir,
                mode: "archive".to_string(),
            });
        }

        Err("No supported update package found. Expected .7z or .zip".into())
    }

    fn download_to_file(
        url: &str,
        path: &Path,
        expected_size: u64,
        tx: &Sender<AppMsg>,
        start: f32,
        end: f32,
        label: &str,
    ) -> Result<(), String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("Vault64-Launcher")
            .build()
            .map_err(|e| e.to_string())?;

        let mut response = client.get(url).send().map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            return Err(format!("HTTP error: {}", response.status()));
        }

        let mut file = File::create(path).map_err(|e| format!("Could not create file: {}", e))?;

        let total = if expected_size == 0 {
            1.0
        } else {
            expected_size as f32
        };

        let mut downloaded = 0u64;
        let mut buffer = [0u8; 65536];

        loop {
            match response.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    file.write_all(&buffer[..n])
                        .map_err(|e| format!("Write error: {}", e))?;

                    downloaded += n as u64;

                    let fraction = if expected_size == 0 {
                        0.5
                    } else {
                        (downloaded as f32 / total).clamp(0.0, 1.0)
                    };

                    let progress = start + fraction * (end - start);

                    let status = if expected_size == 0 {
                        format!("{}: {:.2} MB", label, downloaded as f64 / 1_048_576.0)
                    } else {
                        format!(
                            "{}: {:.2} MB / {:.2} MB",
                            label,
                            downloaded as f64 / 1_048_576.0,
                            expected_size as f64 / 1_048_576.0
                        )
                    };

                    let _ = tx.send(AppMsg::AppUpdateProgress(progress, status));
                }
                Err(e) => return Err(format!("Connection interrupted: {}", e)),
            }
        }

        file.flush().ok();

        if expected_size > 0 && downloaded < expected_size {
            return Err("Download was interrupted before completion".into());
        }

        Ok(())
    }

    fn extract_zip(path: &Path, dest: &Path) -> Result<(), String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mut archive = ZipArchive::new(file).map_err(|e| e.to_string())?;

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;

            let outpath = match entry.enclosed_name() {
                Some(p) => dest.join(p),
                None => continue,
            };

            let is_dir = entry.name().ends_with('/') || entry.name().ends_with('\\');

            if is_dir {
                fs::create_dir_all(&outpath).ok();
            } else {
                if let Some(parent) = outpath.parent() {
                    fs::create_dir_all(parent).ok();
                }

                let mut out = File::create(&outpath)
                    .map_err(|e| format!("Could not create extracted file: {}", e))?;

                copy(&mut entry, &mut out)
                    .map_err(|e| format!("Could not extract file contents: {}", e))?;
            }
        }

        Ok(())
    }

    fn resolve_extract_root(dir: &Path) -> PathBuf {
        if let Ok(entries) = fs::read_dir(dir) {
            let entries: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect();

            if entries.len() == 1 && entries[0].is_dir() {
                return entries[0].clone();
            }
        }

        dir.to_path_buf()
    }

    // Powershell arguments
    fn run_updater_and_exit(plan: UpdatePlan) {
        let _ = Command::new("powershell")
            .arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-WindowStyle")
            .arg("Hidden")
            .arg("-File")
            .arg(&plan.script)
            .arg("-Source")
            .arg(&plan.source)
            .arg("-Dest")
            .arg(&plan.dest)
            .arg("-AppExe")
            .arg(&plan.exe)
            .arg("-Cleanup")
            .arg(&plan.cleanup)
            .arg("-Mode")
            .arg(&plan.mode)
            .spawn();

        std::process::exit(0);
    }

    fn fetch_releases(&mut self) {
        let tx = self.tx.clone();

        thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .user_agent("Vault64-Launcher")
                .build()
                .unwrap();

            let url = "https://api.github.com/repos/FlatronTech/Vault64-games/releases";

            match client.get(url).send() {
                Ok(response) => {
                    if response.status().is_success() {
                        match response.json::<Vec<GithubRelease>>() {
                            Ok(releases) => {
                                let _ = tx.send(AppMsg::ReleasesFetched(Ok(releases)));
                            }
                            Err(e) => {
                                let _ = tx.send(AppMsg::ReleasesFetched(Err(e.to_string())));
                            }
                        }
                    } else {
                        let _ = tx.send(AppMsg::ReleasesFetched(Err(format!(
                            "API error: {}",
                            response.status()
                        ))));
                    }
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::ReleasesFetched(Err(e.to_string())));
                }
            }
        });
    }

    fn find_executable(dir: &Path) -> Option<PathBuf> {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();

                if path.is_dir() {
                    if let Some(exe) = Self::find_executable(&path) {
                        return Some(exe);
                    }
                } else if path.is_file() {
                    if let Some(ext) = path.extension() {
                        if ext.to_string_lossy().to_lowercase() == "exe" {
                            return Some(path);
                        }
                    }
                }
            }
        }

        None
    }

    fn download_and_install(&mut self, asset: GithubAsset, game_name: String) {
        self.is_downloading = true;
        self.download_progress = 0.0;
        self.download_status = "Starting download...".to_string();

        let tx = self.tx.clone();
        let safe_game_name = Self::safe_name(&game_name);

        let target_dir = self.games_dir.join(&safe_game_name);
        let temp_dir = self.games_dir.join(format!("{}_temp", safe_game_name));
        let archive_path = self.games_dir.join(format!("{}.7z", safe_game_name));

        thread::spawn(move || {
            let _ = fs::remove_dir_all(&temp_dir);
            let _ = fs::remove_file(&archive_path);

            if let Err(e) = fs::create_dir_all(&temp_dir) {
                let _ = tx.send(AppMsg::DownloadError(format!(
                    "Cannot create temporary folder: {}",
                    e
                )));
                return;
            }

            let _ = tx.send(AppMsg::DownloadProgress(0.01, "Connecting...".into()));

            let client = reqwest::blocking::Client::builder()
                .user_agent("Vault64-Launcher")
                .build()
                .unwrap();

            let mut response = match client.get(&asset.browser_download_url).send() {
                Ok(res) => res,
                Err(e) => {
                    let _ = tx.send(AppMsg::DownloadError(format!("Connection error: {}", e)));
                    return;
                }
            };

            if !response.status().is_success() {
                let _ = tx.send(AppMsg::DownloadError(format!(
                    "HTTP error: {}",
                    response.status()
                )));
                return;
            }

            let mut file = match File::create(&archive_path) {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(AppMsg::DownloadError(format!(
                        "Error saving file to disk: {}",
                        e
                    )));
                    return;
                }
            };

            let total_size = asset.size as f32;
            let mut downloaded = 0.0_f32;
            let mut buffer = [0u8; 32768];

            loop {
                match response.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Err(e) = file.write_all(&buffer[..n]) {
                            let _ = tx.send(AppMsg::DownloadError(format!(
                                "Error saving file to disk: {}",
                                e
                            )));
                            return;
                        }

                        downloaded += n as f32;
                        let progress = (downloaded / total_size).clamp(0.0, 1.0) * 0.6;

                        let _ = tx.send(AppMsg::DownloadProgress(
                            progress,
                            format!(
                                "Downloading: {:.2} MB / {:.2} MB",
                                downloaded / 1_048_576.0,
                                total_size / 1_048_576.0
                            ),
                        ));
                    }
                    Err(e) => {
                        let _ = tx.send(AppMsg::DownloadError(format!(
                            "Connection interrupted: {}",
                            e
                        )));
                        return;
                    }
                }
            }

            if (downloaded as u64) < asset.size {
                let _ = tx.send(AppMsg::DownloadError(
                    "Download was interrupted. File is probably corrupted.".to_string(),
                ));
                return;
            }

            drop(file);

            let _ = tx.send(AppMsg::DownloadProgress(
                0.70,
                "Unpacking files... this may take a while, so sit back and relax :P".into(),
            ));

            if let Err(e) = sevenz_rust::decompress_file(&archive_path, &temp_dir) {
                let _ = tx.send(AppMsg::DownloadError(format!(
                    "Critical error while decompressing .7z file: {}",
                    e
                )));
                return;
            }

            let _ = tx.send(AppMsg::DownloadProgress(0.95, "Finalizing installation...".into()));

            if target_dir.exists() {
                if let Err(e) = fs::remove_dir_all(&target_dir) {
                    let _ = tx.send(AppMsg::DownloadError(format!(
                        "Could not remove old installation: {}",
                        e
                    )));
                    return;
                }
            }

            if let Err(e) = fs::rename(&temp_dir, &target_dir) {
                let _ = tx.send(AppMsg::DownloadError(format!(
                    "Error finalizing installation: {}",
                    e
                )));
                return;
            }

            if let Err(e) = fs::remove_file(&archive_path) {
                println!("Warning: failed to remove archive file: {}", e);
            }

            let _ = tx.send(AppMsg::DownloadComplete(safe_game_name));
        });
    }

    fn open_path(path: &Path) {
        #[cfg(target_os = "windows")]
        {
            let _ = Command::new("explorer").arg(path).spawn();
        }

        #[cfg(target_os = "macos")]
        {
            let _ = Command::new("open").arg(path).spawn();
        }

        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let _ = Command::new("xdg-open").arg(path).spawn();
        }
    }

    fn launch_game(&mut self, target_dir: &Path) {
        if let Some(exe_path) = Self::find_executable(target_dir) {
            match Command::new(&exe_path)
                .current_dir(exe_path.parent().unwrap_or(target_dir))
                .spawn()
            {
                Ok(_) => {
                    self.download_status = "".to_string();
                }
                Err(e) => {
                    self.download_status = format!("Error launching game: {}", e);
                }
            }
        } else {
            self.download_status = "No executable found in game folder".to_string();
        }
    }

    fn process_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                AppMsg::ReleasesFetched(Ok(data)) => {
                    self.releases = Some(data);
                    self.fetch_error = None;
                }
                AppMsg::ReleasesFetched(Err(e)) => {
                    self.fetch_error = Some(e);
                }
                AppMsg::AppUpdateFetched(Ok(release)) => {
                    self.checking_update = false;

                    if Self::is_newer_version(&release.tag_name, APP_VERSION) {
                        self.app_update_info =
                            format!("Version {} is available.", release.tag_name);
                        self.latest_app_release = Some(release);
                        self.show_update_modal = true;
                    } else {
                        self.latest_app_release = None;
                        self.app_update_info = "Launcher is up to date.".into();
                    }
                }
                AppMsg::AppUpdateFetched(Err(e)) => {
                    self.checking_update = false;
                    self.app_update_info = format!("Update check failed: {}", e);
                }
                AppMsg::AppUpdateProgress(progress, status) => {
                    self.app_update_progress = progress;
                    self.app_update_status = status;
                }
                AppMsg::AppUpdateReady(plan) => {
                    Self::run_updater_and_exit(plan);
                }
                AppMsg::AppUpdateError(err) => {
                    self.app_update_downloading = false;
                    self.app_update_status = format!("Update error: {}", err);
                    self.app_update_info = format!("❌ {}", err);
                }
                AppMsg::DownloadProgress(progress, status) => {
                    self.download_progress = progress;
                    self.download_status = status;
                }
                AppMsg::DownloadComplete(name) => {
                    self.is_downloading = false;
                    self.download_progress = 1.0;
                    self.download_status = format!("{}", name);
                }
                AppMsg::DownloadError(err) => {
                    self.is_downloading = false;
                    self.download_status = format!("Error: {}", err);
                }
            }
        }
    }

    fn ui_login(&mut self, ctx: &egui::Context) {
        let alpha = ctx.animate_value_with_time(egui::Id::new("login_fade_in"), 1.0, 0.45);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() / 5.0);

                let bg = self.card_bg();
                let accent = self.accent();

                egui::Frame::none()
                    .fill(Self::fade(bg, alpha))
                    .rounding(24.0)
                    .inner_margin(40.0)
                    .shadow(egui::epaint::Shadow {
                        offset: egui::vec2(0.0, 10.0),
                        blur: 30.0,
                        spread: 0.0,
                        color: egui::Color32::from_black_alpha(if self.is_dark() { 120 } else { 30 }),
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(300.0);

                        ui.label(egui::RichText::new("🎮").size(56.0));
                        ui.heading(
                            egui::RichText::new("Vault64")
                                .size(44.0)
                                .strong()
                                .color(Self::fade(self.text_color(), alpha)),
                        );
                        ui.label(
                            egui::RichText::new("Your game launcher")
                                .color(Self::fade(self.subtle_text(), alpha)),
                        );

                        ui.add_space(24.0);

                        let username_edit = egui::TextEdit::singleline(&mut self.login_username)
                            .hint_text("Username (guest)")
                            .desired_width(f32::INFINITY)
                            .margin(egui::vec2(12.0, 10.0));

                        ui.add(username_edit);

                        ui.add_space(10.0);

                        let pass_edit = egui::TextEdit::singleline(&mut self.login_password)
                            .password(true)
                            .hint_text("Password (1234)")
                            .desired_width(f32::INFINITY)
                            .margin(egui::vec2(12.0, 10.0));

                        let pass_response = ui.add(pass_edit);

                        ui.add_space(12.0);
                        ui.checkbox(&mut self.remember_me, "Remember me");

                        if let Some(err) = &self.login_error {
                            ui.add_space(8.0);
                            ui.colored_label(self.danger_color(), err);
                        }

                        ui.add_space(16.0);

                        let mut do_login = false;

                        if ui
                            .add_sized(
                                [ui.available_width(), 46.0],
                                egui::Button::new(
                                    egui::RichText::new("L O G I N")
                                        .strong()
                                        .size(16.0)
                                        .color(egui::Color32::WHITE),
                                )
                                .fill(accent),
                            )
                            .clicked()
                        {
                            do_login = true;
                        }

                        if pass_response.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            do_login = true;
                        }

                        if do_login {
                            if UserDatabase::verify(&self.login_username, &self.login_password) {
                                self.login_error = None;

                                if self.remember_me {
                                    self.config.remembered_username =
                                        Some(self.login_username.clone());
                                } else {
                                    self.config.remembered_username = None;
                                }

                                self.save_config();
                                self.state = AppState::Launcher;
                                self.tab_switch_counter += 1;
                                self.fetch_releases();
                                self.check_app_updates();
                            } else {
                                self.login_error =
                                    Some("Invalid username or password".to_string());
                            }
                        }
                    });
            });
        });
    }

    fn ui_launcher(&mut self, ctx: &egui::Context) {
        let mut app_update_to_download = None;
        self.update_modal(ctx, &mut app_update_to_download);

        if let Some(asset) = app_update_to_download {
            self.download_app_update(asset);
        }

        let top_bg = self.panel_bg();

        egui::TopBottomPanel::top("top_panel")
            .frame(egui::Frame::none().fill(top_bg).inner_margin(12.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Vault64")
                            .strong()
                            .size(24.0)
                            .color(self.text_color()),
                    );

                    ui.add_space(16.0);

                    if self.tab == LauncherTab::GameDetails || self.tab == LauncherTab::Settings {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("↩ Back")
                                        .strong()
                                        .color(self.text_color()),
                                )
                                .fill(egui::Color32::TRANSPARENT),
                            )
                            .clicked()
                        {
                            self.change_tab(self.previous_tab);
                        }

                        if self.tab == LauncherTab::Settings {
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("⚙ Settings")
                                    .size(18.0)
                                    .strong()
                                    .color(self.text_color()),
                            );
                        }
                    } else {
                        if self
                            .top_tab_button(ui, ctx, "🏪 Store", self.tab == LauncherTab::Store)
                            .clicked()
                        {
                            self.change_tab(LauncherTab::Store);
                        }

                        ui.add_space(8.0);

                        if self
                            .top_tab_button(
                                ui,
                                ctx,
                                "📚 Library",
                                self.tab == LauncherTab::Library,
                            )
                            .clicked()
                        {
                            self.change_tab(LauncherTab::Library);
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("🚪 Logout").color(self.subtle_text()),
                                )
                                .fill(egui::Color32::TRANSPARENT),
                            )
                            .on_hover_text("Logout")
                            .clicked()
                        {
                            self.config.remembered_username = None;
                            self.save_config();
                            self.state = AppState::Login;
                            self.tab = LauncherTab::Store;
                            self.tab_switch_counter += 1;
                            self.login_password.clear();
                        }

                        ui.add_space(6.0);

                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("⚙ Settings").color(self.subtle_text()),
                                )
                                .fill(egui::Color32::TRANSPARENT),
                            )
                            .on_hover_text("Settings")
                            .clicked()
                        {
                            if self.tab != LauncherTab::Settings {
                                self.change_tab(LauncherTab::Settings);
                            }
                        }

                        ui.add_space(8.0);

                        ui.label(
                            egui::RichText::new(format!("👤 {}", self.login_username))
                                .color(self.subtle_text()),
                        );
                    });
                });
            });

        egui::TopBottomPanel::bottom("bottom_panel")
            .frame(
                egui::Frame::none()
                    .fill(if self.is_dark() {
                        egui::Color32::from_rgb(11, 12, 16)
                    } else {
                        egui::Color32::from_rgb(218, 224, 230)
                    })
                    .inner_margin(10.0),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if self.app_update_downloading {
                        ui.label("↻ Launcher update:");
                        ui.add_sized(
                            [260.0, 18.0],
                            egui::ProgressBar::new(self.app_update_progress)
                                .show_percentage()
                                .animate(true),
                        );
                        ui.add_space(8.0);
                        ui.label(&self.app_update_status);
                    } else if self.is_downloading {
                        ui.label("⬇ Installing:");
                        ui.add_sized(
                            [320.0, 18.0],
                            egui::ProgressBar::new(self.download_progress)
                                .show_percentage()
                                .animate(true),
                        );
                        ui.add_space(8.0);
                        ui.label(&self.download_status);
                    } else if !self.download_status.is_empty() {
                        let color = if self.download_status.starts_with("Error") {
                            self.danger_color()
                        } else {
                            self.success_color()
                        };

                        ui.label(egui::RichText::new(&self.download_status).color(color));
                    } else {
                        ui.label(egui::RichText::new("").color(self.subtle_text()));
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!("{}", APP_VERSION))
                                .color(self.subtle_text()),
                        );
                    });
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let page_anim_id = egui::Id::new("page_transition_anim");
            let anim = ctx.animate_value_with_time(page_anim_id, 1.0, 0.28);

            let offset = (1.0 - anim) * 24.0;

            let rect = ui.available_rect_before_wrap();
            let mut draw_rect = rect;
            draw_rect.min.y += offset;
            draw_rect.max.y += offset;

            let mut page_ui = ui.child_ui(draw_rect, *ui.layout(), None);
            page_ui.set_clip_rect(draw_rect);

            match self.tab {
                LauncherTab::Store => self.ui_store_view(ctx, &mut page_ui),
                LauncherTab::Library => self.ui_library_view(ctx, &mut page_ui),
                LauncherTab::GameDetails => self.ui_game_details_view(ctx, &mut page_ui),
                LauncherTab::Settings => self.ui_settings_view(ctx, &mut page_ui),
            }
        });
    }

    fn top_tab_button(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        label: &str,
        active: bool,
    ) -> egui::Response {
        let accent = self.accent();
        let text_col = if active {
            self.text_color()
        } else {
            self.subtle_text()
        };

        let button =
            egui::Button::new(egui::RichText::new(label).size(16.0).strong().color(text_col))
                .fill(egui::Color32::TRANSPARENT);

        let response = ui.add(button);

        let hover = ctx.animate_bool(response.id.with("hover"), response.hovered());
        let selected = ctx.animate_bool(response.id.with("active"), active);

        let strength = selected.max(hover * 0.45);

        if strength > 0.01 {
            let underline = egui::Rect::from_min_size(
                egui::pos2(response.rect.min.x, response.rect.max.y - 3.0),
                egui::vec2(response.rect.width(), 3.0),
            );

            ui.painter().rect_filled(
                underline,
                egui::Rounding::same(2.0),
                Self::fade(accent, strength),
            );
        }

        response
    }

    fn update_modal(
        &mut self,
        ctx: &egui::Context,
        app_update_to_download: &mut Option<GithubAsset>,
    ) {
        if !self.show_update_modal {
            return;
        }

        let mut close_modal = false;
        let accent = self.accent();

        egui::Window::new("Launcher update")
            .id(egui::Id::new("app_update_modal"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(480.0)
            .show(ctx, |ui| {
                if let Some(release) = &self.latest_app_release {
                    ui.label(
                        egui::RichText::new(format!("Version {} is available", release.tag_name))
                            .size(20.0)
                            .strong(),
                    );

                    ui.label(
                        egui::RichText::new(format!("Current version: {}", APP_VERSION))
                            .color(self.subtle_text()),
                    );

                    ui.add_space(10.0);

                    if self.app_update_downloading {
                        ui.label(
                            egui::RichText::new("Downloading update... Do not close the app!")
                                .color(egui::Color32::from_rgb(255, 180, 50)),
                        );

                        ui.add_space(8.0);

                        ui.add_sized(
                            [380.0, 22.0],
                            egui::ProgressBar::new(self.app_update_progress)
                                .show_percentage()
                                .animate(true),
                        );

                        ui.add_space(6.0);
                        ui.label(&self.app_update_status);
                    } else {
                        if let Some(body) = &release.body {
                            egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                                ui.label(egui::RichText::new(body).size(14.0));
                            });
                        }

                        ui.add_space(8.0);

                        if let Some(asset) = Self::find_update_asset(release) {
                            ui.horizontal(|ui| {
                                ui.label("");
                                ui.label("");
                                ui.label(format!(
                                    "({:.1} MB)",
                                    asset.size as f64 / 1_048_576.0
                                ));
                            });
                        }

                        if self.app_update_status.starts_with("Update error") {
                            ui.add_space(6.0);
                            ui.colored_label(self.danger_color(), &self.app_update_status);
                        }

                        ui.add_space(14.0);

                        ui.horizontal(|ui| {
                            if let Some(asset) = Self::find_update_asset(release) {
                                if ui
                                    .add_sized(
                                        [170.0, 40.0],
                                        egui::Button::new(
                                            egui::RichText::new("⬇ Install update")
                                                .strong()
                                                .color(egui::Color32::WHITE),
                                        )
                                        .fill(accent),
                                    )
                                    .clicked()
                                {
                                    *app_update_to_download = Some(asset.clone());
                                }
                            } else {
                                ui.colored_label(
                                    self.danger_color(),
                                    "No .7z/.zip update package found.",
                                );
                            }

                            if ui
                                .add_sized([100.0, 40.0], egui::Button::new("Later"))
                                .clicked()
                            {
                                close_modal = true;
                            }
                        });
                    }
                } else {
                    ui.label("No update information available.");
                    close_modal = true;
                }
            });

        if close_modal {
            self.show_update_modal = false;
        }
    }

    fn search_box(&mut self, ui: &mut egui::Ui, hint: &str) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("🔍").size(16.0).color(self.subtle_text()));

            let edit = egui::TextEdit::singleline(&mut self.search_query)
                .hint_text(hint)
                .desired_width(f32::INFINITY)
                .frame(true)
                .margin(egui::vec2(10.0, 8.0));

            ui.add(edit);

            if !self.search_query.is_empty() {
                if ui
                    .add(egui::Button::new("✖").frame(false))
                    .on_hover_text("Clear search")
                    .clicked()
                {
                    self.search_query.clear();
                }
            }
        });
    }

    fn loading_view(&self, ui: &mut egui::Ui, text: &str) {
        ui.vertical_centered(|ui| {
            ui.add_space(90.0);
            ui.spinner();
            ui.add_space(10.0);
            ui.label(egui::RichText::new(text).color(self.subtle_text()));
        });
    }

    fn ui_store_view(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let mut switch_to_details = None;
        let mut pending_add = None;
        let mut retry_fetch = false;
        let mut clear_search = false;

        ui.vertical(|ui| {
            ui.add_space(14.0);
            self.search_box(ui, "Search store...");
            ui.add_space(10.0);

            let fetch_error = self.fetch_error.clone();
            let query = self.search_query.trim().to_lowercase();

            let items: Option<Vec<(usize, GithubRelease)>> =
                self.releases.as_ref().map(|releases| {
                    releases
                        .iter()
                        .enumerate()
                        .filter(|(_, release)| Self::matches_release(release, &query))
                        .map(|(idx, release)| (idx, release.clone()))
                        .collect()
                });

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    if let Some(err) = fetch_error {
                        ui.vertical_centered(|ui| {
                            ui.add_space(80.0);
                            ui.label(egui::RichText::new("❌").size(40.0));
                            ui.colored_label(
                                self.danger_color(),
                                format!("Failed to fetch games: {}", err),
                            );

                            if ui.button("Retry connection").clicked() {
                                retry_fetch = true;
                            }
                        });
                        return;
                    }

                    match items {
                        None => {
                            self.loading_view(ui, "Loading store...");
                        }
                        Some(items) => {
                            if items.is_empty() {
                                ui.vertical_centered(|ui| {
                                    ui.add_space(80.0);
                                    ui.label(egui::RichText::new("🕹️").size(42.0));

                                    if query.is_empty() {
                                        ui.label("No games found.");
                                    } else {
                                        ui.label("No results for this search.");

                                        if ui.button("Clear search").clicked() {
                                            clear_search = true;
                                        }
                                    }
                                });
                            }

                            for (card_index, (release_index, release)) in
                                items.iter().enumerate()
                            {
                                let game_name = release
                                    .name
                                    .clone()
                                    .unwrap_or_else(|| release.tag_name.clone());

                                let safe_name = Self::safe_name(&game_name);
                                let in_library = self.config.library.contains(&safe_name);

                                let card_id = ui.id().with("store_card").with(release_index);

                                let card_height = 96.0;
                                let (base_rect, _) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), card_height),
                                    egui::Sense::hover(),
                                );

                                let appear = ctx.animate_value_with_time(
                                    card_id.with("appear"),
                                    1.0,
                                    0.22 + card_index as f32 * 0.045,
                                );

                                let offset_x = (1.0 - appear) * 70.0;
                                let alpha = appear;

                                let draw_rect = egui::Rect::from_min_size(
                                    base_rect.min + egui::vec2(offset_x, 0.0),
                                    base_rect.size(),
                                );

                                let response = ui.interact(
                                    draw_rect,
                                    card_id.with("response"),
                                    egui::Sense::hover(),
                                );

                                let hover =
                                    ctx.animate_bool(card_id.with("hover"), response.hovered());

                                let bg = Self::fade(
                                    Self::mix_color(self.card_bg(), self.card_hover_bg(), hover),
                                    alpha,
                                );

                                let mut card_ui = ui.child_ui(draw_rect, *ui.layout(), None);
                                card_ui.set_clip_rect(draw_rect);

                                let shadow_color = if self.is_dark() {
                                    egui::Color32::from_black_alpha((60.0 * alpha) as u8)
                                } else {
                                    egui::Color32::from_black_alpha((18.0 * alpha) as u8)
                                };

                                egui::Frame::none()
                                    .fill(bg)
                                    .rounding(18.0)
                                    .inner_margin(16.0)
                                    .shadow(egui::epaint::Shadow {
                                        offset: egui::vec2(0.0, 6.0 + hover * 6.0),
                                        blur: 14.0 + hover * 18.0,
                                        spread: 0.0,
                                        color: shadow_color,
                                    })
                                    .show(&mut card_ui, |ui| {
                                        ui.horizontal(|ui| {
                                            let icon_color = Self::hash_color(&game_name);

                                            egui::Frame::none()
                                                .fill(Self::fade(icon_color, 0.18 * alpha))
                                                .rounding(14.0)
                                                .inner_margin(egui::vec2(14.0, 12.0))
                                                .show(ui, |ui| {
                                                    ui.label(
                                                        egui::RichText::new("🎮").size(24.0),
                                                    );
                                                });

                                            ui.add_space(12.0);

                                            ui.vertical(|ui| {
                                                let title_response = ui.add(egui::Link::new(
                                                    egui::RichText::new(&game_name)
                                                        .size(21.0)
                                                        .strong()
                                                        .color(Self::fade(
                                                            self.text_color(),
                                                            alpha,
                                                        )),
                                                ));

                                                if title_response
                                                    .on_hover_text("View game details")
                                                    .clicked()
                                                {
                                                    switch_to_details = Some(*release_index);
                                                }

                                                if let Some(body) = &release.body {
                                                    if !body.trim().is_empty() {
                                                        let snippet: String = body
                                                            .lines()
                                                            .next()
                                                            .unwrap_or("")
                                                            .chars()
                                                            .take(90)
                                                            .collect();

                                                        ui.label(
                                                            egui::RichText::new(format!(
                                                                "{}...",
                                                                snippet
                                                            ))
                                                            .color(Self::fade(
                                                                self.subtle_text(),
                                                                alpha,
                                                            )),
                                                        );
                                                    } else {
                                                        ui.label(
                                                            egui::RichText::new(&release.tag_name)
                                                                .color(Self::fade(
                                                                    self.subtle_text(),
                                                                    alpha,
                                                                )),
                                                        );
                                                    }
                                                } else {
                                                    ui.label(
                                                        egui::RichText::new(&release.tag_name)
                                                            .color(Self::fade(
                                                                self.subtle_text(),
                                                                alpha,
                                                            )),
                                                    );
                                                }
                                            });

                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if in_library {
                                                        ui.label(
                                                            egui::RichText::new("✔ In Library")
                                                                .strong()
                                                                .color(Self::fade(
                                                                    self.success_color(),
                                                                    alpha,
                                                                )),
                                                        );
                                                    } else if ui
                                                        .add_sized(
                                                            [130.0, 38.0],
                                                            egui::Button::new(
                                                                egui::RichText::new(
                                                                    "➕ Add to Library",
                                                                )
                                                                .size(13.0),
                                                            ),
                                                        )
                                                        .clicked()
                                                    {
                                                        pending_add = Some(safe_name.clone());
                                                    }

                                                    ui.add_space(8.0);

                                                    if ui
                                                        .add_sized(
                                                            [90.0, 38.0],
                                                            egui::Button::new(
                                                                egui::RichText::new("Details")
                                                                    .size(13.0),
                                                            ),
                                                        )
                                                        .clicked()
                                                    {
                                                        switch_to_details = Some(*release_index);
                                                    }
                                                },
                                            );
                                        });
                                    });

                                ui.add_space(12.0);
                            }
                        }
                    }
                });
        });

        if clear_search {
            self.search_query.clear();
        }

        if retry_fetch {
            self.fetch_error = None;
            self.fetch_releases();
        }

        if let Some(game) = pending_add {
            if !self.config.library.contains(&game) {
                self.config.library.push(game);
                self.save_config();
            }
        }

        if let Some(idx) = switch_to_details {
            self.selected_release_idx = Some(idx);
            self.change_tab(LauncherTab::GameDetails);
        }
    }

    fn ui_library_view(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let mut pending_download = None;
        let mut pending_launch = None;
        let mut pending_remove = None;
        let mut switch_to_details = None;
        let mut go_store = false;
        let mut clear_search = false;

        ui.vertical(|ui| {
            ui.add_space(14.0);
            self.search_box(ui, "Search library...");
            ui.add_space(10.0);

            let fetch_error = self.fetch_error.clone();
            let query = self.search_query.trim().to_lowercase();
            let library = self.config.library.clone();

            let items: Option<Vec<(usize, GithubRelease)>> =
                self.releases.as_ref().map(|releases| {
                    releases
                        .iter()
                        .enumerate()
                        .filter(|(_, release)| {
                            let name = release
                                .name
                                .clone()
                                .unwrap_or_else(|| release.tag_name.clone());
                            let safe = Self::safe_name(&name);

                            library.contains(&safe) && Self::matches_release(release, &query)
                        })
                        .map(|(idx, release)| (idx, release.clone()))
                        .collect()
                });

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    if let Some(err) = fetch_error {
                        ui.vertical_centered(|ui| {
                            ui.add_space(80.0);
                            ui.colored_label(
                                self.danger_color(),
                                format!("Failed to fetch library data: {}", err),
                            );
                        });
                        return;
                    }

                    match items {
                        None => {
                            self.loading_view(ui, "Loading library...");
                        }
                        Some(items) => {
                            if items.is_empty() {
                                ui.vertical_centered(|ui| {
                                    ui.add_space(80.0);
                                    ui.label(egui::RichText::new("📚").size(42.0));

                                    if !query.is_empty() {
                                        ui.label("No library results for this search.");

                                        if ui.button("Clear search").clicked() {
                                            clear_search = true;
                                        }
                                    } else {
                                        ui.label(
                                            egui::RichText::new(
                                                "This library looks like a blank desert...you can fix that by adding a game from the store!",
                                            )
                                            .size(16.0),
                                        );

                                        if ui.button("Go to Store").clicked() {
                                            go_store = true;
                                        }
                                    }
                                });
                            }

                            for (card_index, (release_index, release)) in
                                items.iter().enumerate()
                            {
                                let game_name = release
                                    .name
                                    .clone()
                                    .unwrap_or_else(|| release.tag_name.clone());

                                let safe_name = Self::safe_name(&game_name);
                                let target_dir = self.games_dir.join(&safe_name);
                                let is_installed = target_dir.exists();

                                let card_id = ui.id().with("library_card").with(release_index);

                                let card_height = 96.0;
                                let (base_rect, _) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), card_height),
                                    egui::Sense::hover(),
                                );

                                let appear = ctx.animate_value_with_time(
                                    card_id.with("appear"),
                                    1.0,
                                    0.22 + card_index as f32 * 0.045,
                                );

                                let offset_x = (1.0 - appear) * 70.0;
                                let alpha = appear;

                                let draw_rect = egui::Rect::from_min_size(
                                    base_rect.min + egui::vec2(offset_x, 0.0),
                                    base_rect.size(),
                                );

                                let response = ui.interact(
                                    draw_rect,
                                    card_id.with("response"),
                                    egui::Sense::hover(),
                                );

                                let hover =
                                    ctx.animate_bool(card_id.with("hover"), response.hovered());

                                let bg = Self::fade(
                                    Self::mix_color(self.card_bg(), self.card_hover_bg(), hover),
                                    alpha,
                                );

                                let mut card_ui = ui.child_ui(draw_rect, *ui.layout(), None);
                                card_ui.set_clip_rect(draw_rect);

                                let shadow_color = if self.is_dark() {
                                    egui::Color32::from_black_alpha((60.0 * alpha) as u8)
                                } else {
                                    egui::Color32::from_black_alpha((18.0 * alpha) as u8)
                                };

                                egui::Frame::none()
                                    .fill(bg)
                                    .rounding(18.0)
                                    .inner_margin(16.0)
                                    .shadow(egui::epaint::Shadow {
                                        offset: egui::vec2(0.0, 6.0 + hover * 6.0),
                                        blur: 14.0 + hover * 18.0,
                                        spread: 0.0,
                                        color: shadow_color,
                                    })
                                    .show(&mut card_ui, |ui| {
                                        ui.horizontal(|ui| {
                                            let icon_color = Self::hash_color(&game_name);

                                            egui::Frame::none()
                                                .fill(Self::fade(icon_color, 0.18 * alpha))
                                                .rounding(14.0)
                                                .inner_margin(egui::vec2(14.0, 12.0))
                                                .show(ui, |ui| {
                                                    ui.label(
                                                        egui::RichText::new("📦").size(24.0),
                                                    );
                                                });

                                            ui.add_space(12.0);

                                            ui.vertical(|ui| {
                                                let title_response = ui.add(egui::Link::new(
                                                    egui::RichText::new(&game_name)
                                                        .size(21.0)
                                                        .strong()
                                                        .color(Self::fade(
                                                            self.text_color(),
                                                            alpha,
                                                        )),
                                                ));

                                                if title_response
                                                    .on_hover_text("View game details")
                                                    .clicked()
                                                {
                                                    switch_to_details = Some(*release_index);
                                                }

                                                if is_installed {
                                                    ui.label(
                                                        egui::RichText::new("")
                                                            .color(Self::fade(
                                                                self.success_color(),
                                                                alpha,
                                                            )),
                                                    );
                                                } else {
                                                    ui.label(
                                                        egui::RichText::new("Not Installed")
                                                            .color(Self::fade(
                                                                self.subtle_text(),
                                                                alpha,
                                                            )),
                                                    );
                                                }
                                            });

                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if is_installed {
                                                        if ui
                                                            .add_sized(
                                                                [110.0, 38.0],
                                                                egui::Button::new(
                                                                    egui::RichText::new("▶ Play")
                                                                        .strong()
                                                                        .color(
                                                                            egui::Color32::from_rgb(
                                                                                8, 25, 12,
                                                                            ),
                                                                        ),
                                                                )
                                                                .fill(self.success_color()),
                                                            )
                                                            .clicked()
                                                        {
                                                            pending_launch =
                                                                Some(target_dir.clone());
                                                        }

                                                        ui.add_space(6.0);

                                                        if ui
                                                            .add_sized(
                                                                [38.0, 38.0],
                                                                egui::Button::new("📁"),
                                                            )
                                                            .on_hover_text("Open game folder")
                                                            .clicked()
                                                        {
                                                            Self::open_path(&target_dir);
                                                        }
                                                    } else if let Some(asset) =
                                                        release.assets.iter().find(|a| {
                                                            a.name.to_lowercase().ends_with(".7z")
                                                        })
                                                    {
                                                        ui.add_enabled_ui(
                                                            !self.is_downloading,
                                                            |ui| {
                                                                if ui
                                                                    .add_sized(
                                                                        [120.0, 38.0],
                                                                        egui::Button::new(
                                                                            egui::RichText::new(
                                                                                "⬇ Download",
                                                                            )
                                                                            .size(14.0),
                                                                        ),
                                                                    )
                                                                    .clicked()
                                                                {
                                                                    pending_download = Some((
                                                                        asset.clone(),
                                                                        game_name.clone(),
                                                                    ));
                                                                }
                                                            },
                                                        );

                                                        ui.label(
                                                            egui::RichText::new(format!(
                                                                "{:.1} MB",
                                                                asset.size as f64 / 1_048_576.0
                                                            ))
                                                            .color(Self::fade(
                                                                self.subtle_text(),
                                                                alpha,
                                                            )),
                                                        );
                                                    } else {
                                                        ui.label(
                                                            egui::RichText::new("Missing package")
                                                                .color(Self::fade(
                                                                    self.danger_color(),
                                                                    alpha,
                                                                )),
                                                        );
                                                    }

                                                    ui.add_space(6.0);

                                                    if ui
                                                        .add_sized(
                                                            [38.0, 38.0],
                                                            egui::Button::new("🗑"),
                                                        )
                                                        .on_hover_text("Remove from library")
                                                        .clicked()
                                                    {
                                                        pending_remove = Some(safe_name.clone());
                                                    }
                                                },
                                            );
                                        });
                                    });

                                ui.add_space(12.0);
                            }
                        }
                    }
                });
        });

        if clear_search {
            self.search_query.clear();
        }

        if go_store {
            self.change_tab(LauncherTab::Store);
        }

        if let Some(game) = pending_remove {
            self.config.library.retain(|g| g != &game);
            self.save_config();
        }

        if let Some(idx) = switch_to_details {
            self.selected_release_idx = Some(idx);
            self.change_tab(LauncherTab::GameDetails);
        }

        if let Some((asset, game_name)) = pending_download {
            self.download_and_install(asset, game_name);
        }

        if let Some(target_dir) = pending_launch {
            self.launch_game(&target_dir);
        }
    }

    fn ui_game_details_view(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let mut pending_download = None;
        let mut pending_launch = None;
        let mut add_to_library = false;
        let mut remove_from_library = false;

        let release_opt = self
            .selected_release_idx
            .and_then(|idx| self.releases.as_ref().and_then(|r| r.get(idx)).cloned());

        let release_opt_clone = release_opt.clone();

        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.add_space(18.0);

                if let Some(release) = release_opt_clone {
                    let game_name = release
                        .name
                        .clone()
                        .unwrap_or_else(|| release.tag_name.clone());

                    let safe_name = Self::safe_name(&game_name);
                    let target_dir = self.games_dir.join(&safe_name);
                    let is_installed = target_dir.exists();
                    let in_library = self.config.library.contains(&safe_name);

                    let anim_id = ui.id().with("details_hero").with(&safe_name);

                    let alpha = ctx.animate_value_with_time(anim_id, 1.0, 0.32);

                    egui::Frame::none()
                        .fill(Self::fade(self.card_bg(), alpha))
                        .rounding(24.0)
                        .inner_margin(28.0)
                        .shadow(egui::epaint::Shadow {
                            offset: egui::vec2(0.0, 12.0),
                            blur: 34.0,
                            spread: 0.0,
                            color: egui::Color32::from_black_alpha(if self.is_dark() {
                                (100.0 * alpha) as u8
                            } else {
                                (28.0 * alpha) as u8
                            }),
                        })
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let icon_color = Self::hash_color(&game_name);

                                egui::Frame::none()
                                    .fill(Self::fade(icon_color, 0.18 * alpha))
                                    .rounding(20.0)
                                    .inner_margin(egui::vec2(24.0, 22.0))
                                    .show(ui, |ui| {
                                        ui.label(egui::RichText::new("🎮").size(42.0));
                                    });

                                ui.add_space(16.0);

                                ui.vertical(|ui| {
                                    ui.heading(
                                        egui::RichText::new(&game_name)
                                            .size(34.0)
                                            .strong()
                                            .color(Self::fade(self.text_color(), alpha)),
                                    );

                                    ui.add_space(4.0);

                                    ui.label(
                                        egui::RichText::new(format!(""))
                                            .color(Self::fade(self.subtle_text(), alpha)),
                                    );

                                    if is_installed {
                                        ui.label(
                                            egui::RichText::new("✅ Installed")
                                                .color(Self::fade(self.success_color(), alpha)),
                                        );
                                    } else {
                                        ui.label(
                                            egui::RichText::new("⬇ Not installed")
                                                .color(Self::fade(self.subtle_text(), alpha)),
                                        );
                                    }
                                });

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if in_library {
                                            if is_installed {
                                                if ui
                                                    .add_sized(
                                                        [160.0, 44.0],
                                                        egui::Button::new(
                                                            egui::RichText::new("▶ Play")
                                                                .size(18.0)
                                                                .strong()
                                                                .color(egui::Color32::from_rgb(
                                                                    8, 25, 12,
                                                                )),
                                                        )
                                                        .fill(self.success_color()),
                                                    )
                                                    .clicked()
                                                {
                                                    pending_launch = Some(target_dir.clone());
                                                }

                                                ui.add_space(8.0);

                                                if ui
                                                    .add_sized([44.0, 44.0], egui::Button::new("📁"))
                                                    .on_hover_text("Open game folder")
                                                    .clicked()
                                                {
                                                    Self::open_path(&target_dir);
                                                }
                                            } else if let Some(asset) =
                                                release.assets.iter().find(|a| {
                                                    a.name.to_lowercase().ends_with(".7z")
                                                })
                                            {
                                                ui.add_enabled_ui(!self.is_downloading, |ui| {
                                                    if ui
                                                        .add_sized(
                                                            [160.0, 44.0],
                                                            egui::Button::new(
                                                                egui::RichText::new("⬇ Install")
                                                                    .size(17.0)
                                                                    .strong(),
                                                            ),
                                                        )
                                                        .clicked()
                                                    {
                                                        pending_download = Some((
                                                            asset.clone(),
                                                            game_name.clone(),
                                                        ));
                                                    }
                                                });
                                            } else {
                                                ui.label(
                                                    egui::RichText::new("Missing package")
                                                        .color(Self::fade(
                                                            self.danger_color(),
                                                            alpha,
                                                        )),
                                                );
                                            }

                                            ui.add_space(8.0);

                                            if ui
                                                .add_sized(
                                                    [140.0, 44.0],
                                                    egui::Button::new("Remove from Library"),
                                                )
                                                .clicked()
                                            {
                                                remove_from_library = true;
                                            }
                                        } else if ui
                                            .add_sized(
                                                [160.0, 44.0],
                                                egui::Button::new("➕ Add to Library"),
                                            )
                                            .clicked()
                                        {
                                            add_to_library = true;
                                        }
                                    },
                                );
                            });

                            ui.add_space(24.0);
                            ui.separator();
                            ui.add_space(18.0);

                            ui.label(
                                egui::RichText::new("Description")
                                    .size(22.0)
                                    .strong()
                                    .color(Self::fade(self.text_color(), alpha)),
                            );

                            ui.add_space(12.0);

                            if let Some(body) = &release.body {
                                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new(body)
                                            .size(15.0)
                                            .color(Self::fade(self.subtle_text(), alpha)),
                                    );
                                });
                            } else {
                                ui.label(
                                    egui::RichText::new(r"No description ¯\_(ツ)_/¯")
                                        .italics()
                                        .color(Self::fade(self.subtle_text(), alpha)),
                                );
                            }
                        });
                } else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(80.0);
                        ui.label("Game not found.");
                    });
                }
            });

        if add_to_library {
            if let Some(release) = release_opt.clone() {
                let game_name = release
                    .name
                    .clone()
                    .unwrap_or_else(|| release.tag_name.clone());

                let safe_name = Self::safe_name(&game_name);

                if !self.config.library.contains(&safe_name) {
                    self.config.library.push(safe_name);
                    self.save_config();
                }
            }
        }

        if remove_from_library {
            if let Some(release) = release_opt.clone() {
                let game_name = release
                    .name
                    .clone()
                    .unwrap_or_else(|| release.tag_name.clone());

                let safe_name = Self::safe_name(&game_name);

                self.config.library.retain(|g| g != &safe_name);
                self.save_config();
            }
        }

        if let Some((asset, game_name)) = pending_download {
            self.download_and_install(asset, game_name);
        }

        if let Some(target_dir) = pending_launch {
            self.launch_game(&target_dir);
        }
    }

    fn ui_settings_view(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let mut theme_changed = false;
        let mut check_update = false;
        let mut open_games = false;
        let mut open_config = false;
        let mut view_update = false;

        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(20.0);
                    ui.set_max_width(700.0);

                    egui::Frame::none()
                        .fill(self.card_bg())
                        .rounding(20.0)
                        .inner_margin(24.0)
                        .shadow(egui::epaint::Shadow {
                            offset: egui::vec2(0.0, 8.0),
                            blur: 22.0,
                            spread: 0.0,
                            color: egui::Color32::from_black_alpha(if self.is_dark() { 90 } else { 22 }),
                        })
                        .show(ui, |ui| {
                            ui.heading("🎨 Appearance");
                            ui.add_space(12.0);

                            ui.horizontal(|ui| {
                                if ui
                                    .selectable_value(&mut self.config.theme, AppTheme::Dark, "🌙 Dark")
                                    .clicked()
                                {
                                    theme_changed = true;
                                }

                                if ui
                                    .selectable_value(&mut self.config.theme, AppTheme::Light, "☀ Light")
                                    .clicked()
                                {
                                    theme_changed = true;
                                }

                                if ui
                                    .selectable_value(&mut self.config.theme, AppTheme::Neon, "🌈 Neon")
                                    .clicked()
                                {
                                    theme_changed = true;
                                }
                            });
                        });

                    ui.add_space(16.0);

                    egui::Frame::none()
                        .fill(self.card_bg())
                        .rounding(20.0)
                        .inner_margin(24.0)
                        .shadow(egui::epaint::Shadow {
                            offset: egui::vec2(0.0, 8.0),
                            blur: 22.0,
                            spread: 0.0,
                            color: egui::Color32::from_black_alpha(if self.is_dark() { 90 } else { 22 }),
                        })
                        .show(ui, |ui| {
                            ui.heading("💾 Storage");
                            ui.add_space(12.0);

                            ui.horizontal(|ui| {
                                ui.label("Games directory:");
                                ui.label(
                                    egui::RichText::new(self.games_dir.to_string_lossy())
                                        .color(self.subtle_text()),
                                );

                                if ui.button("Open").clicked() {
                                    open_games = true;
                                }
                            });

                            ui.add_space(8.0);

                            ui.horizontal(|ui| {
                                ui.label("Config directory:");
                                ui.label(
                                    egui::RichText::new(self.config_dir.to_string_lossy())
                                        .color(self.subtle_text()),
                                );

                                if ui.button("Open").clicked() {
                                    open_config = true;
                                }
                            });
                        });

                    ui.add_space(16.0);

                    egui::Frame::none()
                        .fill(self.card_bg())
                        .rounding(20.0)
                        .inner_margin(24.0)
                        .shadow(egui::epaint::Shadow {
                            offset: egui::vec2(0.0, 8.0),
                            blur: 22.0,
                            spread: 0.0,
                            color: egui::Color32::from_black_alpha(if self.is_dark() { 90 } else { 22 }),
                        })
                        .show(ui, |ui| {
                            ui.heading("Launcher updates");
                            ui.add_space(12.0);

                            ui.horizontal(|ui| {
                                ui.label("Current version:");
                                ui.label(egui::RichText::new(APP_VERSION).strong());
                            });

                            if !self.app_update_info.is_empty() {
                                ui.add_space(6.0);
                                ui.label(&self.app_update_info);
                            }

                            if self.checking_update {
                                ui.add_space(6.0);
                                ui.spinner();
                            }

                            ui.add_space(12.0);

                            ui.horizontal(|ui| {
                                if ui.button("Check for updates").clicked() {
                                    check_update = true;
                                }

                                if self.latest_app_release.is_some() && !self.app_update_downloading
                                {
                                    if ui.button("View available update").clicked() {
                                        view_update = true;
                                    }
                                }
                            });
                        });

                    ui.add_space(20.0);
                });
            });

        if theme_changed {
            Self::apply_theme(&self.config.theme, ctx);
            self.save_config();
        }

        if check_update {
            self.check_app_updates();
        }

        if open_games {
            Self::open_path(&self.games_dir);
        }

        if open_config {
            Self::open_path(&self.config_dir);
        }

        if view_update {
            self.show_update_modal = true;
        }
    }
}

impl eframe::App for Vault64App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_messages();

        match self.state {
            AppState::Login => self.ui_login(ctx),
            AppState::Launcher => self.ui_launcher(ctx),
        }

        ctx.request_repaint();
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 680.0])
            .with_min_inner_size([760.0, 520.0]),
        vsync: true,
        ..Default::default()
    };

    eframe::run_native(
        "Vault64 Launcher",
        options,
        Box::new(|cc| Ok(Box::new(Vault64App::new(cc)))),
    )
}
// Copyright Flatron Tech 2026 :D //

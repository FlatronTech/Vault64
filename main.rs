// MADE BY FLATRONIX :D //
#![windows_subsystem = "windows"]

use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{copy, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zip::ZipArchive;
use tray_icon::{
    menu::{Menu, MenuItem, MenuEvent, PredefinedMenuItem},
    TrayIcon, TrayIconBuilder, TrayIconEvent,
};

const APP_VERSION: &str = "4.0";

const SUPABASE_URL: &str = "https://eblytftpjtjfdpncqaiq.supabase.co";
const SUPABASE_ANON_KEY: &str = "sb_publishable_9__Dkx_mbYzUMw06zjsZog_LQDkbYzx";

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

# Wait for the app to close (max 30s)
$waited = 0
while ((Get-Process -Name $exeBase -ErrorAction SilentlyContinue) -and ($waited -lt 30)) {
    Start-Sleep -Milliseconds 500
    $waited += 0.5
}
Start-Sleep -Seconds 1
Log "App process closed (waited $waited s)"

# 2) Copy new files from the update folder to the app folder
try {
    Copy-Item -Path (Join-Path $Source "*") -Destination $Dest -Recurse -Force -ErrorAction Stop
    Log "Copy OK"
} catch {
    Log "COPY FAILED: $($_.Exception.Message)"
    exit 1
}

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


if (Test-Path -LiteralPath $Cleanup) {
    Remove-Item -LiteralPath $Cleanup -Recurse -Force -ErrorAction SilentlyContinue
    Log "Cleanup removed"
}


$newExe = Join-Path $Dest $AppExe
if (Test-Path -LiteralPath $newExe) {
    Start-Process -FilePath $newExe -WorkingDirectory $Dest
    Log "App restarted"
} else {
    Log "ERROR: $newExe not found after update!"
    exit 1
}

Log "=== Update finished OK ==="


Remove-Item -LiteralPath $PSCommandPath -Force -ErrorAction SilentlyContinue
exit 0
"#;

#[derive(Serialize, Deserialize, Clone, Debug)]
struct GithubRelease {
    name: Option<String>,
    tag_name: String,
    body: Option<String>,
    assets: Vec<GithubAsset>,
    #[allow(dead_code)]
    html_url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Serialize, Deserialize, Clone)]
struct ReleasesCache {
    etag: Option<String>,
    releases: Vec<GithubRelease>,
    updated_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Default)]
enum AppTheme {
    #[default]
    Dark,
    Light,
    Neon,
    Aero,
}

#[derive(Serialize, Deserialize, Default)]
struct AppConfig {
    remembered_username: Option<String>,
    #[serde(default)]
    library: Vec<String>,
    #[serde(default)]
    theme: AppTheme,
    #[serde(default)]
    playtime: HashMap<String, u64>,
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

#[derive(Clone, Debug)]
enum AccountUpdateSuccess {
    DisplayName(String),
    Email(String),
    Password,
}

#[derive(PartialEq, Clone, Copy, Debug, Default)]
enum SettingsSubTab {
    #[default]
    General,
    Account,
}

enum AppMsg {
    ReleasesFetched(Result<Vec<GithubRelease>, String>),
    ReleasesWarning(Option<String>),
    AppUpdateFetched(Result<GithubRelease, String>),
    AppUpdateProgress(f32, String),
    AppUpdateReady(UpdatePlan),
    AppUpdateError(String),
    DownloadProgress(f32, String),
    DownloadComplete(String),
    DownloadError(String),
    // Auth
    LoginResult(Result<String, String>),    // Ok(username) or Err(message)
    RegisterResult(Result<String, String>), // Ok(username) or Err(message)
    AccountUpdateResult(Result<AccountUpdateSuccess, String>),
    AccountDeleteResult(Result<(), String>),
    RestoreWindow,
    // Playtime & Icons
    PlaytimeTick { game: String, seconds: u64 },
    GameExited(String),
    IconDownloaded(String),
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

// ---- Supabase Auth helpers (run on background thread) ----

#[derive(Serialize, Deserialize, Clone, Debug)]
struct SupabaseAuthUser {
    id: Option<String>,
    email: Option<String>,
    user_metadata: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct SupabaseAuthSession {
    access_token: String,
    user: SupabaseAuthUser,
}

#[derive(Deserialize)]
struct SupabaseSignUpResponse {
    access_token: Option<String>,
    user: Option<SupabaseAuthUser>,
    id: Option<String>,
}

#[derive(Deserialize)]
struct SupabaseErrorBody {
    message: Option<String>,
    error_description: Option<String>,
    msg: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct SupabaseProfile {
    username: Option<String>,
}

fn supabase_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(6))
        .timeout(Duration::from_secs(12))
        .user_agent("Vault64-Launcher")
        .build()
        .map_err(|e| e.to_string())
}

/// Sign in with email/password, returns username from profiles table.
fn supabase_sign_in(email: &str, password: &str) -> Result<String, String> {
    let client = supabase_client()?;
    let url = format!("{}/auth/v1/token?grant_type=password", SUPABASE_URL);

    let body = serde_json::json!({
        "email": email,
        "password": password,
    });

    let resp = client
        .post(&url)
        .header("apikey", SUPABASE_ANON_KEY)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| format!("Network error: {}", e))?;

    if resp.status().is_success() {
        let session: SupabaseAuthSession = resp
            .json()
            .map_err(|e| format!("Failed to parse response: {}", e))?;

        let profile_query = if let Some(ref uid) = session.user.id {
            format!("{}/rest/v1/profiles?id=eq.{}&select=username", SUPABASE_URL, uid)
        } else {
            format!("{}/rest/v1/profiles?select=username&limit=1", SUPABASE_URL)
        };

        let user_id_resp = client
            .get(&profile_query)
            .header("apikey", SUPABASE_ANON_KEY)
            .header("Authorization", format!("Bearer {}", session.access_token))
            .header("Accept", "application/json")
            .send();

        let mut found_username = None;
        if let Ok(res) = user_id_resp {
            if res.status().is_success() {
                if let Ok(profiles) = res.json::<Vec<SupabaseProfile>>() {
                    if let Some(first) = profiles.into_iter().next() {
                        if let Some(u) = first.username {
                            if !u.trim().is_empty() {
                                found_username = Some(u);
                            }
                        }
                    }
                }
            }
        }

        let username = found_username
            .or_else(|| {
                session
                    .user
                    .user_metadata
                    .as_ref()
                    .and_then(|m| m.get("username"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .or(session.user.email)
            .unwrap_or_else(|| email.to_string());

        Ok(username)
    } else {
        let status = resp.status();
        let err: SupabaseErrorBody = resp.json().unwrap_or(SupabaseErrorBody {
            message: None,
            error_description: None,
            msg: None,
            error: None,
        });
        let msg = err
            .message
            .or(err.error_description)
            .or(err.msg)
            .or(err.error)
            .unwrap_or_else(|| format!("HTTP {}", status));
        Err(msg)
    }
}

/// Sign up with email/password + username, inserts into profiles table.
fn supabase_sign_up(email: &str, password: &str, username: &str) -> Result<String, String> {
    let client = supabase_client()?;
    let signup_url = format!("{}/auth/v1/signup", SUPABASE_URL);

    let body = serde_json::json!({
        "email": email,
        "password": password,
        "data": {
            "username": username,
        }
    });

    let resp = client
        .post(&signup_url)
        .header("apikey", SUPABASE_ANON_KEY)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| format!("Network error: {}", e))?;

    if resp.status().is_success() {
        let signup_data: Option<SupabaseSignUpResponse> = resp.json().ok();
        let mut user_id = signup_data.as_ref().and_then(|s| s.user.as_ref().and_then(|u| u.id.clone()))
            .or_else(|| signup_data.as_ref().and_then(|s| s.id.clone()));
        let mut access_token = signup_data.as_ref().and_then(|s| s.access_token.clone());

        // If access token was not in signup response, attempt password sign-in
        if access_token.is_none() {
            let token_url = format!("{}/auth/v1/token?grant_type=password", SUPABASE_URL);
            if let Ok(token_resp) = client
                .post(&token_url)
                .header("apikey", SUPABASE_ANON_KEY)
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({"email": email, "password": password}))
                .send()
            {
                if token_resp.status().is_success() {
                    if let Ok(token_session) = token_resp.json::<SupabaseAuthSession>() {
                        access_token = Some(token_session.access_token);
                        if user_id.is_none() {
                            user_id = token_session.user.id;
                        }
                    }
                }
            }
        }

        // If we have token, upsert into profiles
        if let Some(token) = access_token {
            let profile_url = format!("{}/rest/v1/profiles", SUPABASE_URL);
            let mut profile_obj = serde_json::Map::new();
            profile_obj.insert("username".to_string(), serde_json::Value::String(username.to_string()));
            if let Some(uid) = user_id {
                profile_obj.insert("id".to_string(), serde_json::Value::String(uid));
            }

            let _ = client
                .post(&profile_url)
                .header("apikey", SUPABASE_ANON_KEY)
                .header("Authorization", format!("Bearer {}", token))
                .header("Content-Type", "application/json")
                .header("Prefer", "resolution=merge-duplicates,return=minimal")
                .json(&serde_json::Value::Object(profile_obj))
                .send();
        }

        Ok(username.to_string())
    } else {
        let status = resp.status();
        let err: SupabaseErrorBody = resp.json().unwrap_or(SupabaseErrorBody {
            message: None,
            error_description: None,
            msg: None,
            error: None,
        });
        let msg = err
            .message
            .or(err.error_description)
            .or(err.msg)
            .or(err.error)
            .unwrap_or_else(|| format!("HTTP {}", status));
        Err(msg)
    }
}

fn supabase_update_account(
    current_email: &str,
    current_password: &str,
    new_username: Option<String>,
    new_email: Option<String>,
    new_password: Option<String>,
) -> Result<AccountUpdateSuccess, String> {
    let client = supabase_client()?;

    // Authenticate with current credentials to acquire a fresh access token
    let token_url = format!("{}/auth/v1/token?grant_type=password", SUPABASE_URL);
    let token_resp = client
        .post(&token_url)
        .header("apikey", SUPABASE_ANON_KEY)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "email": current_email,
            "password": current_password,
        }))
        .send()
        .map_err(|e| format!("Network error: {}", e))?;

    if !token_resp.status().is_success() {
        let err: SupabaseErrorBody = token_resp.json().unwrap_or(SupabaseErrorBody {
            message: None,
            error_description: None,
            msg: None,
            error: None,
        });
        let msg = err
            .message
            .or(err.error_description)
            .or(err.msg)
            .or(err.error)
            .unwrap_or_else(|| "Current password is incorrect or authentication failed.".to_string());
        return Err(msg);
    }

    let session: SupabaseAuthSession = token_resp
        .json()
        .map_err(|e| format!("Failed to parse session: {}", e))?;

    let access_token = session.access_token;
    let user_id = session.user.id;

    // 1. Update Display Name
    if let Some(username) = new_username {
        let trimmed = username.trim().to_string();
        if trimmed.is_empty() {
            return Err("Display name cannot be empty.".to_string());
        }

        let user_url = format!("{}/auth/v1/user", SUPABASE_URL);
        let _ = client
            .put(&user_url)
            .header("apikey", SUPABASE_ANON_KEY)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "data": { "username": &trimmed }
            }))
            .send();

        let profile_url = format!("{}/rest/v1/profiles", SUPABASE_URL);
        let mut profile_obj = serde_json::Map::new();
        profile_obj.insert("username".to_string(), serde_json::Value::String(trimmed.clone()));
        if let Some(ref uid) = user_id {
            profile_obj.insert("id".to_string(), serde_json::Value::String(uid.clone()));
        }

        let prof_resp = client
            .post(&profile_url)
            .header("apikey", SUPABASE_ANON_KEY)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .header("Prefer", "resolution=merge-duplicates,return=minimal")
            .json(&serde_json::Value::Object(profile_obj))
            .send();

        if let Ok(res) = prof_resp {
            if !res.status().is_success() {
                if let Some(ref uid) = user_id {
                    let patch_url = format!("{}/rest/v1/profiles?id=eq.{}", SUPABASE_URL, uid);
                    let _ = client
                        .patch(&patch_url)
                        .header("apikey", SUPABASE_ANON_KEY)
                        .header("Authorization", format!("Bearer {}", access_token))
                        .header("Content-Type", "application/json")
                        .json(&serde_json::json!({ "username": &trimmed }))
                        .send();
                }
            }
        }

        return Ok(AccountUpdateSuccess::DisplayName(trimmed));
    }

    // 2. Update Email
    if let Some(email) = new_email {
        let trimmed = email.trim().to_string();
        if !trimmed.contains('@') {
            return Err("Please enter a valid email address.".to_string());
        }

        let user_url = format!("{}/auth/v1/user", SUPABASE_URL);
        let resp = client
            .put(&user_url)
            .header("apikey", SUPABASE_ANON_KEY)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "email": &trimmed
            }))
            .send()
            .map_err(|e| format!("Network error: {}", e))?;

        if !resp.status().is_success() {
            let err: SupabaseErrorBody = resp.json().unwrap_or(SupabaseErrorBody {
                message: None,
                error_description: None,
                msg: None,
                error: None,
            });
            let msg = err
                .message
                .or(err.error_description)
                .or(err.msg)
                .or(err.error)
                .unwrap_or_else(|| "Failed to update email address.".to_string());
            return Err(msg);
        }

        return Ok(AccountUpdateSuccess::Email(trimmed));
    }

    // 3. Update Password
    if let Some(password) = new_password {
        if password.len() < 6 {
            return Err("New password must be at least 6 characters.".to_string());
        }

        let user_url = format!("{}/auth/v1/user", SUPABASE_URL);
        let resp = client
            .put(&user_url)
            .header("apikey", SUPABASE_ANON_KEY)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "password": password
            }))
            .send()
            .map_err(|e| format!("Network error: {}", e))?;

        if !resp.status().is_success() {
            let err: SupabaseErrorBody = resp.json().unwrap_or(SupabaseErrorBody {
                message: None,
                error_description: None,
                msg: None,
                error: None,
            });
            let msg = err
                .message
                .or(err.error_description)
                .or(err.msg)
                .or(err.error)
                .unwrap_or_else(|| "Failed to update password.".to_string());
            return Err(msg);
        }

        return Ok(AccountUpdateSuccess::Password);
    }

    Err("No account change specified.".to_string())
}

fn supabase_delete_account(
    current_email: &str,
    current_password: &str,
) -> Result<(), String> {
    let client = supabase_client()?;

    let token_url = format!("{}/auth/v1/token?grant_type=password", SUPABASE_URL);
    let token_resp = client
        .post(&token_url)
        .header("apikey", SUPABASE_ANON_KEY)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "email": current_email,
            "password": current_password,
        }))
        .send()
        .map_err(|e| format!("Network error: {}", e))?;

    if !token_resp.status().is_success() {
        let err: SupabaseErrorBody = token_resp.json().unwrap_or(SupabaseErrorBody {
            message: None,
            error_description: None,
            msg: None,
            error: None,
        });
        let msg = err
            .message
            .or(err.error_description)
            .or(err.msg)
            .or(err.error)
            .unwrap_or_else(|| "Current password is incorrect.".to_string());
        return Err(msg);
    }

    let session: SupabaseAuthSession = token_resp
        .json()
        .map_err(|e| format!("Failed to parse session: {}", e))?;

    let access_token = session.access_token;
    let user_id = session.user.id;

    // 1. Delete public profile from Supabase profiles table
    if let Some(ref uid) = user_id {
        let profile_url = format!("{}/rest/v1/profiles?id=eq.{}", SUPABASE_URL, uid);
        let _ = client
            .delete(&profile_url)
            .header("apikey", SUPABASE_ANON_KEY)
            .header("Authorization", format!("Bearer {}", access_token))
            .send();
    }

    // 2. Mark user deleted in Supabase auth metadata
    let user_url = format!("{}/auth/v1/user", SUPABASE_URL);
    let _ = client
        .put(&user_url)
        .header("apikey", SUPABASE_ANON_KEY)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "data": { "deleted": true, "username": "[deleted]" }
        }))
        .send();

    // 3. Invalidate auth session
    let logout_url = format!("{}/auth/v1/logout", SUPABASE_URL);
    let _ = client
        .post(&logout_url)
        .header("apikey", SUPABASE_ANON_KEY)
        .header("Authorization", format!("Bearer {}", access_token))
        .send();

    Ok(())
}

struct Vault64App {
    state: AppState,
    tab: LauncherTab,
    previous_tab: LauncherTab,
    tab_switch_counter: u64,
    selected_release_idx: Option<usize>,

    login_username: String,
    login_password: String,
    login_email: String,
    remember_me: bool,
    login_error: Option<String>,
    login_pending: bool,

    // Registration
    show_register: bool,
    reg_email: String,
    reg_username: String,
    reg_password: String,
    reg_password2: String,
    reg_error: Option<String>,
    reg_success: Option<String>,
    reg_pending: bool,

    config_dir: PathBuf,
    games_dir: PathBuf,
    config: AppConfig,

    releases: Option<Vec<GithubRelease>>,
    fetch_error: Option<String>,
    fetch_warning: Option<String>,
    is_fetching_releases: bool,

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
    icons_dir: PathBuf,
    active_games: HashSet<String>,
    game_icons: HashMap<String, egui::TextureHandle>,
    app_icon_texture: Option<egui::TextureHandle>,

    // Settings Subtabs & Account management
    settings_subtab: SettingsSubTab,
    acc_display_name: String,
    acc_new_email: String,
    acc_curr_password: String,
    acc_new_password: String,
    acc_new_password2: String,
    acc_status: Option<(bool, String)>,
    acc_pending: bool,
    acc_delete_password: String,
    acc_delete_confirm: bool,

    // System tray & close behavior
    tray: Option<AppTray>,
    should_exit: bool,
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
        let icons_dir = config_dir.join("icons");
        fs::create_dir_all(&icons_dir).ok();

        let mut app_icon_texture = None;
        let icon_bytes = include_bytes!("../icon.ico");
        if let Ok(img) = image::load_from_memory_with_format(icon_bytes, image::ImageFormat::Ico)
            .or_else(|_| image::load_from_memory(icon_bytes))
        {
            let rgba = img.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba.into_raw());
            app_icon_texture = Some(cc.egui_ctx.load_texture(
                "vault64_app_icon",
                color_image,
                egui::TextureOptions::LINEAR,
            ));
        }

        let config_path = config_dir.join("config.json");
        let mut config = AppConfig::default();
        let mut state = AppState::Login;
        let mut login_username = String::new();
        let mut login_email = String::new();
        let mut remember_me = false;

        if let Ok(file_content) = fs::read_to_string(&config_path) {
            if let Ok(parsed) = serde_json::from_str::<AppConfig>(&file_content) {
                config = parsed;
                if let Some(user) = &config.remembered_username {
                    if let Some((u, e)) = user.split_once('|') {
                        login_username = u.to_string();
                        login_email = e.to_string();
                    } else {
                        login_username = user.clone();
                    }
                    remember_me = true;
                    state = AppState::Launcher;
                }
            }
        }

        Self::apply_theme(&config.theme, &cc.egui_ctx);

        let cached_releases = Self::load_cached_releases(&config_dir);
        let initial_releases = if let Some((releases, _)) = cached_releases {
            Some(releases)
        } else {
            let defaults = Self::default_releases_catalog();
            Self::save_cached_releases(&config_dir, &defaults, None);
            Some(defaults)
        };

        let mut app = Self {
            state,
            tab: LauncherTab::Store,
            previous_tab: LauncherTab::Store,
            tab_switch_counter: 0,
            selected_release_idx: None,
            login_username,
            login_password: String::new(),
            login_email,
            remember_me,
            login_error: None,
            login_pending: false,
            show_register: false,
            reg_email: String::new(),
            reg_username: String::new(),
            reg_password: String::new(),
            reg_password2: String::new(),
            reg_error: None,
            reg_success: None,
            reg_pending: false,
            config_dir,
            games_dir,
            config,
            releases: initial_releases,
            fetch_error: None,
            fetch_warning: None,
            is_fetching_releases: false,
            latest_app_release: None,
            show_update_modal: false,
            checking_update: false,
            app_update_downloading: false,
            app_update_progress: 0.0,
            app_update_status: String::new(),
            app_update_info: String::new(),
            search_query: String::new(),
            tx: tx.clone(),
            rx,
            is_downloading: false,
            download_progress: 0.0,
            download_status: String::new(),
            icons_dir,
            active_games: HashSet::new(),
            game_icons: HashMap::new(),
            app_icon_texture,

            settings_subtab: SettingsSubTab::General,
            acc_display_name: String::new(),
            acc_new_email: String::new(),
            acc_curr_password: String::new(),
            acc_new_password: String::new(),
            acc_new_password2: String::new(),
            acc_status: None,
            acc_pending: false,
            acc_delete_password: String::new(),
            acc_delete_confirm: false,

            tray: AppTray::new(cc.egui_ctx.clone(), tx),
            should_exit: false,
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
            AppTheme::Aero => {
                let mut v = egui::Visuals::dark();
                v.panel_fill = egui::Color32::from_rgb(12, 24, 40);
                v.window_fill = egui::Color32::from_rgb(16, 32, 54);
                v.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(20, 42, 70);
                v.widgets.inactive.bg_fill = egui::Color32::from_rgb(26, 54, 88);
                v.widgets.hovered.bg_fill = egui::Color32::from_rgb(36, 76, 122);
                v.widgets.active.bg_fill = egui::Color32::from_rgb(0, 195, 255);
                v.selection.bg_fill = egui::Color32::from_rgb(0, 195, 255);
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
            AppTheme::Aero => egui::Color32::from_rgb(0, 195, 255),
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
        match self.config.theme {
            AppTheme::Dark => egui::Color32::from_rgb(145, 150, 165),
            AppTheme::Light => egui::Color32::from_rgb(90, 95, 110),
            AppTheme::Neon => egui::Color32::from_rgb(170, 140, 200),
            AppTheme::Aero => egui::Color32::from_rgb(135, 185, 220),
        }
    }

    fn panel_bg(&self) -> egui::Color32 {
        match self.config.theme {
            AppTheme::Dark => egui::Color32::from_rgb(13, 14, 19),
            AppTheme::Light => egui::Color32::from_rgb(228, 232, 238),
            AppTheme::Neon => egui::Color32::from_rgb(9, 4, 16),
            AppTheme::Aero => egui::Color32::from_rgb(12, 24, 40),
        }
    }

    fn card_bg(&self) -> egui::Color32 {
        match self.config.theme {
            AppTheme::Dark => egui::Color32::from_rgb(26, 28, 36),
            AppTheme::Light => egui::Color32::WHITE,
            AppTheme::Neon => egui::Color32::from_rgb(22, 10, 36),
            AppTheme::Aero => egui::Color32::from_rgb(18, 38, 62),
        }
    }

    fn card_hover_bg(&self) -> egui::Color32 {
        match self.config.theme {
            AppTheme::Dark => egui::Color32::from_rgb(35, 39, 52),
            AppTheme::Light => egui::Color32::from_rgb(238, 244, 252),
            AppTheme::Neon => egui::Color32::from_rgb(36, 16, 58),
            AppTheme::Aero => egui::Color32::from_rgb(26, 56, 92),
        }
    }

    fn card_stroke(&self, hover: f32) -> egui::Stroke {
        match self.config.theme {
            AppTheme::Aero => {
                let alpha = (80.0 + hover * 140.0) as u8;
                egui::Stroke::new(1.2 + hover * 0.8, egui::Color32::from_rgba_premultiplied(120, 215, 255, alpha))
            }
            AppTheme::Neon => {
                let alpha = (40.0 + hover * 160.0) as u8;
                egui::Stroke::new(1.0_f32, egui::Color32::from_rgba_premultiplied(255, 0, 128, alpha))
            }
            _ => {
                if hover > 0.05 {
                    egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha((35.0 * hover) as u8))
                } else {
                    egui::Stroke::NONE
                }
            }
        }
    }

    fn paint_aero_glass(&self, painter: &egui::Painter, rect: egui::Rect, hover: f32) {
        if self.config.theme != AppTheme::Aero {
            return;
        }

        let glass_rect = egui::Rect::from_min_max(
            rect.min,
            egui::pos2(rect.max.x, rect.min.y + rect.height() * 0.44),
        );
        painter.rect_filled(
            glass_rect,
            egui::Rounding { nw: 18.0, ne: 18.0, sw: 0.0, se: 0.0 },
            egui::Color32::from_rgba_premultiplied(255, 255, 255, (14.0 + hover * 18.0) as u8),
        );

        painter.line_segment(
            [
                rect.left_top() + egui::vec2(14.0, 1.0),
                rect.right_top() + egui::vec2(-14.0, 1.0),
            ],
            egui::Stroke::new(
                1.5_f32,
                egui::Color32::from_rgba_premultiplied(160, 230, 255, (110.0 + hover * 90.0) as u8),
            ),
        );
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

    fn cache_path(config_dir: &Path) -> PathBuf {
        config_dir.join("releases_cache.json")
    }

    fn load_cached_releases(config_dir: &Path) -> Option<(Vec<GithubRelease>, Option<String>)> {
        let path = Self::cache_path(config_dir);
        let content = fs::read_to_string(path).ok()?;
        let cache: ReleasesCache = serde_json::from_str(&content).ok()?;
        if cache.releases.is_empty() {
            None
        } else {
            Some((cache.releases, cache.etag))
        }
    }

    fn save_cached_releases(config_dir: &Path, releases: &[GithubRelease], etag: Option<&str>) {
        let path = Self::cache_path(config_dir);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cache = ReleasesCache {
            etag: etag.map(|s| s.to_string()),
            releases: releases.to_vec(),
            updated_at: stamp,
        };
        if let Ok(json) = serde_json::to_string_pretty(&cache) {
            fs::write(path, json).ok();
        }
    }

    fn default_releases_catalog() -> Vec<GithubRelease> {
        vec![
            GithubRelease {
                name: Some("Mon Bazou".to_string()),
                tag_name: "MonBazou1".to_string(),
                body: Some("Build your own car, chop wood, sell maple syrup and pizza delivery in Canada!".to_string()),
                assets: vec![
                    GithubAsset {
                        name: "Mon.Bazou.7z".to_string(),
                        browser_download_url: "https://github.com/FlatronTech/Vault64-games/releases/download/MonBazou1/Mon.Bazou.7z".to_string(),
                        size: 650_000_000,
                    },
                ],
                html_url: Some("https://github.com/FlatronTech/Vault64-games/releases/tag/MonBazou1".to_string()),
            },
            GithubRelease {
                name: Some("The long drive".to_string()),
                tag_name: "tld".to_string(),
                body: Some("A post-apocalyptic road trip game through an endless desert.".to_string()),
                assets: vec![
                    GithubAsset {
                        name: "The.Long.Drive.7z".to_string(),
                        browser_download_url: "https://github.com/FlatronTech/Vault64-games/releases/download/tld/The.Long.Drive.7z".to_string(),
                        size: 550_000_000,
                    },
                ],
                html_url: Some("https://github.com/FlatronTech/Vault64-games/releases/tag/tld".to_string()),
            },
            GithubRelease {
                name: Some("People Playground".to_string()),
                tag_name: "pp".to_string(),
                body: Some("Shoot, stab, burn, poison, tear, vaporise, or crush ragdolls in a large open space.".to_string()),
                assets: vec![
                    GithubAsset {
                        name: "People.Playground.7z".to_string(),
                        browser_download_url: "https://github.com/FlatronTech/Vault64-games/releases/download/pp/People.Playground.7z".to_string(),
                        size: 150_000_000,
                    },
                ],
                html_url: Some("https://github.com/FlatronTech/Vault64-games/releases/tag/pp".to_string()),
            },
        ]
    }

    fn check_app_updates(&mut self) {
        if self.checking_update {
            return;
        }

        self.checking_update = true;
        self.app_update_info = "Checking for updates...".to_string();

        let tx = self.tx.clone();
        thread::spawn(move || {
            let client = match reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(4))
                .timeout(Duration::from_secs(8))
                .user_agent("Vault64-Launcher")
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(AppMsg::AppUpdateFetched(Err(e.to_string())));
                    return;
                }
            };

            let url = "https://api.github.com/repos/FlatronTech/Vault64/releases/latest";

            let mut last_err = String::new();
            for attempt in 0..2 {
                if attempt > 0 {
                    thread::sleep(Duration::from_millis(1500));
                }

                match client
                    .get(url)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .send()
                {
                    Ok(response) => {
                        let status = response.status();
                        if status.is_success() {
                            match response.json::<GithubRelease>() {
                                Ok(release) => {
                                    let _ = tx.send(AppMsg::AppUpdateFetched(Ok(release)));
                                    return;
                                }
                                Err(e) => {
                                    last_err = format!("Parse error: {}", e);
                                }
                            }
                        } else if status.as_u16() == 504 || status.as_u16() == 503 || status.as_u16() == 502 {
                            last_err = "Update server temporarily busy (GitHub 504). Will check again later.".to_string();
                        } else if status.as_u16() == 403 {
                            last_err = "GitHub rate limit reached. Will check later.".to_string();
                        } else {
                            last_err = format!("Server status: {}", status);
                        }
                    }
                    Err(e) => {
                        if e.is_timeout() {
                            last_err = "Update check timed out (GitHub slow / 504). Will retry later.".to_string();
                        } else {
                            last_err = e.to_string();
                        }
                    }
                }
            }

            let _ = tx.send(AppMsg::AppUpdateFetched(Err(last_err)));
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
            .connect_timeout(Duration::from_secs(10))
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

    fn do_login_supabase(&mut self) {
        if self.login_pending {
            return;
        }
        self.login_pending = true;
        self.login_error = None;

        let email = self.login_email.trim().to_string();
        let password = self.login_password.clone();
        let tx = self.tx.clone();

        thread::spawn(move || {
            let result = supabase_sign_in(&email, &password);
            let _ = tx.send(AppMsg::LoginResult(result));
        });
    }

    fn do_register_supabase(&mut self) {
        if self.reg_pending {
            return;
        }

        // Client-side validation
        let email = self.reg_email.trim().to_string();
        let username = self.reg_username.trim().to_string();
        let password = self.reg_password.clone();
        let password2 = self.reg_password2.clone();

        if email.is_empty() || username.is_empty() || password.is_empty() {
            self.reg_error = Some("All fields are required.".to_string());
            return;
        }
        if password != password2 {
            self.reg_error = Some("Passwords do not match.".to_string());
            return;
        }
        if password.len() < 6 {
            self.reg_error = Some("Password must be at least 6 characters.".to_string());
            return;
        }

        self.reg_pending = true;
        self.reg_error = None;
        self.reg_success = None;

        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = supabase_sign_up(&email, &password, &username);
            let _ = tx.send(AppMsg::RegisterResult(result));
        });
    }

    fn fetch_releases(&mut self) {
        if self.is_fetching_releases {
            return;
        }
        self.is_fetching_releases = true;
        let tx = self.tx.clone();
        let config_dir = self.config_dir.clone();

        thread::spawn(move || {
            let cached = Self::load_cached_releases(&config_dir);
            let (cached_releases, cached_etag) = match cached {
                Some((r, etag)) => (Some(r), etag),
                None => (Some(Self::default_releases_catalog()), None),
            };

            let client = match reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(4))
                .timeout(Duration::from_secs(9))
                .user_agent("Vault64-Launcher")
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    if let Some(r) = cached_releases {
                        let _ = tx.send(AppMsg::ReleasesFetched(Ok(r)));
                        let _ = tx.send(AppMsg::ReleasesWarning(Some(format!(
                            "Offline mode: {}",
                            e
                        ))));
                    } else {
                        let _ = tx.send(AppMsg::ReleasesFetched(Err(e.to_string())));
                    }
                    return;
                }
            };

            let url = "https://api.github.com/repos/FlatronTech/Vault64-games/releases";

            let mut last_err = String::new();
            for attempt in 0..2 {
                if attempt > 0 {
                    thread::sleep(Duration::from_millis(1500));
                }

                let mut req = client
                    .get(url)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28");

                if let Some(etag) = &cached_etag {
                    req = req.header("If-None-Match", etag.as_str());
                }

                match req.send() {
                    Ok(response) => {
                        let status = response.status();
                        if status == reqwest::StatusCode::NOT_MODIFIED {
                            if let Some(r) = cached_releases {
                                let _ = tx.send(AppMsg::ReleasesFetched(Ok(r)));
                                let _ = tx.send(AppMsg::ReleasesWarning(None));
                                return;
                            }
                        } else if status.is_success() {
                            let etag = response
                                .headers()
                                .get("ETag")
                                .and_then(|v| v.to_str().ok())
                                .map(|s| s.to_string());

                            match response.json::<Vec<GithubRelease>>() {
                                Ok(releases) => {
                                    Self::save_cached_releases(
                                        &config_dir,
                                        &releases,
                                        etag.as_deref(),
                                    );
                                    let _ = tx.send(AppMsg::ReleasesFetched(Ok(releases)));
                                    let _ = tx.send(AppMsg::ReleasesWarning(None));
                                    return;
                                }
                                Err(e) => {
                                    last_err = format!("Failed to parse releases: {}", e);
                                }
                            }
                        } else if status.as_u16() == 504 || status.as_u16() == 503 || status.as_u16() == 502 {
                            last_err = format!("GitHub API server busy ({})", status);
                        } else if status.as_u16() == 403 {
                            last_err = "GitHub API rate limit reached (60 req/hr).".to_string();
                        } else {
                            last_err = format!("API status: {}", status);
                        }
                    }
                    Err(e) => {
                        if e.is_timeout() {
                            last_err = "Connection timed out (GitHub slow / 504).".to_string();
                        } else {
                            last_err = e.to_string();
                        }
                    }
                }
            }

            if let Some(r) = cached_releases {
                let _ = tx.send(AppMsg::ReleasesFetched(Ok(r)));
                let _ = tx.send(AppMsg::ReleasesWarning(Some(format!(
                    "GitHub is busy or unreachable ({}). Showing cached games catalogue.",
                    last_err
                ))));
            } else {
                let _ = tx.send(AppMsg::ReleasesFetched(Err(last_err)));
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
                .connect_timeout(Duration::from_secs(10))
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

    fn format_playtime(seconds: u64) -> String {
        let mins = seconds / 60;
        if mins == 0 {
            if seconds > 0 {
                "< 1 min".to_string()
            } else {
                "0 min".to_string()
            }
        } else if mins < 60 {
            format!("{} min", mins)
        } else {
            let hours = mins / 60;
            let rem_mins = mins % 60;
            if rem_mins == 0 {
                format!("{} h", hours)
            } else {
                format!("{} h {} min", hours, rem_mins)
            }
        }
    }

    fn find_icon_in_dir(dir: &Path) -> Option<PathBuf> {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension() {
                        if ext.to_string_lossy().to_lowercase() == "ico" {
                            return Some(path);
                        }
                    }
                }
            }
        }
        None
    }

    fn load_icon_texture(&mut self, ctx: &egui::Context, safe_name: &str) {
        if self.game_icons.contains_key(safe_name) {
            return;
        }

        let cached = self.icons_dir.join(format!("{}.ico", safe_name));
        let icon_path = if cached.exists() {
            Some(cached)
        } else {
            let game_dir = self.games_dir.join(safe_name);
            let in_game = game_dir.join("icon.ico");
            if in_game.exists() {
                Some(in_game)
            } else {
                Self::find_icon_in_dir(&game_dir)
            }
        };

        if let Some(path) = icon_path {
            if let Ok(bytes) = fs::read(&path) {
                if let Ok(img) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Ico)
                    .or_else(|_| image::load_from_memory(&bytes))
                {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    let color_image =
                        egui::ColorImage::from_rgba_unmultiplied(size, &rgba.into_raw());
                    let texture = ctx.load_texture(
                        format!("game_icon_{}", safe_name),
                        color_image,
                        egui::TextureOptions::LINEAR,
                    );
                    self.game_icons.insert(safe_name.to_string(), texture);
                }
            }
        }
    }

    fn download_release_icons(&self, releases: &[GithubRelease]) {
        for release in releases {
            let game_name = release.name.clone().unwrap_or_else(|| release.tag_name.clone());
            let safe_name = Self::safe_name(&game_name);
            let dest = self.icons_dir.join(format!("{}.ico", safe_name));

            if dest.exists() {
                continue;
            }

            if let Some(asset) = release.assets.iter().find(|a| {
                let n = a.name.to_lowercase();
                n == "icon.ico" || n.ends_with(".ico")
            }) {
                let url = asset.browser_download_url.clone();
                let tx = self.tx.clone();
                let game_id = safe_name.clone();

                thread::spawn(move || {
                    let client = reqwest::blocking::Client::builder()
                        .connect_timeout(Duration::from_secs(4))
                        .timeout(Duration::from_secs(8))
                        .user_agent("Vault64-Launcher")
                        .build();
                    if let Ok(client) = client {
                        if let Ok(resp) = client.get(&url).send() {
                            if resp.status().is_success() {
                                if let Ok(bytes) = resp.bytes() {
                                    if fs::write(&dest, &bytes).is_ok() {
                                        let _ = tx.send(AppMsg::IconDownloaded(game_id));
                                    }
                                }
                            }
                        }
                    }
                });
            }
        }
    }

    fn ensure_icons_loaded(&mut self, ctx: &egui::Context) {
        let mut check_list = Vec::new();
        if let Some(releases) = &self.releases {
            for r in releases {
                let name = r.name.clone().unwrap_or_else(|| r.tag_name.clone());
                let safe = Self::safe_name(&name);
                if !self.game_icons.contains_key(&safe) {
                    check_list.push(safe);
                }
            }
        }
        for safe in &self.config.library {
            if !self.game_icons.contains_key(safe) && !check_list.contains(safe) {
                check_list.push(safe.clone());
            }
        }
        for safe in check_list {
            self.load_icon_texture(ctx, &safe);
        }
    }

    fn ui_game_icon(
        &self,
        ui: &mut egui::Ui,
        safe_name: &str,
        game_name: &str,
        alpha: f32,
        size: f32,
        fallback_emoji: &str,
    ) {
        let rounding = if size > 60.0 { 20.0 } else { 14.0 };
        if let Some(texture) = self.game_icons.get(safe_name) {
            egui::Frame::none()
                .rounding(rounding)
                .inner_margin(0.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::Image::new(texture)
                            .fit_to_exact_size(egui::vec2(size, size))
                            .rounding(rounding),
                    );
                });
        } else {
            let icon_color = Self::hash_color(game_name);
            let margin = if size > 60.0 {
                egui::vec2(24.0, 22.0)
            } else {
                egui::vec2(14.0, 12.0)
            };
            let emoji_size = if size > 60.0 { 42.0 } else { 24.0 };

            egui::Frame::none()
                .fill(Self::fade(icon_color, 0.18 * alpha))
                .rounding(rounding)
                .inner_margin(margin)
                .show(ui, |ui| {
                    ui.label(egui::RichText::new(fallback_emoji).size(emoji_size));
                });
        }
    }

    fn launch_game(&mut self, target_dir: &Path, safe_name: &str) {
        if let Some(exe_path) = Self::find_executable(target_dir) {
            match Command::new(&exe_path)
                .current_dir(exe_path.parent().unwrap_or(target_dir))
                .spawn()
            {
                Ok(mut child) => {
                    self.download_status = "".to_string();
                    self.active_games.insert(safe_name.to_string());

                    let tx = self.tx.clone();
                    let game_id = safe_name.to_string();

                    thread::spawn(move || {
                        loop {
                            thread::sleep(Duration::from_secs(5));
                            match child.try_wait() {
                                Ok(Some(_)) => {
                                    let _ = tx.send(AppMsg::GameExited(game_id));
                                    break;
                                }
                                Ok(None) => {
                                    let _ = tx.send(AppMsg::PlaytimeTick {
                                        game: game_id.clone(),
                                        seconds: 5,
                                    });
                                }
                                Err(_) => {
                                    let _ = tx.send(AppMsg::GameExited(game_id));
                                    break;
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    self.download_status = format!("Error launching game: {}", e);
                }
            }
        } else {
            self.download_status = "No executable found in game folder".to_string();
        }
    }

    fn process_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                AppMsg::ReleasesFetched(Ok(data)) => {
                    self.is_fetching_releases = false;
                    self.download_release_icons(&data);
                    self.releases = Some(data);
                    self.fetch_error = None;
                }
                AppMsg::ReleasesFetched(Err(e)) => {
                    self.is_fetching_releases = false;
                    if self.releases.is_none() {
                        self.fetch_error = Some(e);
                    }
                }
                AppMsg::ReleasesWarning(warning) => {
                    self.fetch_warning = warning;
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
                    self.app_update_info = e;
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
                    self.load_icon_texture(ctx, &name);
                    ctx.request_repaint();
                }
                AppMsg::DownloadError(err) => {
                    self.is_downloading = false;
                    self.download_status = format!("Error: {}", err);
                }
                AppMsg::LoginResult(Ok(username)) => {
                    self.login_pending = false;
                    self.login_error = None;
                    self.login_username = username.clone();

                    if self.remember_me {
                        let saved = format!("{}|{}", username, self.login_email);
                        self.config.remembered_username = Some(saved);
                    } else {
                        self.config.remembered_username = None;
                    }

                    self.save_config();
                    self.state = AppState::Launcher;
                    self.tab_switch_counter += 1;
                    self.fetch_releases();
                    self.check_app_updates();
                }
                AppMsg::LoginResult(Err(msg)) => {
                    self.login_pending = false;
                    self.login_error = Some(msg);
                }
                AppMsg::RegisterResult(Ok(username)) => {
                    self.reg_pending = false;
                    self.reg_error = None;
                    self.reg_success = Some(format!(
                        "Account created! You can now log in as {}.",
                        username
                    ));
                    self.login_email = self.reg_email.clone();
                    self.show_register = false;
                }
                AppMsg::RegisterResult(Err(msg)) => {
                    self.reg_pending = false;
                    self.reg_error = Some(msg);
                }
                AppMsg::AccountUpdateResult(Ok(success)) => {
                    self.acc_pending = false;
                    self.acc_curr_password.clear();
                    self.acc_new_password.clear();
                    self.acc_new_password2.clear();

                    match success {
                        AccountUpdateSuccess::DisplayName(new_name) => {
                            self.login_username = new_name.clone();
                            self.acc_display_name = new_name.clone();
                            if self.remember_me {
                                self.config.remembered_username =
                                    Some(format!("{}|{}", self.login_username, self.login_email));
                                self.save_config();
                            }
                            self.acc_status = Some((false, format!("Display name updated to '{}'!", new_name)));
                        }
                        AccountUpdateSuccess::Email(new_email) => {
                            self.login_email = new_email.clone();
                            self.acc_new_email = new_email.clone();
                            if self.remember_me {
                                self.config.remembered_username =
                                    Some(format!("{}|{}", self.login_username, self.login_email));
                                self.save_config();
                            }
                            self.acc_status = Some((
                                false,
                                format!("Email updated to '{}'! (Check inbox if confirmation is required)", new_email),
                            ));
                        }
                        AccountUpdateSuccess::Password => {
                            self.acc_status = Some((false, "Password changed successfully!".to_string()));
                        }
                    }
                    ctx.request_repaint();
                }
                AppMsg::AccountUpdateResult(Err(err)) => {
                    self.acc_pending = false;
                    self.acc_status = Some((true, err));
                    ctx.request_repaint();
                }
                AppMsg::AccountDeleteResult(res) => {
                    self.acc_pending = false;
                    match res {
                        Ok(()) => {
                            self.acc_status = Some((false, "Account successfully deleted.".to_string()));
                            self.acc_delete_password.clear();
                            self.acc_delete_confirm = false;
                            self.config.remembered_username = None;
                            self.remember_me = false;
                            self.save_config();
                            self.login_password.clear();
                            self.login_email.clear();
                            self.login_username.clear();
                            self.state = AppState::Login;
                            self.reg_success = Some("Your account has been deleted permanently.".to_string());
                        }
                        Err(err) => {
                            self.acc_status = Some((true, format!("Failed to delete account: {}", err)));
                        }
                    }
                    ctx.request_repaint();
                }
                AppMsg::RestoreWindow => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.request_repaint();
                }
                AppMsg::PlaytimeTick { game, seconds } => {
                    *self.config.playtime.entry(game).or_insert(0) += seconds;
                    self.save_config();
                    ctx.request_repaint();
                }
                AppMsg::GameExited(game) => {
                    self.active_games.remove(&game);
                    self.save_config();
                    ctx.request_repaint();
                }
                AppMsg::IconDownloaded(game) => {
                    self.load_icon_texture(ctx, &game);
                    ctx.request_repaint();
                }
            }
        }
    }

    fn ui_login(&mut self, ctx: &egui::Context) {
        let alpha = ctx.animate_value_with_time(egui::Id::new("login_fade_in"), 1.0, 0.45);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                let top_space = if self.show_register {
                    (ui.available_height() / 9.0).max(10.0)
                } else {
                    (ui.available_height() / 6.0).max(20.0)
                };
                ui.add_space(top_space);

                let bg = self.card_bg();
                let accent = self.accent();

                egui::Frame::none()
                    .fill(Self::fade(bg, alpha))
                    .rounding(24.0)
                    .inner_margin(36.0)
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        if self.is_dark() {
                            egui::Color32::from_rgba_unmultiplied(255, 255, 255, (25.0 * alpha) as u8)
                        } else {
                            egui::Color32::from_rgba_unmultiplied(0, 0, 0, (20.0 * alpha) as u8)
                        },
                    ))
                    .shadow(egui::epaint::Shadow {
                        offset: egui::vec2(0.0, 12.0),
                        blur: 36.0,
                        spread: 0.0,
                        color: egui::Color32::from_black_alpha(if self.is_dark() { 130 } else { 35 }),
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(340.0);

                        // App Icon + Glow
                        if let Some(ref icon) = self.app_icon_texture {
                            egui::Frame::none()
                                .rounding(20.0)
                                .inner_margin(6.0)
                                .fill(Self::fade(accent, 0.12 * alpha))
                                .stroke(egui::Stroke::new(1.5_f32, Self::fade(accent, 0.5 * alpha)))
                                .shadow(egui::epaint::Shadow {
                                    offset: egui::vec2(0.0, 4.0),
                                    blur: 18.0,
                                    spread: 0.0,
                                    color: Self::fade(accent, 0.35 * alpha),
                                })
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Image::new(icon)
                                            .fit_to_exact_size(egui::vec2(68.0, 68.0))
                                            .rounding(16.0),
                                    );
                                });
                        } else {
                            ui.label(egui::RichText::new("🎮").size(52.0));
                        }

                        ui.add_space(10.0);
                        ui.heading(
                            egui::RichText::new("Vault64")
                                .size(34.0)
                                .strong()
                                .color(Self::fade(self.text_color(), alpha)),
                        );
                        ui.label(
                            egui::RichText::new("")
                                .size(13.0)
                                .color(Self::fade(self.subtle_text(), alpha)),
                        );

                        ui.add_space(18.0);

                        // Mode Tab Switcher (Sign In / Register)
                        let tab_bar_bg = if self.is_dark() {
                            egui::Color32::from_rgb(17, 19, 25)
                        } else {
                            egui::Color32::from_rgb(222, 228, 236)
                        };

                        egui::Frame::none()
                            .fill(Self::fade(tab_bar_bg, alpha))
                            .rounding(12.0)
                            .inner_margin(4.0)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let btn_w = (ui.available_width() - 8.0) / 2.0;
                                    let is_login = !self.show_register;

                                    let login_btn = egui::Button::new(
                                        egui::RichText::new("Sign In")
                                            .size(13.0)
                                            .strong()
                                            .color(if is_login {
                                                egui::Color32::WHITE
                                            } else {
                                                self.subtle_text()
                                            }),
                                    )
                                    .fill(if is_login { accent } else { egui::Color32::TRANSPARENT })
                                    .rounding(9.0);

                                    if ui.add_sized([btn_w, 32.0], login_btn).clicked() {
                                        self.show_register = false;
                                        self.login_error = None;
                                        self.reg_error = None;
                                        self.reg_success = None;
                                    }

                                    let reg_btn = egui::Button::new(
                                        egui::RichText::new("Register")
                                            .size(13.0)
                                            .strong()
                                            .color(if !is_login {
                                                egui::Color32::WHITE
                                            } else {
                                                self.subtle_text()
                                            }),
                                    )
                                    .fill(if !is_login { accent } else { egui::Color32::TRANSPARENT })
                                    .rounding(9.0);

                                    if ui.add_sized([btn_w, 32.0], reg_btn).clicked() {
                                        self.show_register = true;
                                        self.login_error = None;
                                        self.reg_error = None;
                                    }
                                });
                            });

                        ui.add_space(14.0);

                        if self.show_register {
                            // ---- REGISTER FORM ----
                            ui.label(
                                egui::RichText::new("EMAIL ADDRESS")
                                    .color(Self::fade(self.subtle_text(), alpha))
                                    .size(11.0)
                                    .strong(),
                            );
                            let reg_email_edit = egui::TextEdit::singleline(&mut self.reg_email)
                                .hint_text("user@example.com")
                                .desired_width(f32::INFINITY)
                                .margin(egui::vec2(12.0, 10.0));
                            ui.add(reg_email_edit);

                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("USERNAME")
                                    .color(Self::fade(self.subtle_text(), alpha))
                                    .size(11.0)
                                    .strong(),
                            );
                            let reg_user_edit = egui::TextEdit::singleline(&mut self.reg_username)
                                .hint_text("display name")
                                .desired_width(f32::INFINITY)
                                .margin(egui::vec2(12.0, 10.0));
                            ui.add(reg_user_edit);

                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("PASSWORD")
                                    .color(Self::fade(self.subtle_text(), alpha))
                                    .size(11.0)
                                    .strong(),
                            );
                            let reg_pass_edit = egui::TextEdit::singleline(&mut self.reg_password)
                                .password(true)
                                .hint_text("min. 6 characters")
                                .desired_width(f32::INFINITY)
                                .margin(egui::vec2(12.0, 10.0));
                            ui.add(reg_pass_edit);

                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("CONFIRM PASSWORD")
                                    .color(Self::fade(self.subtle_text(), alpha))
                                    .size(11.0)
                                    .strong(),
                            );
                            let reg_pass2_edit = egui::TextEdit::singleline(&mut self.reg_password2)
                                .password(true)
                                .hint_text("repeat password")
                                .desired_width(f32::INFINITY)
                                .margin(egui::vec2(12.0, 10.0));
                            let reg_pass2_response = ui.add(reg_pass2_edit);

                            if let Some(err) = &self.reg_error {
                                ui.add_space(8.0);
                                egui::Frame::none()
                                    .fill(Self::fade(self.danger_color(), 0.12 * alpha))
                                    .stroke(egui::Stroke::new(1.0_f32, Self::fade(self.danger_color(), 0.35 * alpha)))
                                    .rounding(8.0)
                                    .inner_margin(egui::vec2(12.0, 8.0))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(egui::RichText::new("⚠️").size(14.0));
                                            ui.colored_label(self.danger_color(), err);
                                        });
                                    });
                            }
                            if let Some(ok) = &self.reg_success {
                                ui.add_space(8.0);
                                egui::Frame::none()
                                    .fill(Self::fade(self.success_color(), 0.12 * alpha))
                                    .stroke(egui::Stroke::new(1.0_f32, Self::fade(self.success_color(), 0.35 * alpha)))
                                    .rounding(8.0)
                                    .inner_margin(egui::vec2(12.0, 8.0))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(egui::RichText::new("✅").size(14.0));
                                            ui.colored_label(self.success_color(), ok);
                                        });
                                    });
                            }

                            ui.add_space(16.0);

                            let mut do_register = false;

                            if self.reg_pending {
                                ui.add_sized([ui.available_width(), 44.0], egui::Spinner::new());
                            } else {
                                if ui
                                    .add_sized(
                                        [ui.available_width(), 44.0],
                                        egui::Button::new(
                                            egui::RichText::new("C R E A T E   A C C O U N T")
                                                .strong()
                                                .size(14.0)
                                                .color(egui::Color32::WHITE),
                                        )
                                        .fill(accent),
                                    )
                                    .clicked()
                                {
                                    do_register = true;
                                }
                            }

                            if reg_pass2_response.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            {
                                do_register = true;
                            }

                            if do_register {
                                self.do_register_supabase();
                            }
                        } else {
                            // ---- LOGIN FORM ----
                            if let Some(ok) = &self.reg_success.clone() {
                                egui::Frame::none()
                                    .fill(Self::fade(self.success_color(), 0.12 * alpha))
                                    .stroke(egui::Stroke::new(1.0_f32, Self::fade(self.success_color(), 0.35 * alpha)))
                                    .rounding(8.0)
                                    .inner_margin(egui::vec2(12.0, 8.0))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(egui::RichText::new("✅").size(14.0));
                                            ui.colored_label(self.success_color(), ok);
                                        });
                                    });
                                ui.add_space(8.0);
                            }

                            ui.label(
                                egui::RichText::new("EMAIL ADDRESS")
                                    .color(Self::fade(self.subtle_text(), alpha))
                                    .size(11.0)
                                    .strong(),
                            );
                            let email_edit = egui::TextEdit::singleline(&mut self.login_email)
                                .hint_text("user@example.com")
                                .desired_width(f32::INFINITY)
                                .margin(egui::vec2(12.0, 10.0));
                            ui.add(email_edit);

                            ui.add_space(10.0);
                            ui.label(
                                egui::RichText::new("PASSWORD")
                                    .color(Self::fade(self.subtle_text(), alpha))
                                    .size(11.0)
                                    .strong(),
                            );
                            let pass_edit = egui::TextEdit::singleline(&mut self.login_password)
                                .password(true)
                                .hint_text("Enter your password")
                                .desired_width(f32::INFINITY)
                                .margin(egui::vec2(12.0, 10.0));
                            let pass_response = ui.add(pass_edit);

                            ui.add_space(12.0);
                            ui.checkbox(&mut self.remember_me, "Remember me");

                            if let Some(err) = &self.login_error {
                                ui.add_space(8.0);
                                egui::Frame::none()
                                    .fill(Self::fade(self.danger_color(), 0.12 * alpha))
                                    .stroke(egui::Stroke::new(1.0_f32, Self::fade(self.danger_color(), 0.35 * alpha)))
                                    .rounding(8.0)
                                    .inner_margin(egui::vec2(12.0, 8.0))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(egui::RichText::new("⚠️").size(14.0));
                                            ui.colored_label(self.danger_color(), err);
                                        });
                                    });
                            }

                            ui.add_space(16.0);

                            let mut do_login = false;

                            if self.login_pending {
                                ui.add_sized([ui.available_width(), 44.0], egui::Spinner::new());
                            } else {
                                if ui
                                    .add_sized(
                                        [ui.available_width(), 44.0],
                                        egui::Button::new(
                                            egui::RichText::new("S I G N   I N")
                                                .strong()
                                                .size(15.0)
                                                .color(egui::Color32::WHITE),
                                        )
                                        .fill(accent),
                                    )
                                    .clicked()
                                {
                                    do_login = true;
                                }
                            }

                            if pass_response.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            {
                                do_login = true;
                            }

                            if do_login && !self.login_pending {
                                self.do_login_supabase();
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
                    if let Some(ref icon) = self.app_icon_texture {
                        ui.add(
                            egui::Image::new(icon)
                                .fit_to_exact_size(egui::vec2(28.0, 28.0))
                                .rounding(7.0),
                        );
                        ui.add_space(6.0);
                    }

                    ui.label(
                        egui::RichText::new("Vault64")
                            .strong()
                            .size(24.0)
                            .color(self.text_color()),
                    );

                    ui.add_space(20.0);

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

                        let lib_label = format!("📚 Library ({})", self.config.library.len());
                        if self
                            .top_tab_button(
                                ui,
                                ctx,
                                &lib_label,
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

                        ui.add_space(10.0);

                        let chip_bg = if self.is_dark() {
                            egui::Color32::from_rgb(22, 25, 34)
                        } else {
                            egui::Color32::from_rgb(236, 240, 246)
                        };

                        egui::Frame::none()
                            .fill(chip_bg)
                            .rounding(16.0)
                            .inner_margin(egui::vec2(10.0, 5.0))
                            .stroke(egui::Stroke::new(1.0_f32, Self::fade(self.subtle_text(), 0.2)))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    
                                    ui.label(
                                        egui::RichText::new(&self.login_username)
                                            .strong()
                                            .size(13.0)
                                            .color(self.text_color()),
                                    );
                                });
                            });
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
            let page_anim_id = egui::Id::new("page_transition_anim").with(self.tab_switch_counter);
            let anim = ctx.animate_value_with_time(page_anim_id, 1.0, 0.32);

            if anim < 1.0 {
                ctx.request_repaint();
            }

            // Smooth cubic ease-out
            let ease = 1.0 - (1.0 - anim).powi(3);
            let offset = (1.0 - ease) * 22.0;

            let rect = ui.available_rect_before_wrap();
            let mut draw_rect = rect;
            draw_rect.min.y += offset;
            draw_rect.max.y += offset;

            let mut page_ui = ui.child_ui(draw_rect, *ui.layout(), None);
            page_ui.set_clip_rect(draw_rect);

            self.ensure_icons_loaded(ctx);

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

        if (hover > 0.0 && hover < 1.0) || (selected > 0.0 && selected < 1.0) {
            ctx.request_repaint();
        }

        let strength = selected.max(hover * 0.5);

        if strength > 0.01 {
            // Pill glow highlight
            let pill_rect = response.rect.expand2(egui::vec2(6.0, 2.0));
            ui.painter().rect_filled(
                pill_rect,
                egui::Rounding::same(8.0),
                Self::fade(accent, strength * 0.18),
            );

            // Dynamic expanding glowing underline
            let bar_width = response.rect.width() * (0.45 + 0.55 * selected);
            let bar_x = response.rect.center().x - bar_width * 0.5;
            let underline = egui::Rect::from_min_size(
                egui::pos2(bar_x, response.rect.max.y - 1.0),
                egui::vec2(bar_width, 3.0),
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

            if let Some(warning) = &self.fetch_warning {
                egui::Frame::none()
                    .fill(if self.is_dark() {
                        egui::Color32::from_rgb(38, 32, 18)
                    } else {
                        egui::Color32::from_rgb(254, 243, 199)
                    })
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(217, 119, 6)))
                    .rounding(10.0)
                    .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("⚠️").size(16.0));
                            ui.label(
                                egui::RichText::new(warning)
                                    .color(if self.is_dark() {
                                        egui::Color32::from_rgb(251, 191, 36)
                                    } else {
                                        egui::Color32::from_rgb(180, 83, 9)
                                    })
                                    .size(13.0),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button(egui::RichText::new("🔄 Refresh").size(12.0)).clicked() {
                                    retry_fetch = true;
                                }
                            });
                        });
                    });
                ui.add_space(10.0);
            }

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
                            ui.label(egui::RichText::new("⚠️").size(40.0));
                            ui.colored_label(
                                self.danger_color(),
                                format!("GitHub API is currently busy: {}", err),
                            );
                            ui.add_space(8.0);
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
                                    card_id.with("appear").with(self.tab_switch_counter),
                                    1.0,
                                    0.20 + (card_index as f32 * 0.04).min(0.35),
                                );

                                if appear < 1.0 {
                                    ctx.request_repaint();
                                }

                                let offset_x = (1.0 - appear) * 55.0;
                                let alpha = appear;

                                let mut draw_rect = egui::Rect::from_min_size(
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

                                if hover > 0.0 && hover < 1.0 {
                                    ctx.request_repaint();
                                }

                                // Interactive card hover lift
                                let lift = hover * 4.0;
                                draw_rect = draw_rect.translate(egui::vec2(0.0, -lift));

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
                                    .stroke(self.card_stroke(hover))
                                    .inner_margin(16.0)
                                    .shadow(egui::epaint::Shadow {
                                        offset: egui::vec2(0.0, 6.0 + hover * 6.0),
                                        blur: 14.0 + hover * 18.0,
                                        spread: 0.0,
                                        color: shadow_color,
                                    })
                                    .show(&mut card_ui, |ui| {
                                        ui.horizontal(|ui| {
                                            self.ui_game_icon(ui, &safe_name, &game_name, alpha, 48.0, "NO ICON");

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
                                                        let playtime = self.config.playtime.get(&safe_name).copied().unwrap_or(0);
                                                        if playtime > 0 {
                                                            ui.label(
                                                                egui::RichText::new(format!(" {}", Self::format_playtime(playtime)))
                                                                    .size(12.0)
                                                                    .color(Self::fade(self.subtle_text(), alpha)),
                                                            );
                                                            ui.add_space(8.0);
                                                        }
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

                                self.paint_aero_glass(ui.painter(), draw_rect, hover);

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
            self.fetch_warning = None;
            self.fetch_releases();
        }

        if let Some(game) = pending_add {
            if !self.config.library.contains(&game) {
                self.config.library.push(game);
                self.save_config();
            }
        }

        if let Some(idx) = switch_to_details {
            if idx < self.releases.as_ref().map(|r| r.len()).unwrap_or(0) {
                self.selected_release_idx = Some(idx);
                self.change_tab(LauncherTab::GameDetails);
            }
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

            if let Some(warning) = &self.fetch_warning {
                egui::Frame::none()
                    .fill(if self.is_dark() {
                        egui::Color32::from_rgb(38, 32, 18)
                    } else {
                        egui::Color32::from_rgb(254, 243, 199)
                    })
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(217, 119, 6)))
                    .rounding(10.0)
                    .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("⚠️").size(16.0));
                            ui.label(
                                egui::RichText::new(warning)
                                    .color(if self.is_dark() {
                                        egui::Color32::from_rgb(251, 191, 36)
                                    } else {
                                        egui::Color32::from_rgb(180, 83, 9)
                                    })
                                    .size(13.0),
                            );
                        });
                    });
                ui.add_space(10.0);
            }

            let query = self.search_query.trim().to_lowercase();
            let library = self.config.library.clone();

            let mut items: Vec<(usize, GithubRelease)> = Vec::new();
            if let Some(releases) = &self.releases {
                for (idx, release) in releases.iter().enumerate() {
                    let name = release
                        .name
                        .clone()
                        .unwrap_or_else(|| release.tag_name.clone());
                    let safe = Self::safe_name(&name);

                    if library.contains(&safe) && Self::matches_release(release, &query) {
                        items.push((idx, release.clone()));
                    }
                }
            }
            // Also ensure any library games not matched in releases are still shown
            for safe in &library {
                let found = items.iter().any(|(_, r)| {
                    let n = r.name.as_ref().unwrap_or(&r.tag_name);
                    Self::safe_name(n) == *safe
                });
                if !found && (query.is_empty() || safe.to_lowercase().contains(&query)) {
                    items.push((
                        usize::MAX,
                        GithubRelease {
                            name: Some(safe.clone()),
                            tag_name: safe.clone(),
                            body: Some("Installed local game".to_string()),
                            assets: Vec::new(),
                            html_url: None,
                        },
                    ));
                }
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
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
                    } else {

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
                                    card_id.with("appear").with(self.tab_switch_counter),
                                    1.0,
                                    0.20 + (card_index as f32 * 0.04).min(0.35),
                                );

                                if appear < 1.0 {
                                    ctx.request_repaint();
                                }

                                let offset_x = (1.0 - appear) * 55.0;
                                let alpha = appear;

                                let mut draw_rect = egui::Rect::from_min_size(
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

                                if hover > 0.0 && hover < 1.0 {
                                    ctx.request_repaint();
                                }

                                // Interactive card hover lift
                                let lift = hover * 4.0;
                                draw_rect = draw_rect.translate(egui::vec2(0.0, -lift));

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
                                    .stroke(self.card_stroke(hover))
                                    .inner_margin(16.0)
                                    .shadow(egui::epaint::Shadow {
                                        offset: egui::vec2(0.0, 6.0 + hover * 6.0),
                                        blur: 14.0 + hover * 18.0,
                                        spread: 0.0,
                                        color: shadow_color,
                                    })
                                    .show(&mut card_ui, |ui| {
                                        ui.horizontal(|ui| {
                                            self.ui_game_icon(ui, &safe_name, &game_name, alpha, 48.0, "NO ICON");

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

                                                let is_active = self.active_games.contains(&safe_name);
                                                let playtime_secs = self.config.playtime.get(&safe_name).copied().unwrap_or(0);
                                                let playtime_str = Self::format_playtime(playtime_secs);

                                                if is_active {
                                                    ui.horizontal(|ui| {
                                                        ui.label(
                                                            egui::RichText::new("Playing now")
                                                                .color(self.success_color())
                                                                .size(12.0)
                                                                .strong(),
                                                        );
                                                        ui.label(
                                                            egui::RichText::new(format!("•  {}", playtime_str))
                                                                .color(Self::fade(self.subtle_text(), alpha))
                                                                .size(12.0),
                                                        );
                                                    });
                                                } else if playtime_secs > 0 {
                                                    ui.label(
                                                        egui::RichText::new(format!("Playtime: {}", playtime_str))
                                                            .color(Self::fade(self.subtle_text(), alpha))
                                                            .size(12.0),
                                                    );
                                                } else if is_installed {
                                                    ui.label(
                                                        egui::RichText::new("0 min")
                                                            .color(Self::fade(self.subtle_text(), alpha))
                                                            .size(12.0),
                                                    );
                                                } else {
                                                    ui.label(
                                                        egui::RichText::new("Not Installed")
                                                            .color(Self::fade(
                                                                self.subtle_text(),
                                                                alpha,
                                                            ))
                                                            .size(12.0),
                                                    );
                                                }
                                            });

                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if is_installed {
                                                        let is_active = self.active_games.contains(&safe_name);
                                                        if is_active {
                                                            ui.add_sized(
                                                                [110.0, 38.0],
                                                                egui::Button::new(
                                                                    egui::RichText::new("Running")
                                                                        .strong()
                                                                        .color(egui::Color32::WHITE),
                                                                )
                                                                .fill(egui::Color32::from_rgb(40, 140, 70)),
                                                            );
                                                        } else if ui
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
                                                                Some((target_dir.clone(), safe_name.clone()));
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

                                self.paint_aero_glass(ui.painter(), draw_rect, hover);

                                ui.add_space(12.0);
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
            if idx < self.releases.as_ref().map(|r| r.len()).unwrap_or(0) {
                self.selected_release_idx = Some(idx);
                self.change_tab(LauncherTab::GameDetails);
            }
        }

        if let Some((asset, game_name)) = pending_download {
            self.download_and_install(asset, game_name);
        }

        if let Some((target_dir, safe_name)) = pending_launch {
            self.launch_game(&target_dir, &safe_name);
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

                    let anim_id = ui.id().with("details_hero").with(&safe_name).with(self.tab_switch_counter);

                    let alpha = ctx.animate_value_with_time(anim_id, 1.0, 0.32);
                    if alpha < 1.0 {
                        ctx.request_repaint();
                    }

                    let hero_resp = egui::Frame::none()
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
                                self.ui_game_icon(ui, &safe_name, &game_name, alpha, 80.0, "NO
                                ICON");

                                ui.add_space(16.0);

                                ui.vertical(|ui| {
                                    ui.heading(
                                        egui::RichText::new(&game_name)
                                            .size(34.0)
                                            .strong()
                                            .color(Self::fade(self.text_color(), alpha)),
                                    );

                                    ui.add_space(4.0);

                                    let is_active = self.active_games.contains(&safe_name);
                                    let playtime_secs = self.config.playtime.get(&safe_name).copied().unwrap_or(0);
                                    let playtime_str = Self::format_playtime(playtime_secs);

                                    ui.horizontal(|ui| {
                                        if is_installed {
                                            ui.label(
                                                egui::RichText::new("Installed")
                                                    .color(Self::fade(self.success_color(), alpha)),
                                            );
                                        } else {
                                            ui.label(
                                                egui::RichText::new("Not installed")
                                                    .color(Self::fade(self.subtle_text(), alpha)),
                                            );
                                        }

                                        ui.add_space(8.0);
                                        ui.label(
                                            egui::RichText::new(format!("Playtime: {}", playtime_str))
                                                .color(Self::fade(self.subtle_text(), alpha)),
                                        );

                                        if is_active {
                                            ui.add_space(8.0);
                                            ui.label(
                                                egui::RichText::new("Playing now")
                                                    .strong()
                                                    .color(Self::fade(self.success_color(), alpha)),
                                            );
                                        }
                                    });
                                });

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if in_library {
                                            if is_installed {
                                                let is_active = self.active_games.contains(&safe_name);
                                                if is_active {
                                                    ui.add_sized(
                                                        [160.0, 44.0],
                                                        egui::Button::new(
                                                            egui::RichText::new("Running")
                                                                .size(18.0)
                                                                .strong()
                                                                .color(egui::Color32::WHITE),
                                                        )
                                                        .fill(egui::Color32::from_rgb(40, 140, 70)),
                                                    );
                                                } else if ui
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
                                                    pending_launch = Some((target_dir.clone(), safe_name.clone()));
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

                    self.paint_aero_glass(ui.painter(), hero_resp.response.rect, 0.0);
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

        if let Some((target_dir, safe_name)) = pending_launch {
            self.launch_game(&target_dir, &safe_name);
        }
    }

    fn trigger_account_update(
        &mut self,
        new_username: Option<String>,
        new_email: Option<String>,
        new_password: Option<String>,
    ) {
        if self.acc_pending {
            return;
        }

        let current_email = self.login_email.clone();
        let current_password = self.acc_curr_password.clone();

        if current_password.is_empty() {
            self.acc_status = Some((
                true,
                "Please enter your current password to authorize changes.".to_string(),
            ));
            return;
        }

        self.acc_pending = true;
        self.acc_status = None;
        let tx = self.tx.clone();

        thread::spawn(move || {
            let res = supabase_update_account(
                &current_email,
                &current_password,
                new_username,
                new_email,
                new_password,
            );
            let _ = tx.send(AppMsg::AccountUpdateResult(res));
        });
    }

    fn trigger_delete_account(&mut self) {
        if self.acc_pending {
            return;
        }

        let current_email = self.login_email.clone();
        let current_password = self.acc_delete_password.clone();

        if current_password.is_empty() {
            self.acc_status = Some((
                true,
                "Please enter your current password to confirm account deletion.".to_string(),
            ));
            return;
        }

        if !self.acc_delete_confirm {
            self.acc_status = Some((
                true,
                "Please check the confirmation box to authorize irreversible deletion.".to_string(),
            ));
            return;
        }

        self.acc_pending = true;
        self.acc_status = None;
        let tx = self.tx.clone();

        thread::spawn(move || {
            let res = supabase_delete_account(&current_email, &current_password);
            let _ = tx.send(AppMsg::AccountDeleteResult(res));
        });
    }

    fn ui_settings_view(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let mut theme_changed = false;
        let mut check_update = false;
        let mut open_games = false;
        let mut open_config = false;
        let mut view_update = false;
        let mut do_logout = false;

        let mut trigger_name = false;
        let mut trigger_email = false;
        let mut trigger_password = false;
        let mut trigger_delete = false;

        if self.acc_display_name.is_empty() && !self.login_username.is_empty() {
            self.acc_display_name = self.login_username.clone();
        }
        if self.acc_new_email.is_empty() && !self.login_email.is_empty() {
            self.acc_new_email = self.login_email.clone();
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(20.0);
                    ui.set_max_width(700.0);

                    // Sub-tab Navigation: General vs Account
                    ui.horizontal(|ui| {
                        let gen_active = self.settings_subtab == SettingsSubTab::General;
                        let gen_anim = ctx.animate_bool(ui.id().with("set_gen_sub"), gen_active);
                        let gen_btn = egui::Button::new(
                            egui::RichText::new("⚙ General")
                                .size(15.0)
                                .strong()
                                .color(if gen_active {
                                    self.text_color()
                                } else {
                                    self.subtle_text()
                                }),
                        )
                        .fill(Self::fade(self.card_hover_bg(), gen_anim))
                        .rounding(10.0);

                        if ui.add(gen_btn).clicked() {
                            if self.settings_subtab != SettingsSubTab::General {
                                self.settings_subtab = SettingsSubTab::General;
                                self.tab_switch_counter += 1;
                            }
                        }

                        ui.add_space(8.0);

                        let acc_active = self.settings_subtab == SettingsSubTab::Account;
                        let acc_anim = ctx.animate_bool(ui.id().with("set_acc_sub"), acc_active);
                        let acc_btn = egui::Button::new(
                            egui::RichText::new("👤 Account")
                                .size(15.0)
                                .strong()
                                .color(if acc_active {
                                    self.text_color()
                                } else {
                                    self.subtle_text()
                                }),
                        )
                        .fill(Self::fade(self.card_hover_bg(), acc_anim))
                        .rounding(10.0);

                        if ui.add(acc_btn).clicked() {
                            if self.settings_subtab != SettingsSubTab::Account {
                                self.settings_subtab = SettingsSubTab::Account;
                                self.tab_switch_counter += 1;
                            }
                        }
                    });

                    ui.add_space(16.0);

                    match self.settings_subtab {
                        SettingsSubTab::General => {
                            // General Subtab: Appearance, Storage, Updates
                            let gen_resp = egui::Frame::none()
                                .fill(self.card_bg())
                                .rounding(20.0)
                                .stroke(self.card_stroke(0.0))
                                .inner_margin(24.0)
                                .shadow(egui::epaint::Shadow {
                                    offset: egui::vec2(0.0, 8.0),
                                    blur: 22.0,
                                    spread: 0.0,
                                    color: egui::Color32::from_black_alpha(if self.is_dark() {
                                        90
                                    } else {
                                        22
                                    }),
                                })
                                .show(ui, |ui| {
                                    ui.heading("Appearance");
                                    ui.add_space(12.0);

                                    ui.horizontal(|ui| {
                                        if ui
                                            .selectable_value(
                                                &mut self.config.theme,
                                                AppTheme::Dark,
                                                "Dark",
                                            )
                                            .clicked()
                                        {
                                            theme_changed = true;
                                        }

                                        if ui
                                            .selectable_value(
                                                &mut self.config.theme,
                                                AppTheme::Light,
                                                "Light",
                                            )
                                            .clicked()
                                        {
                                            theme_changed = true;
                                        }

                                        if ui
                                            .selectable_value(
                                                &mut self.config.theme,
                                                AppTheme::Neon,
                                                "Neon",
                                            )
                                            .clicked()
                                        {
                                            theme_changed = true;
                                        }

                                        if ui
                                            .selectable_value(
                                                &mut self.config.theme,
                                                AppTheme::Aero,
                                                "Aero Glass",
                                            )
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
                                .stroke(self.card_stroke(0.0))
                                .inner_margin(24.0)
                                .shadow(egui::epaint::Shadow {
                                    offset: egui::vec2(0.0, 8.0),
                                    blur: 22.0,
                                    spread: 0.0,
                                    color: egui::Color32::from_black_alpha(if self.is_dark() {
                                        90
                                    } else {
                                        22
                                    }),
                                })
                                .show(ui, |ui| {
                                    ui.heading("Storage");
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
                                .stroke(self.card_stroke(0.0))
                                .inner_margin(24.0)
                                .shadow(egui::epaint::Shadow {
                                    offset: egui::vec2(0.0, 8.0),
                                    blur: 22.0,
                                    spread: 0.0,
                                    color: egui::Color32::from_black_alpha(if self.is_dark() {
                                        90
                                    } else {
                                        22
                                    }),
                                })
                                .show(ui, |ui| {
                                    ui.heading("Launcher Updates");
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

                                        if self.latest_app_release.is_some()
                                            && !self.app_update_downloading
                                        {
                                            if ui.button("View available update").clicked() {
                                                view_update = true;
                                            }
                                        }
                                    });
                                });

                            self.paint_aero_glass(ui.painter(), gen_resp.response.rect, 0.0);
                        }
                        SettingsSubTab::Account => {
                            // Status Banner
                            if self.acc_pending {
                                egui::Frame::none()
                                    .fill(self.card_bg())
                                    .rounding(14.0)
                                    .inner_margin(14.0)
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.spinner();
                                            ui.label("Saving changes...");
                                        });
                                    });
                                ui.add_space(12.0);
                            } else if let Some((is_err, msg)) = &self.acc_status {
                                let (bg, border, text_col) = if *is_err {
                                    (
                                        egui::Color32::from_rgb(60, 20, 20),
                                        egui::Color32::from_rgb(220, 60, 60),
                                        egui::Color32::from_rgb(255, 120, 120),
                                    )
                                } else {
                                    (
                                        egui::Color32::from_rgb(20, 50, 30),
                                        egui::Color32::from_rgb(50, 180, 80),
                                        egui::Color32::from_rgb(120, 240, 150),
                                    )
                                };

                                egui::Frame::none()
                                    .fill(bg)
                                    .stroke(egui::Stroke::new(1.0_f32, border))
                                    .rounding(14.0)
                                    .inner_margin(14.0)
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(if *is_err { "❌" } else { "✅" });
                                            ui.label(
                                                egui::RichText::new(msg).color(text_col).strong(),
                                            );
                                        });
                                    });
                                ui.add_space(12.0);
                            }

                            // Card 1: User Profile & Display Name
                            let prof_resp = egui::Frame::none()
                                .fill(self.card_bg())
                                .rounding(20.0)
                                .stroke(self.card_stroke(0.0))
                                .inner_margin(24.0)
                                .shadow(egui::epaint::Shadow {
                                    offset: egui::vec2(0.0, 8.0),
                                    blur: 22.0,
                                    spread: 0.0,
                                    color: egui::Color32::from_black_alpha(if self.is_dark() {
                                        90
                                    } else {
                                        22
                                    }),
                                })
                                .show(ui, |ui| {
                                    ui.heading("User Profile");
                                    ui.add_space(6.0);
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Logged in as: {} ({})",
                                            self.login_username, self.login_email
                                        ))
                                        .color(self.subtle_text()),
                                    );

                                    ui.add_space(14.0);
                                    ui.label("Display Name:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.acc_display_name)
                                            .hint_text("Enter new display name")
                                            .desired_width(f32::INFINITY),
                                    );

                                    ui.add_space(8.0);
                                    ui.label("Current Password (required for authorization):");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.acc_curr_password)
                                            .password(true)
                                            .hint_text("Current password")
                                            .desired_width(f32::INFINITY),
                                    );

                                    ui.add_space(12.0);
                                    if ui
                                        .add_sized(
                                            [170.0, 36.0],
                                            egui::Button::new("Save Display Name"),
                                        )
                                        .clicked()
                                    {
                                        trigger_name = true;
                                    }
                                });

                            self.paint_aero_glass(ui.painter(), prof_resp.response.rect, 0.0);

                            ui.add_space(16.0);

                            // Card 2: Email Address
                            let email_resp = egui::Frame::none()
                                .fill(self.card_bg())
                                .rounding(20.0)
                                .stroke(self.card_stroke(0.0))
                                .inner_margin(24.0)
                                .shadow(egui::epaint::Shadow {
                                    offset: egui::vec2(0.0, 8.0),
                                    blur: 22.0,
                                    spread: 0.0,
                                    color: egui::Color32::from_black_alpha(if self.is_dark() {
                                        90
                                    } else {
                                        22
                                    }),
                                })
                                .show(ui, |ui| {
                                    ui.heading("Change Email");
                                    ui.add_space(10.0);

                                    ui.label("New Email Address:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.acc_new_email)
                                            .hint_text("new_email@example.com")
                                            .desired_width(f32::INFINITY),
                                    );

                                    ui.add_space(8.0);
                                    ui.label("Current Password (required for authorization):");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.acc_curr_password)
                                            .password(true)
                                            .hint_text("Current password")
                                            .desired_width(f32::INFINITY),
                                    );

                                    ui.add_space(12.0);
                                    if ui
                                        .add_sized(
                                            [170.0, 36.0],
                                            egui::Button::new("Update Email"),
                                        )
                                        .clicked()
                                    {
                                        trigger_email = true;
                                    }
                                });

                            self.paint_aero_glass(ui.painter(), email_resp.response.rect, 0.0);

                            ui.add_space(16.0);

                            // Card 3: Change Password
                            let pass_resp = egui::Frame::none()
                                .fill(self.card_bg())
                                .rounding(20.0)
                                .stroke(self.card_stroke(0.0))
                                .inner_margin(24.0)
                                .shadow(egui::epaint::Shadow {
                                    offset: egui::vec2(0.0, 8.0),
                                    blur: 22.0,
                                    spread: 0.0,
                                    color: egui::Color32::from_black_alpha(if self.is_dark() {
                                        90
                                    } else {
                                        22
                                    }),
                                })
                                .show(ui, |ui| {
                                    ui.heading("Change Password");
                                    ui.add_space(10.0);

                                    ui.label("Current Password:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.acc_curr_password)
                                            .password(true)
                                            .hint_text("Current password")
                                            .desired_width(f32::INFINITY),
                                    );

                                    ui.add_space(8.0);
                                    ui.label("New Password:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.acc_new_password)
                                            .password(true)
                                            .hint_text("At least 6 characters")
                                            .desired_width(f32::INFINITY),
                                    );

                                    ui.add_space(8.0);
                                    ui.label("Confirm New Password:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.acc_new_password2)
                                            .password(true)
                                            .hint_text("Repeat new password")
                                            .desired_width(f32::INFINITY),
                                    );

                                    ui.add_space(12.0);
                                    if ui
                                        .add_sized(
                                            [170.0, 36.0],
                                            egui::Button::new("Update Password"),
                                        )
                                        .clicked()
                                    {
                                        trigger_password = true;
                                    }
                                });

                            self.paint_aero_glass(ui.painter(), pass_resp.response.rect, 0.0);

                            ui.add_space(16.0);

                            // Card 4: Sign out
                            let signout_resp = egui::Frame::none()
                                .fill(self.card_bg())
                                .rounding(20.0)
                                .stroke(self.card_stroke(0.0))
                                .inner_margin(20.0)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new("Session management")
                                                .color(self.subtle_text()),
                                        );

                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui
                                                    .add_sized(
                                                        [130.0, 34.0],
                                                        egui::Button::new(
                                                            egui::RichText::new("🚪 Sign Out")
                                                                .color(self.danger_color()),
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    do_logout = true;
                                                }
                                            },
                                        );
                                    });
                                });

                            self.paint_aero_glass(ui.painter(), signout_resp.response.rect, 0.0);

                            ui.add_space(16.0);

                            // Card 5: Danger Zone - Delete Account
                            let delete_bg = if self.is_dark() {
                                egui::Color32::from_rgb(32, 16, 20)
                            } else {
                                egui::Color32::from_rgb(255, 242, 242)
                            };
                            let delete_border = egui::Color32::from_rgb(220, 60, 60);

                            egui::Frame::none()
                                .fill(delete_bg)
                                .rounding(20.0)
                                .stroke(egui::Stroke::new(1.2_f32, delete_border))
                                .inner_margin(24.0)
                                .shadow(egui::epaint::Shadow {
                                    offset: egui::vec2(0.0, 8.0),
                                    blur: 24.0,
                                    spread: 0.0,
                                    color: egui::Color32::from_rgba_premultiplied(180, 40, 40, 35),
                                })
                                .show(ui, |ui| {
                                    ui.heading(
                                        egui::RichText::new("Danger Zone: Delete Account")
                                            .color(egui::Color32::from_rgb(240, 70, 70)),
                                    );
                                    ui.add_space(6.0);
                                    ui.label(
                                        egui::RichText::new(
                                            "Permanently delete your profile and account. All data will be wiped irreversibly.",
                                        )
                                        .color(self.subtle_text()),
                                    );

                                    ui.add_space(14.0);
                                    ui.label("Enter your current password to confirm deletion:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.acc_delete_password)
                                            .password(true)
                                            .hint_text("Current password")
                                            .desired_width(f32::INFINITY),
                                    );

                                    ui.add_space(8.0);
                                    ui.checkbox(
                                        &mut self.acc_delete_confirm,
                                        "I understand that my profile and account will be deleted permanently.",
                                    );

                                    ui.add_space(14.0);
                                    let can_delete = !self.acc_delete_password.is_empty()
                                        && self.acc_delete_confirm
                                        && !self.acc_pending;

                                    let delete_btn = egui::Button::new(
                                        egui::RichText::new("Delete My Account Permanently")
                                            .color(egui::Color32::WHITE)
                                            .strong(),
                                    )
                                    .fill(if can_delete {
                                        egui::Color32::from_rgb(195, 30, 30)
                                    } else {
                                        egui::Color32::from_rgb(85, 35, 40)
                                    })
                                    .rounding(10.0);

                                    if ui.add_enabled(can_delete, delete_btn).clicked() {
                                        trigger_delete = true;
                                    }
                                });
                        }
                    }

                    ui.add_space(20.0);
                });
            });

        if trigger_name {
            let new_name = self.acc_display_name.trim().to_string();
            self.trigger_account_update(Some(new_name), None, None);
        }

        if trigger_email {
            let new_email = self.acc_new_email.trim().to_string();
            self.trigger_account_update(None, Some(new_email), None);
        }

        if trigger_password {
            if self.acc_new_password != self.acc_new_password2 {
                self.acc_status = Some((true, "New passwords do not match!".to_string()));
            } else {
                let new_pass = self.acc_new_password.clone();
                self.trigger_account_update(None, None, Some(new_pass));
            }
        }

        if trigger_delete {
            self.trigger_delete_account();
        }

        if do_logout {
            self.config.remembered_username = None;
            self.save_config();
            self.login_password.clear();
            self.acc_curr_password.clear();
            self.state = AppState::Login;
            self.tab = LauncherTab::Store;
            self.tab_switch_counter += 1;
        }

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
        // Intercept close event: hide window to system tray instead of exiting
        if ctx.input(|i| i.viewport().close_requested()) {
            if !self.should_exit {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }

        // Handle tray menu events (Open / Exit)
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(tray) = &self.tray {
                if event.id == tray.open_id {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                } else if event.id == tray.quit_id {
                    std::process::exit(0);
                }
            }
        }

        // Handle tray icon clicks (click or double-click to restore)
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            match event {
                TrayIconEvent::Click { .. } | TrayIconEvent::DoubleClick { .. } => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                _ => {}
            }
        }

        self.process_messages(ctx);

        match self.state {
            AppState::Login => self.ui_login(ctx),
            AppState::Launcher => self.ui_launcher(ctx),
        }

        ctx.request_repaint();
    }
}

fn load_app_icon() -> Option<egui::IconData> {
    let icon_bytes = include_bytes!("../icon.ico");
    if let Ok(img) = image::load_from_memory_with_format(icon_bytes, image::ImageFormat::Ico)
        .or_else(|_| image::load_from_memory(icon_bytes))
    {
        let rgba = img.to_rgba8();
        let (width, height) = (rgba.width(), rgba.height());
        Some(egui::IconData {
            rgba: rgba.into_raw(),
            width,
            height,
        })
    } else {
        None
    }
}

fn load_tray_icon() -> Option<tray_icon::Icon> {
    let icon_bytes = include_bytes!("../icon.ico");
    if let Ok(img) = image::load_from_memory_with_format(icon_bytes, image::ImageFormat::Ico)
        .or_else(|_| image::load_from_memory(icon_bytes))
    {
        let rgba = img.to_rgba8();
        let (width, height) = (rgba.width(), rgba.height());
        tray_icon::Icon::from_rgba(rgba.into_raw(), width, height).ok()
    } else {
        None
    }
}

struct AppTray {
    _tray: TrayIcon,
    open_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
}

impl AppTray {
    fn new(ctx: egui::Context, tx: crossbeam_channel::Sender<AppMsg>) -> Option<Self> {
        let icon = load_tray_icon()?;
        let menu = Menu::new();
        let open_item = MenuItem::new("Open Vault64", true, None);
        let quit_item = MenuItem::new("Exit", true, None);
        let open_id = open_item.id().clone();
        let quit_id = quit_item.id().clone();

        menu.append(&open_item).ok()?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;
        menu.append(&quit_item).ok()?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Vault64 Launcher")
            .with_icon(icon)
            .build()
            .ok()?;

        let t_open_id = open_id.clone();
        let t_quit_id = quit_id.clone();
        let bg_ctx = ctx;
        let bg_tx = tx;

        // Dedicated background thread: processes Tray and Menu clicks even when
        // the application window is hidden or suspended by the OS!
        std::thread::spawn(move || {
            let menu_rx = MenuEvent::receiver();
            let tray_rx = TrayIconEvent::receiver();
            loop {
                crossbeam_channel::select! {
                    recv(menu_rx) -> msg => {
                        if let Ok(event) = msg {
                            if event.id == t_quit_id {
                                std::process::exit(0);
                            } else if event.id == t_open_id {
                                let _ = bg_tx.send(AppMsg::RestoreWindow);
                                bg_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                                bg_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                                bg_ctx.request_repaint();
                            }
                        }
                    }
                    recv(tray_rx) -> msg => {
                        if let Ok(event) = msg {
                            match event {
                                TrayIconEvent::Click { .. } | TrayIconEvent::DoubleClick { .. } => {
                                    let _ = bg_tx.send(AppMsg::RestoreWindow);
                                    bg_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                                    bg_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                                    bg_ctx.request_repaint();
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        });

        Some(Self {
            _tray: tray,
            open_id,
            quit_id,
        })
    }
}

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1040.0, 700.0])
        .with_min_inner_size([820.0, 560.0])
        .with_title("Vault64 Launcher");

    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
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
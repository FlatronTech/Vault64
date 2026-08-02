#![windows_subsystem = "windows"]

use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::PathBuf,
    sync::Arc,
    thread,
};

#[derive(Deserialize, Clone)]
struct GithubRelease {
    name: Option<String>,
    tag_name: String,
    body: Option<String>,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize, Clone)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct AppConfig {
    remembered_username: Option<String>,
    #[serde(default)] // Pozwala załadować starą konfigurację z pliku bez sypania błędami
    library: Vec<String>,
}

enum AppMsg {
    ReleasesFetched(Result<Vec<GithubRelease>, String>),
    DownloadProgress(f32, String),
    DownloadComplete(String),
    DownloadError(String),
}

#[derive(PartialEq)]
enum AppState {
    Login,
    Launcher,
}

#[derive(PartialEq, Clone, Copy)]
enum LauncherTab {
    Store,
    Library,
    GameDetails,
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
    tx: Sender<AppMsg>,
    rx: Receiver<AppMsg>,
    is_downloading: bool,
    download_progress: f32,
    download_status: String,
}

impl Vault64App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.window_rounding = egui::Rounding::same(12.0);
        visuals.panel_fill = egui::Color32::from_rgb(18, 19, 24); 
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(26, 27, 34); 
        visuals.widgets.noninteractive.rounding = egui::Rounding::same(8.0);
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(36, 38, 48);
        visuals.widgets.inactive.rounding = egui::Rounding::same(8.0);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(52, 55, 70);
        visuals.widgets.hovered.rounding = egui::Rounding::same(8.0);
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(0, 132, 255);
        visuals.widgets.active.rounding = egui::Rounding::same(8.0);
        visuals.selection.bg_fill = egui::Color32::from_rgb(0, 132, 255);
        cc.egui_ctx.set_visuals(visuals);

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

        let mut app = Self {
            state,
            tab: LauncherTab::Store,
            previous_tab: LauncherTab::Store,
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
            tx,
            rx,
            is_downloading: false,
            download_progress: 0.0,
            download_status: String::new(),
        };

        if app.state == AppState::Launcher {
            app.fetch_releases();
        }

        app
    }

    fn save_config(&self) {
        let config_path = self.config_dir.join("config.json");
        if let Ok(json) = serde_json::to_string_pretty(&self.config) {
            fs::write(config_path, json).ok();
        }
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
                        if let Ok(releases) = response.json::<Vec<GithubRelease>>() {
                            let _ = tx.send(AppMsg::ReleasesFetched(Ok(releases)));
                        } else {
                            let _ = tx.send(AppMsg::ReleasesFetched(Err("Failed to parse JSON".into())));
                        }
                    } else {
                        let _ = tx.send(AppMsg::ReleasesFetched(Err(format!("API Error: {}", response.status()))));
                    }
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::ReleasesFetched(Err(e.to_string())));
                }
            }
        });
    }

    fn find_executable(dir: &PathBuf) -> Option<PathBuf> {
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

    // Tworzenie skrótu na pulpicie po pobraniu!
    fn create_desktop_shortcut(game_name: &str, exe_path: &PathBuf) {
        #[cfg(target_os = "windows")]
        {
            if let Some(user_dirs) = directories::UserDirs::new() {
                if let Some(desktop_dir) = user_dirs.desktop_dir() {
                    let shortcut_path = desktop_dir.join(format!("{}.lnk", game_name));
                    
                    let exe_str = exe_path.display().to_string().replace("\"", "`\"");
                    let work_dir = exe_path.parent().unwrap_or(exe_path).display().to_string().replace("\"", "`\"");
                    let shortcut_str = shortcut_path.display().to_string().replace("\"", "`\"");

                    let ps_script = format!(
                        "$wshell = New-Object -ComObject WScript.Shell;\n\
                         $shortcut = $wshell.CreateShortcut('{}');\n\
                         $shortcut.TargetPath = '{}';\n\
                         $shortcut.WorkingDirectory = '{}';\n\
                         $shortcut.Save();",
                        shortcut_str, exe_str, work_dir
                    );

                    let _ = std::process::Command::new("powershell")
                        .args(&["-NoProfile", "-NonInteractive", "-Command", &ps_script])
                        .spawn();
                }
            }
        }
    }

    fn launch_game(&mut self, target_dir: &PathBuf) {
        if let Some(exe_path) = Self::find_executable(target_dir) {
            match std::process::Command::new(&exe_path)
                .current_dir(exe_path.parent().unwrap()) 
                .spawn() 
            {
                Ok(_) => {
                    self.download_status = "".to_string();
                }
                Err(e) => {
                    self.download_status = format!("Error launching: {}", e);
                }
            }
        } else {
            self.download_status = "No exe found! (Is it gone?)".to_string();
        }
    }

    fn open_game_folder(target_dir: &PathBuf) {
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("explorer").arg(target_dir).spawn();
        }
    }

    fn download_and_install(&mut self, asset: GithubAsset, game_name: String) {
        self.is_downloading = true;
        self.download_progress = 0.0;
        
        let tx = self.tx.clone();
        
        let safe_game_name = game_name.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != ' ', "_");
        self.download_status = format!("");
        
        let target_dir = self.games_dir.join(&safe_game_name);
        let temp_dir = self.games_dir.join(format!("{}_temp", safe_game_name));
        let archive_path = self.games_dir.join(format!("{}.7z", safe_game_name));
        
        thread::spawn(move || {
            let _ = fs::remove_dir_all(&temp_dir);
            let _ = fs::remove_dir_all(&target_dir);
            let _ = fs::remove_file(&archive_path);

            if let Err(e) = fs::create_dir_all(&temp_dir) {
                let _ = tx.send(AppMsg::DownloadError(format!("Can't create temporary folder: {}", e)));
                return;
            }

            let _ = tx.send(AppMsg::DownloadProgress(0.01, "Connecting".into()));
            let client = reqwest::blocking::Client::builder().user_agent("Vault64-Launcher").build().unwrap();
            
            let mut response = match client.get(&asset.browser_download_url).send() {
                Ok(res) => res,
                Err(e) => {
                    let _ = tx.send(AppMsg::DownloadError(format!("Connection Error: {}", e)));
                    return;
                }
            };

            if !response.status().is_success() {
                let _ = tx.send(AppMsg::DownloadError(format!("HTTP Error: {}", response.status())));
                return;
            }

            let mut file = match File::create(&archive_path) {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(AppMsg::DownloadError(format!("Error saving the file to hard drive! {}", e)));
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
                            let _ = tx.send(AppMsg::DownloadError(format!("Error saving the file to hard drive: {}", e)));
                            return;
                        }
                        downloaded += n as f32;
                        let progress = (downloaded / total_size).clamp(0.0, 1.0) * 0.6;
                        let _ = tx.send(AppMsg::DownloadProgress(
                            progress, 
                            format!("Downloading: {:.2} MB / {:.2} MB", downloaded / 1_048_576.0, total_size / 1_048_576.0)
                        ));
                    },
                    Err(e) => {
                        let _ = tx.send(AppMsg::DownloadError(format!("Connection interrupted {}", e)));
                        return;
                    }
                }
            }

            if (downloaded as u64) < asset.size {
                let _ = tx.send(AppMsg::DownloadError("Downloading got interrupted! (the file is probably corrupted :( ) ).".to_string()));
                return;
            }

            drop(file);

            let _ = tx.send(AppMsg::DownloadProgress(0.70, "Unpacking files (Might take a while...so sit and relax :P)".into()));
            
            if let Err(e) = sevenz_rust::decompress_file(&archive_path, &temp_dir) {
                let _ = tx.send(AppMsg::DownloadError(format!("Critical Error decompressing .7z file. (Is it corrupted?): {}", e)));
                return;
            }

            let _ = tx.send(AppMsg::DownloadProgress(0.99, "Cleaning up...".into()));
            
            if let Err(e) = fs::rename(&temp_dir, &target_dir) {
                let _ = tx.send(AppMsg::DownloadError(format!("Error finalizing installation! {}", e)));
                return;
            }

            if let Err(e) = fs::remove_file(&archive_path) {
                println!("Warning: failed to remove 7z file: {}", e);
            }

            let _ = tx.send(AppMsg::DownloadComplete(safe_game_name));
        });
    }

    fn ui_login(&mut self, ctx: &egui::Context) {
        let alpha = ctx.animate_value_with_time(egui::Id::new("login_fade"), 1.0, 0.4);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() / 4.0);
                
                let tint_color = egui::Color32::from_rgb(28, 30, 38).linear_multiply(alpha);
                
                egui::Frame::none()
                    .fill(tint_color)
                    .rounding(16.0)
                    .inner_margin(35.0)
                    .shadow(egui::epaint::Shadow {
                        offset: egui::vec2(0.0, 8.0),
                        blur: 25.0,
                        spread: 0.0,
                        color: egui::Color32::from_black_alpha(120),
                    })
                    .show(ui, |ui| {
                        ui.heading(egui::RichText::new("Vault64").size(50.0).strong().color(egui::Color32::WHITE));
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new("Enter login & password!").color(egui::Color32::GRAY));
                        ui.add_space(25.0);

                        ui.set_max_width(240.0);

                        let username_edit = egui::TextEdit::singleline(&mut self.login_username)
                            .hint_text("Username (guest)")
                            .margin(egui::vec2(12.0, 12.0));
                        ui.add(username_edit);
                        ui.add_space(12.0);

                        let pass_edit = egui::TextEdit::singleline(&mut self.login_password)
                            .password(true)
                            .hint_text("Password (1234)")
                            .margin(egui::vec2(12.0, 12.0));
                        ui.add(pass_edit);
                        
                        ui.add_space(15.0);
                        ui.checkbox(&mut self.remember_me, "Remember me");
                        ui.add_space(20.0);

                        if let Some(err) = &self.login_error {
                            ui.colored_label(egui::Color32::from_rgb(255, 90, 90), err);
                            ui.add_space(10.0);
                        }

                        if ui.add_sized([ui.available_width(), 44.0], egui::Button::new(egui::RichText::new("L O G I N").strong().size(16.0))).clicked() {
                            if UserDatabase::verify(&self.login_username, &self.login_password) {
                                self.login_error = None;
                                
                                if self.remember_me {
                                    self.config.remembered_username = Some(self.login_username.clone());
                                } else {
                                    self.config.remembered_username = None;
                                }
                                self.save_config();
                                
                                self.state = AppState::Launcher;
                                self.fetch_releases();
                            } else {
                                self.login_error = Some("Invalid username or password".to_string());
                            }
                        }
                    });
            });
        });
    }

    fn ui_launcher(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                AppMsg::ReleasesFetched(Ok(data)) => self.releases = Some(data),
                AppMsg::ReleasesFetched(Err(e)) => self.fetch_error = Some(e),
                AppMsg::DownloadProgress(prog, status) => {
                    self.download_progress = prog;
                    self.download_status = status;
                }
                AppMsg::DownloadComplete(safe_game_name) => {
                    self.is_downloading = false;
                    self.download_status = format!("");
                    
                    // Automatyczne tworzenie skrótu na pulpicie
                    let target_dir = self.games_dir.join(&safe_game_name);
                    if let Some(exe_path) = Self::find_executable(&target_dir) {
                        Self::create_desktop_shortcut(&safe_game_name, &exe_path);
                    }
                }
                AppMsg::DownloadError(err) => {
                    self.is_downloading = false;
                    self.download_status = format!("Error: {}", err);
                }
            }
        }

        egui::TopBottomPanel::top("top_panel")
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(15, 16, 20)).inner_margin(15.0))
            .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Vault64").strong().size(26.0).color(egui::Color32::WHITE));
                ui.add_space(20.0);

                if self.tab == LauncherTab::GameDetails {
                    if ui.add(egui::Button::new(egui::RichText::new("⬅ Back").strong())).clicked() {
                        self.tab = self.previous_tab;
                    }
                } else {
                    ui.selectable_value(&mut self.tab, LauncherTab::Store, egui::RichText::new("🏪 Store").size(16.0));
                    ui.add_space(10.0);
                    ui.selectable_value(&mut self.tab, LauncherTab::Library, egui::RichText::new("📚 Library").size(16.0));
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_sized([40.0, 30.0], egui::Button::new("🚪 Logout")).clicked() {
                        self.config.remembered_username = None;
                        self.save_config();
                        self.state = AppState::Login;
                        self.tab = LauncherTab::Library;
                        self.login_password.clear();
                    }
                    ui.label(egui::RichText::new(format!("👤 {}", self.login_username)).color(egui::Color32::LIGHT_GRAY));
                });
            });
        });

        egui::TopBottomPanel::bottom("bottom_panel")
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(14, 15, 18)).inner_margin(12.0))
            .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.is_downloading {
                    let progress_bar = egui::ProgressBar::new(self.download_progress)
                        .show_percentage()
                        .animate(true);
                    ui.add_sized([320.0, 20.0], progress_bar);
                    ui.add_space(12.0);
                    ui.label(&self.download_status);
                } else if !self.download_status.is_empty() {
                    let color = if self.download_status.starts_with("Error") || self.download_status.starts_with("Not found") {
                        egui::Color32::from_rgb(255, 90, 90) 
                    } else {
                        egui::Color32::from_rgb(100, 255, 120) 
                    };
                    ui.label(egui::RichText::new(&self.download_status).color(color));
                } else {
                    ui.label(egui::RichText::new("").color(egui::Color32::DARK_GRAY));
                }
            });
        });

        // Fixed match statement here - dispatches correctly to Store, Library, and Game Details.
        match self.tab {
            LauncherTab::Store => self.ui_store_view(ctx),
            LauncherTab::Library => self.ui_library_view(ctx),
            LauncherTab::GameDetails => self.ui_game_details_view(ctx),
        }
    }

    fn ui_store_view(&mut self, ctx: &egui::Context) {
        let mut switch_to_details = None;
        let mut pending_add_to_library = None;

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                ui.add_space(20.0);
                
                if let Some(err) = &self.fetch_error {
                    ui.colored_label(egui::Color32::RED, format!("Failed to fetch games: {}", err));
                    if ui.button("Retry Connection").clicked() {
                        self.fetch_error = None;
                        self.fetch_releases();
                    }
                    return;
                }

                if let Some(releases) = &self.releases {
                    if releases.is_empty() {
                        ui.label(egui::RichText::new("No games found!").size(18.0));
                    }
                    
                    for (index, release) in releases.iter().enumerate() {
                        let game_name = release.name.clone().unwrap_or(release.tag_name.clone());
                        let safe_game_name = game_name.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != ' ', "_");
                        let in_library = self.config.library.contains(&safe_game_name);
                        
                        let card_id = ui.id().with("store_card").with(index);
                        let alpha = ctx.animate_value_with_time(card_id, 1.0, 0.35);

                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(28, 30, 38).linear_multiply(alpha))
                            .rounding(14.0)
                            .inner_margin(egui::Margin::symmetric(22.0, 16.0))
                            .shadow(egui::epaint::Shadow {
                                offset: egui::vec2(0.0, 5.0),
                                blur: 12.0,
                                spread: 0.0,
                                color: egui::Color32::from_black_alpha(70),
                            })
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        let name = release.name.as_deref().unwrap_or(&release.tag_name);
                                        
                                        if ui.add(egui::Link::new(egui::RichText::new(name).size(22.0).strong().color(egui::Color32::WHITE)))
                                            .on_hover_text("View game details & Release Notes")
                                            .clicked() 
                                        {
                                            switch_to_details = Some(index);
                                        }

                                        if let Some(body) = &release.body {
                                            if !body.is_empty() {
                                                let snippet = body.lines().next().unwrap_or("").chars().take(80).collect::<String>();
                                                ui.add_space(4.0);
                                                ui.label(egui::RichText::new(format!("{}...", snippet)).color(egui::Color32::GRAY));
                                            }
                                        }
                                    });
                                    
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if in_library {
                                            ui.label(egui::RichText::new("").strong().color(egui::Color32::GREEN));
                                        } else {
                                            if ui.add_sized([130.0, 38.0], egui::Button::new(egui::RichText::new("➕ Add to Library").size(14.0))).clicked() {
                                                pending_add_to_library = Some(safe_game_name.clone());
                                            }
                                        }
                                        
                                        ui.add_space(10.0);
                                        if ui.add_sized([80.0, 38.0], egui::Button::new("Details")).clicked() {
                                            switch_to_details = Some(index);
                                        }
                                    });
                                });
                            });
                        ui.add_space(14.0);
                    }
                } else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() / 3.0);
                        ui.spinner();
                        ui.add_space(15.0);
                        ui.label(egui::RichText::new("Loading Store...").color(egui::Color32::GRAY));
                    });
                }
            });
        });

        // Add the pending game to the library after the UI rendering
        if let Some(game) = pending_add_to_library {
            self.config.library.push(game);
            self.save_config();
        }

        if let Some(idx) = switch_to_details {
            self.selected_release_idx = Some(idx);
            self.previous_tab = LauncherTab::Store;
            self.tab = LauncherTab::GameDetails;
        }
    }

    fn ui_library_view(&mut self, ctx: &egui::Context) {
        let mut pending_download = None;
        let mut pending_launch = None; 
        let mut switch_to_details = None;

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                ui.add_space(20.0);
                
                if let Some(err) = &self.fetch_error {
                    ui.colored_label(egui::Color32::RED, format!("Failed to fetch games: {}", err));
                    if ui.button("Retry Connection").clicked() {
                        self.fetch_error = None;
                        self.fetch_releases();
                    }
                    return;
                }

                if let Some(releases) = &self.releases {
                    let mut has_games = false;
                    for (index, release) in releases.iter().enumerate() {
                        let game_name = release.name.clone().unwrap_or(release.tag_name.clone());
                        let safe_game_name = game_name.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != ' ', "_");
                        
                        // SPRAWDZANIE CZY GRA JEST W BIBLIOTECE
                        if !self.config.library.contains(&safe_game_name) {
                            continue;
                        }
                        
                        has_games = true;
                        let target_dir = self.games_dir.join(&safe_game_name);
                        let is_installed = target_dir.exists(); 
                        
                        let card_id = ui.id().with("lib_card").with(index);
                        let alpha = ctx.animate_value_with_time(card_id, 1.0, 0.35);

                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(28, 30, 38).linear_multiply(alpha))
                            .rounding(14.0)
                            .inner_margin(egui::Margin::symmetric(22.0, 16.0))
                            .shadow(egui::epaint::Shadow {
                                offset: egui::vec2(0.0, 5.0),
                                blur: 12.0,
                                spread: 0.0,
                                color: egui::Color32::from_black_alpha(70),
                            })
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        let name = release.name.as_deref().unwrap_or(&release.tag_name);
                                        
                                        if ui.add(egui::Link::new(egui::RichText::new(name).size(22.0).strong().color(egui::Color32::WHITE)))
                                            .on_hover_text("View game details")
                                            .clicked() 
                                        {
                                            switch_to_details = Some(index);
                                        }

                                        if is_installed {
                                            ui.add_space(4.0);
                                            ui.label(egui::RichText::new("").color(egui::Color32::GREEN));
                                        } else {
                                            ui.add_space(4.0);
                                            ui.label(egui::RichText::new("").color(egui::Color32::GRAY));
                                        }
                                    });
                                    
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if is_installed {
                                            if ui.add_sized([110.0, 38.0], egui::Button::new(egui::RichText::new("▶ Play").size(17.0).color(egui::Color32::from_rgb(120, 255, 120)))).clicked() {
                                                pending_launch = Some(target_dir.clone());
                                            }
                                            
                                            if ui.add_sized([38.0, 38.0], egui::Button::new("📁").fill(egui::Color32::from_rgb(42, 44, 54))).on_hover_text("Open game's folder").clicked() {
                                                Self::open_game_folder(&target_dir);
                                            }
                                        } else {
                                            if let Some(asset) = release.assets.iter().find(|a| a.name.ends_with(".7z")) {
                                                ui.add_enabled_ui(!self.is_downloading, |ui| {
                                                    if ui.add_sized([120.0, 38.0], egui::Button::new(egui::RichText::new("⬇ Download").size(15.0))).clicked() {
                                                        pending_download = Some((asset.clone(), game_name.clone()));
                                                    }
                                                });
                                                ui.label(egui::RichText::new(format!("{:.1} MB", asset.size as f64 / 1_048_576.0)).color(egui::Color32::DARK_GRAY));
                                                ui.add_space(8.0);
                                            } else {
                                                ui.label(egui::RichText::new("Missing package").color(egui::Color32::from_rgb(255, 100, 100)));
                                            }
                                        }
                                    });
                                });
                            });
                        ui.add_space(14.0);
                    }
                    
                    if !has_games {
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui.available_height() / 3.0);
                            ui.label(egui::RichText::new("Oh noes! It looks like your library is empty!)").size(24.0).strong().color(egui::Color32::WHITE));
                            ui.add_space(10.0);
                            ui.label(egui::RichText::new("Try going to the store tab and add games you want :D").size(16.0).color(egui::Color32::GRAY));
                        });
                    }
                } else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() / 3.0);
                        ui.spinner();
                        ui.add_space(15.0);
                        ui.label(egui::RichText::new("Loading Library...").color(egui::Color32::GRAY));
                    });
                }
            });
        });

        if let Some(idx) = switch_to_details {
            self.selected_release_idx = Some(idx);
            self.previous_tab = LauncherTab::Library;
            self.tab = LauncherTab::GameDetails;
        }

        if let Some((asset, game_name)) = pending_download {
            self.download_and_install(asset, game_name);
        }

        if let Some(target_dir) = pending_launch {
            self.launch_game(&target_dir);
        }
    }

    fn ui_game_details_view(&mut self, ctx: &egui::Context) {
        let mut pending_download = None;
        let mut pending_launch = None;

        let release_opt = self.selected_release_idx.and_then(|idx| {
            self.releases.as_ref().and_then(|r| r.get(idx)).cloned()
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(release) = release_opt {
                let game_name = release.name.clone().unwrap_or(release.tag_name.clone());
                let safe_game_name = game_name.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != ' ', "_");
                let target_dir = self.games_dir.join(&safe_game_name);
                let is_installed = target_dir.exists();
                let in_library = self.config.library.contains(&safe_game_name);

                egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                    ui.add_space(15.0);

                    // Nagłówek gry
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(26, 28, 36))
                        .rounding(14.0)
                        .inner_margin(25.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.heading(egui::RichText::new(&game_name).size(32.0).strong().color(egui::Color32::WHITE));
                                    ui.add_space(6.0);
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(format!("")).color(egui::Color32::from_rgb(0, 160, 255)));
                                        ui.add_space(15.0);
                                        if is_installed {
                                            ui.label(egui::RichText::new("").color(egui::Color32::GREEN));
                                        } else if in_library {
                                            ui.label(egui::RichText::new("").color(egui::Color32::from_rgb(200, 200, 200)));
                                        } else {
                                            ui.label(egui::RichText::new("").color(egui::Color32::GRAY));
                                        }
                                    });
                                });

                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if in_library {
                                        if is_installed {
                                            if ui.add_sized([130.0, 42.0], egui::Button::new(egui::RichText::new("▶ PLAY").size(18.0).strong().color(egui::Color32::from_rgb(120, 255, 120)))).clicked() {
                                                pending_launch = Some(target_dir.clone());
                                            }
                                            if ui.add_sized([42.0, 42.0], egui::Button::new("📁")).on_hover_text("Open folder").clicked() {
                                                Self::open_game_folder(&target_dir);
                                            }
                                        } else if let Some(asset) = release.assets.iter().find(|a| a.name.ends_with(".7z")) {
                                            ui.add_enabled_ui(!self.is_downloading, |ui| {
                                                if ui.add_sized([140.0, 42.0], egui::Button::new(egui::RichText::new("⬇ Download").size(16.0).strong())).clicked() {
                                                    pending_download = Some((asset.clone(), game_name.clone()));
                                                }
                                            });
                                        }
                                    } else {
                                        if ui.add_sized([160.0, 42.0], egui::Button::new(egui::RichText::new("➕ Add to Library").size(16.0).strong())).clicked() {
                                            self.config.library.push(safe_game_name.clone());
                                            self.save_config();
                                        }
                                    }
                                });
                            });
                        });

                    ui.add_space(20.0);
                    ui.label(egui::RichText::new("Description").size(20.0).strong().color(egui::Color32::WHITE));
                    ui.add_space(8.0);

                    // Zawartość opisu Release Notes z GitHub
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(22, 23, 29))
                        .rounding(12.0)
                        .inner_margin(20.0)
                        .show(ui, |ui| {
                            if let Some(body) = &release.body {
                                if !body.trim().is_empty() {
                                    ui.label(egui::RichText::new(body).size(15.0).color(egui::Color32::LIGHT_GRAY));
                                } else {
                                    ui.label(egui::RichText::new("No description provided :/").italics().color(egui::Color32::GRAY));
                                }
                            } else {
                                ui.label(egui::RichText::new("No description provided :/").italics().color(egui::Color32::GRAY));
                            }
                        });
                });

                if let Some((asset, game_name)) = pending_download {
                    self.download_and_install(asset, game_name);
                }

                if let Some(target_dir) = pending_launch {
                    self.launch_game(&target_dir);
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Game not found!");
                });
            }
        });
    }
}

impl eframe::App for Vault64App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        match self.state {
            AppState::Login => self.ui_login(ctx),
            AppState::Launcher => self.ui_launcher(ctx),
        }
        
        // Force repaint
        ctx.request_repaint();
    }
}

fn load_icon(path: &str) -> Option<egui::IconData> {
    image::open(path).ok().map(|img| {
        let img = img.into_rgba8();
        let (width, height) = img.dimensions();
        let rgba = img.into_raw();
        egui::IconData { rgba, width, height }
    })
}

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([900.0, 700.0])
        .with_title("Vault64");
    
    // Pobieranie ikony programu z /icons/icon.ico
    if let Some(icon_data) = load_icon("icons/icon.ico") {
        viewport = viewport.with_icon(Arc::new(icon_data));
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Vault64",
        options,
        Box::new(|cc| Box::new(Vault64App::new(cc))),
    )
}

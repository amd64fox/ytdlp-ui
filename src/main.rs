#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod updater;

use eframe::egui;
use std::env;
use std::fs::{self};
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use arboard::Clipboard;
use serde::{Deserialize, Serialize};

// --- КОНФИГУРАЦИЯ ---
const CONFIG_FILE: &str = "config.toml";

#[derive(Serialize, Deserialize, Clone, Debug)]
struct AppConfig {
    output_path: String,
    yt_dlp_args: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        let mut video_path = env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        video_path.pop();
        video_path.push("Video");
        let _ = fs::create_dir_all(&video_path);

        Self {
            output_path: video_path.to_string_lossy().to_string(),
            yt_dlp_args: vec![
                "--sponsorblock-remove sponsor,selfpromo".to_string(),
                "--format bestvideo[height<=1080]+bestaudio/best[height<=1080]/best".to_string(),
                "-S vcodec:h264,acodec:mp4a,fps:30".to_string(),
                "--merge-output-format mp4".to_string(),
            ],
        }
    }
}

impl AppConfig {
    fn load() -> Self {
        if let Ok(content) = fs::read_to_string(CONFIG_FILE) {
            if let Ok(cfg) = toml::from_str::<AppConfig>(&content) {
                let _ = fs::create_dir_all(&cfg.output_path);
                return cfg;
            }
        }
        let cfg = Self::default();
        cfg.save();
        cfg
    }

    fn save(&self) {
        if let Ok(content) = toml::to_string_pretty(self) {
            let _ = fs::write(CONFIG_FILE, content);
        }
    }
}

enum AppMessage {
    Log(String),
    UpdateSnapshot(Vec<updater::ComponentInfo>),
    UpdatingComponent(Option<String>),
    AllFinished,
}

// --- UI Theme (СТИЛЬ LOADERSPOT) ---
pub struct UiTheme;
impl UiTheme {
    // Очень темный фон (почти черный)
    pub const BG: egui::Color32 = egui::Color32::from_rgb(18, 18, 18);

    // Группы: Прозрачные или чуть светлее фона, НО ГЛАВНОЕ - РАМКА
    pub const GROUP_BG: egui::Color32 = egui::Color32::from_rgb(24, 24, 24);

    // Поля ввода: Темнее группы ("вдавленные")
    pub const INPUT_BG: egui::Color32 = egui::Color32::from_rgb(10, 10, 10);

    // Обводка: Заметный серый контур (суть стиля Wireframe)
    pub const STROKE: egui::Color32 = egui::Color32::from_rgb(65, 65, 65);

    // Кнопки
    pub const BUTTON_BG: egui::Color32 = egui::Color32::from_rgb(45, 45, 45);
    pub const BUTTON_HOVER: egui::Color32 = egui::Color32::from_rgb(70, 70, 70);
}

// Настройка глобального стиля виджетов
fn configure_global_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    // Скругления как на скриншоте (небольшие)
    let rounding = egui::Rounding::same(4.0);
    style.visuals.window_rounding = rounding;
    style.visuals.widgets.noninteractive.rounding = rounding;
    style.visuals.widgets.inactive.rounding = rounding;
    style.visuals.widgets.hovered.rounding = rounding;
    style.visuals.widgets.active.rounding = rounding;

    // СТИЛЬ КНОПОК
    style.visuals.widgets.inactive.bg_fill = UiTheme::BUTTON_BG;
    style.visuals.widgets.inactive.weak_bg_fill = UiTheme::BUTTON_BG;
    // Тонкая рамка вокруг кнопок
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, UiTheme::STROKE);
    style.visuals.widgets.inactive.fg_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_gray(220));

    style.visuals.widgets.hovered.bg_fill = UiTheme::BUTTON_HOVER;
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);

    style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(90, 90, 90);

    // Цвета окна
    style.visuals.panel_fill = UiTheme::BG;
    style.visuals.window_fill = UiTheme::BG;
    style.visuals.window_stroke = egui::Stroke::new(1.0, UiTheme::STROKE);

    ctx.set_style(style);
}

struct YtDlpApp {
    urls: Vec<String>,
    config: AppConfig,
    logs: String,

    is_working: bool,
    show_url_window: bool,
    show_update_confirm: bool,
    center_url_window_on_open: bool,
    center_confirm_window_on_open: bool,

    receiver: Receiver<AppMessage>,
    sender: Sender<AppMessage>,
    component_states: Vec<updater::ComponentInfo>,
    updating_component: Option<String>,
    app_dir: PathBuf,
}

impl YtDlpApp {
    fn url_manager_viewport_id() -> egui::ViewportId {
        egui::ViewportId::from_hash_of("url_manager_viewport")
    }

    fn update_confirm_viewport_id() -> egui::ViewportId {
        egui::ViewportId::from_hash_of("update_confirm_viewport")
    }

    fn spawn_update_check(sender: Sender<AppMessage>, ctx: egui::Context, app_dir: PathBuf) {
        thread::spawn(move || {
            match updater::check_for_updates(&app_dir, env!("CARGO_PKG_VERSION")) {
                Ok(states) => {
                    let _ = sender.send(AppMessage::UpdateSnapshot(states));
                }
                Err(err) => {
                    let _ = sender.send(AppMessage::Log(format!("Err update check: {}", err)));
                }
            }
            ctx.request_repaint();
        });
    }

    fn new(cc: &eframe::CreationContext) -> Self {
        let (sender, receiver) = channel();
        let config = AppConfig::load();
        let ctx = cc.egui_ctx.clone();

        // ПРИМЕНЯЕМ СТИЛЬ ЗДЕСЬ
        configure_global_style(&ctx);

        let app_dir = env::current_exe()
            .ok()
            .and_then(|mut p| {
                p.pop();
                Some(p)
            })
            .unwrap_or_else(|| PathBuf::from("."));

        Self::spawn_update_check(sender.clone(), ctx, app_dir.clone());

        Self {
            urls: vec![String::new()],
            config,
            logs: String::new(),
            is_working: false,
            show_url_window: false,
            show_update_confirm: false,
            center_url_window_on_open: false,
            center_confirm_window_on_open: false,
            receiver,
            sender,
            component_states: Vec::new(),
            updating_component: None,
            app_dir,
        }
    }

    // ... (методы collect_update_targets, start_download, start_update без изменений логики) ...
    fn collect_update_targets(&self) -> Vec<updater::ComponentInfo> {
        let mut result: Vec<updater::ComponentInfo> = self
            .component_states
            .iter()
            .filter(|comp| {
                comp.kind == updater::ComponentKind::YtDlp
                    && matches!(
                        comp.status,
                        updater::ComponentStatus::Missing
                            | updater::ComponentStatus::UpdateAvailable
                    )
            })
            .cloned()
            .collect();

        let ff_bundle_needed = self.component_states.iter().any(|comp| {
            matches!(
                comp.kind,
                updater::ComponentKind::Ffmpeg | updater::ComponentKind::Ffprobe
            ) && matches!(
                comp.status,
                updater::ComponentStatus::Missing | updater::ComponentStatus::UpdateAvailable
            )
        });

        if ff_bundle_needed {
            if let Some(template) = self.component_states.iter().find(|comp| {
                matches!(
                    comp.kind,
                    updater::ComponentKind::Ffmpeg | updater::ComponentKind::Ffprobe
                )
            }) {
                let mut bundled = template.clone();
                bundled.kind = updater::ComponentKind::Ffmpeg;
                bundled.title = "ffmpeg/ffprobe".to_string();
                result.push(bundled);
            }
        }
        result
    }

    fn start_download(&mut self, ctx: &egui::Context) {
        let _ = fs::create_dir_all(&self.config.output_path);
        self.config.save();
        let valid_urls: Vec<String> = self
            .urls
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if valid_urls.is_empty() {
            self.logs.push_str(">>> Список ссылок пуст!\n");
            return;
        }
        self.is_working = true;
        self.logs.clear();
        let total = valid_urls.len();
        self.logs
            .push_str(&format!(">>> Старт: {} файл(ов)\n", total));
        let path = self.config.output_path.clone();
        let config_args = self.config.yt_dlp_args.clone();
        let sender = self.sender.clone();
        let thread_ctx = ctx.clone();
        thread::spawn(move || {
            let clean_path = path.trim_end_matches('\\');
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            for (i, url) in valid_urls.iter().enumerate() {
                let _ = sender.send(AppMessage::Log(format!(
                    ">>> [{}/{}] {}",
                    i + 1,
                    total,
                    url
                )));
                thread_ctx.request_repaint();
                let output_template = format!(r"{}/%(title)s.%(ext)s", clean_path);
                let mut args = vec!["--newline".to_string()];
                for arg_line in config_args.iter() {
                    for part in arg_line.split_whitespace() {
                        args.push(part.to_string());
                    }
                }
                args.push("-o".to_string());
                args.push(output_template);
                args.push(url.to_string());
                let child = Command::new("yt-dlp")
                    .args(&args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn();
                match child {
                    Ok(mut child_process) => {
                        if let Some(stdout) = child_process.stdout.take() {
                            let reader = BufReader::new(stdout);
                            for line in reader.lines() {
                                if let Ok(l) = line {
                                    let _ = sender.send(AppMessage::Log(l));
                                    thread_ctx.request_repaint();
                                }
                            }
                        }
                        let _ = child_process.wait();
                    }
                    Err(e) => {
                        let _ = sender.send(AppMessage::Log(format!("❌ Ошибка: {}", e)));
                        thread_ctx.request_repaint();
                    }
                }
            }
            let _ = sender.send(AppMessage::AllFinished);
            thread_ctx.request_repaint();
        });
    }

    fn start_update(&mut self, ctx: &egui::Context) {
        let to_update = self.collect_update_targets();
        if to_update.is_empty() {
            self.logs.push_str(">>> Обновления не требуются.\n");
            return;
        }
        self.is_working = true;
        let sender = self.sender.clone();
        let thread_ctx = ctx.clone();
        let app_dir = self.app_dir.clone();
        thread::spawn(move || {
            for component in to_update.iter() {
                let _ = sender.send(AppMessage::UpdatingComponent(Some(component.title.clone())));
                match updater::install_component(&app_dir, component) {
                    Ok(updater::InstallResult::Installed(msg)) => {
                        let _ = sender.send(AppMessage::Log(format!("✅ {}", msg)));
                    }
                    Err(err) => {
                        let _ = sender
                            .send(AppMessage::Log(format!("❌ {}: {}", component.title, err)));
                    }
                }
                thread_ctx.request_repaint();
            }
            let _ = sender.send(AppMessage::UpdatingComponent(None));
            let _ = sender.send(AppMessage::Log("✅ Обновление завершено.".to_string()));
            if let Ok(states) = updater::check_for_updates(&app_dir, env!("CARGO_PKG_VERSION")) {
                let _ = sender.send(AppMessage::UpdateSnapshot(states));
            }
            let _ = sender.send(AppMessage::AllFinished);
            thread_ctx.request_repaint();
        });
    }

    fn component_badge(
        &self,
        ui: &egui::Ui,
        component: &updater::ComponentInfo,
    ) -> (egui::Color32, String) {
        let visuals = ui.visuals();
        if self.is_working && self.updating_component.as_deref() == Some(component.title.as_str()) {
            return (visuals.warn_fg_color, "обновляется".to_string());
        }
        match component.status {
            updater::ComponentStatus::Missing => {
                (visuals.error_fg_color, "не установлен".to_string())
            }
            updater::ComponentStatus::UpdateAvailable => {
                (visuals.warn_fg_color, "update available".to_string())
            }
            updater::ComponentStatus::UpToDate => (
                egui::Color32::from_rgb(100, 200, 100),
                "актуален".to_string(),
            ), // Менее яркий зеленый
            updater::ComponentStatus::Unknown => (visuals.weak_text_color(), "unknown".to_string()),
        }
    }

    fn draw_status_dot(ui: &mut egui::Ui, color: egui::Color32) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.0, color); // Чуть меньше точка
    }
}

impl eframe::App for YtDlpApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.receiver.try_recv() {
            match msg {
                AppMessage::Log(line) => {
                    self.logs.push_str(&line);
                    self.logs.push('\n');
                }
                AppMessage::UpdateSnapshot(states) => {
                    self.component_states = states;
                }
                AppMessage::UpdatingComponent(current) => {
                    self.updating_component = current;
                }
                AppMessage::AllFinished => {
                    self.is_working = false;
                    self.logs.push_str(">>> Готово.\n");
                }
            }
        }

        // --- ГЛОБАЛЬНАЯ ОБРАБОТКА CTRL+V ---
        if ctx.input_mut(|i| {
            i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::CTRL,
                egui::Key::V,
            ))
        }) {
            if let Ok(mut clipboard) = Clipboard::new() {
                if let Ok(text) = clipboard.get_text() {
                    ctx.output_mut(|o| o.copied_text = text);
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(5.0);

            ui.horizontal(|ui| {
                ui.heading("YouTube Downloader");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !self.is_working {
                        if ui.button("🔄 Обновить").clicked() {
                            if self.collect_update_targets().is_empty() {
                                self.logs.push_str(">>> Нет доступных обновлений.\n");
                            } else {
                                self.show_update_confirm = true;
                                self.center_confirm_window_on_open = true;
                            }
                        }
                    }
                });
            });

            ui.add_space(10.0);

            // --- БЛОК ССЫЛОК (В СТИЛЕ LOADERSPOT) ---
            egui::Frame::none()
                .fill(UiTheme::GROUP_BG)
                .stroke(egui::Stroke::new(1.0, UiTheme::STROKE)) // Рамка группы
                .inner_margin(8.0)
                .rounding(4.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new("Входящие ссылки")
                            .strong()
                            .color(egui::Color32::GRAY),
                    );
                    ui.add_space(4.0);

                    let btn_text = format!("Открыть список ({})", self.urls.len());
                    if ui
                        .add(
                            egui::Button::new(btn_text)
                                .min_size(egui::vec2(ui.available_width(), 24.0)),
                        )
                        .clicked()
                    {
                        self.show_url_window = true;
                        self.center_url_window_on_open = true;
                    }

                    ui.add_space(8.0);
                    ui.label("Вставить ссылку:");

                    // Input Field с рамкой
                    egui::Frame::none()
                        .fill(UiTheme::INPUT_BG)
                        .stroke(egui::Stroke::new(1.0, UiTheme::STROKE))
                        .rounding(4.0)
                        .inner_margin(4.0)
                        .show(ui, |ui| {
                            let url_edit = ui.add(
                                egui::TextEdit::singleline(&mut self.urls[0])
                                    .desired_width(f32::INFINITY)
                                    .frame(false)
                                    .hint_text("https://..."),
                            );
                            url_edit.context_menu(|ui| {
                                if ui.button("Вставить").clicked() {
                                    if let Ok(mut c) = Clipboard::new() {
                                        if let Ok(t) = c.get_text() {
                                            self.urls[0] = t;
                                        }
                                    }
                                    ui.close_menu();
                                }
                                if ui.button("Очистить").clicked() {
                                    self.urls[0].clear();
                                    ui.close_menu();
                                }
                            });
                        });
                });

            ui.add_space(10.0);

            // --- БЛОК КОНФИГА ---
            egui::Frame::none()
                .fill(UiTheme::GROUP_BG)
                .stroke(egui::Stroke::new(1.0, UiTheme::STROKE))
                .inner_margin(8.0)
                .rounding(4.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new("Путь сохранения")
                            .strong()
                            .color(egui::Color32::GRAY),
                    );
                    ui.add_space(4.0);
                    egui::Frame::none()
                        .fill(UiTheme::INPUT_BG)
                        .stroke(egui::Stroke::new(1.0, UiTheme::STROKE))
                        .rounding(4.0)
                        .inner_margin(4.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.config.output_path)
                                    .frame(false)
                                    .interactive(false)
                                    .desired_width(f32::INFINITY),
                            );
                        });
                });

            ui.add_space(10.0);

            // --- БЛОК КОМПОНЕНТОВ ---
            egui::Frame::none()
                .fill(UiTheme::GROUP_BG)
                .stroke(egui::Stroke::new(1.0, UiTheme::STROKE))
                .inner_margin(8.0)
                .rounding(4.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new("Состояние компонентов")
                            .strong()
                            .color(egui::Color32::GRAY),
                    );

                    egui::ScrollArea::vertical()
                        .id_source("components_scroll")
                        .max_height(80.0)
                        .min_scrolled_height(80.0)
                        .show(ui, |ui| {
                            if self.component_states.is_empty() {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("⌛ Проверка...").weak());
                                });
                            } else {
                                for component in &self.component_states {
                                    let (color, status_text) = self.component_badge(ui, component);
                                    ui.horizontal(|ui| {
                                        Self::draw_status_dot(ui, color);
                                        let title = egui::RichText::new(&component.title).strong();
                                        if component.status == updater::ComponentStatus::UpToDate {
                                            ui.label(title);
                                            ui.label(
                                                egui::RichText::new(
                                                    component
                                                        .local_version
                                                        .as_deref()
                                                        .unwrap_or("?"),
                                                )
                                                .weak(),
                                            );
                                        } else if component.status
                                            == updater::ComponentStatus::Missing
                                        {
                                            ui.label(title);
                                            ui.label(
                                                egui::RichText::new(status_text)
                                                    .color(ui.visuals().error_fg_color),
                                            );
                                        } else {
                                            ui.label(title);
                                            ui.label(format!(
                                                "{} -> {}",
                                                component.local_version.as_deref().unwrap_or("?"),
                                                component.latest_version.as_deref().unwrap_or("?")
                                            ));
                                        }
                                    });
                                }
                            }
                        });
                });

            ui.add_space(10.0);

            ui.horizontal(|ui| {
                if !self.is_working {
                    let has_missing = self
                        .component_states
                        .iter()
                        .any(|c| c.status == updater::ComponentStatus::Missing);
                    let is_checking = self.component_states.is_empty();

                    let label = if self.urls.len() > 1 && !self.urls[1].is_empty() {
                        "СКАЧАТЬ ВСЕ"
                    } else {
                        "СКАЧАТЬ"
                    };

                    let button_enabled = !has_missing && !is_checking;
                    let btn = egui::Button::new(label).min_size(egui::vec2(120.0, 36.0));

                    if ui.add_enabled(button_enabled, btn).clicked() {
                        self.start_download(ctx);
                    }
                } else {
                    ui.spinner();
                    ui.label("Работаю...");
                }
            });

            ui.add_space(10.0);

            // ЛОГ (Терминальный вид)
            egui::Frame::none()
                .fill(UiTheme::INPUT_BG) // Темный фон для лога
                .stroke(egui::Stroke::new(1.0, UiTheme::STROKE))
                .inner_margin(4.0)
                .rounding(4.0)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.add_sized(
                                [ui.available_width(), ui.available_height()],
                                egui::TextEdit::multiline(&mut self.logs)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(f32::INFINITY)
                                    .frame(false)
                                    .interactive(false),
                            );
                        });
                });
        });

        // --- ОКНО СПИСКА ССЫЛОК ---
        if self.show_url_window {
            let viewport_id = Self::url_manager_viewport_id();
            let mut close_clicked = false;
            let url_window_size = egui::vec2(720.0, 320.0);
            let mut viewport_builder = egui::ViewportBuilder::default()
                .with_title("Управление списком")
                .with_inner_size([url_window_size.x, url_window_size.y])
                .with_min_inner_size([url_window_size.x, url_window_size.y])
                .with_resizable(false)
                .with_maximize_button(false);

            if self.center_url_window_on_open {
                if let Some(ms) = ctx.input(|i| i.viewport().monitor_size) {
                    let pos = egui::pos2(
                        (ms.x - url_window_size.x) / 2.0,
                        (ms.y - url_window_size.y) / 2.0,
                    );
                    viewport_builder = viewport_builder.with_position(pos);
                }
                self.center_url_window_on_open = false;
            }

            ctx.show_viewport_immediate(viewport_id, viewport_builder, |ctx, _class| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::none().fill(UiTheme::BG).inner_margin(12.0)) // ВАЖНО: фон окна
                    .show(ctx, |ui| {
                        ui.label("Редактирование списка ссылок");
                        ui.add_space(8.0);
                        let bottom_h = 40.0;
                        let list_h = ui.available_height() - bottom_h - 10.0;

                        // Рамка вокруг списка
                        egui::Frame::none()
                            .fill(UiTheme::GROUP_BG)
                            .stroke(egui::Stroke::new(1.0, UiTheme::STROKE))
                            .inner_margin(8.0)
                            .rounding(4.0)
                            .show(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .id_source("url_list")
                                    .auto_shrink([false, true])
                                    .max_height(list_h)
                                    .show(ui, |ui| {
                                        let mut remove_idx = None;
                                        for (i, url) in self.urls.iter_mut().enumerate() {
                                            // Рамка вокруг каждой строки
                                            egui::Frame::none()
                                                .fill(UiTheme::INPUT_BG)
                                                .stroke(egui::Stroke::new(1.0, UiTheme::STROKE))
                                                .inner_margin(6.0)
                                                .rounding(4.0)
                                                .show(ui, |ui| {
                                                    ui.horizontal(|ui| {
                                                        ui.label(
                                                            egui::RichText::new(format!(
                                                                "{}.",
                                                                i + 1
                                                            ))
                                                            .color(egui::Color32::GRAY),
                                                        );
                                                        let w = (ui.available_width() - 30.0)
                                                            .max(100.0);
                                                        let te = ui.add(
                                                            egui::TextEdit::singleline(url)
                                                                .desired_width(w)
                                                                .frame(false)
                                                                .margin(egui::vec2(0.0, 0.0)),
                                                        );
                                                        te.context_menu(|ui| {
                                                            if ui.button("Paste").clicked() {
                                                                if let Ok(mut c) = Clipboard::new()
                                                                {
                                                                    if let Ok(t) = c.get_text() {
                                                                        *url = t;
                                                                    }
                                                                }
                                                                ui.close_menu();
                                                            }
                                                            if ui.button("Clear").clicked() {
                                                                url.clear();
                                                                ui.close_menu();
                                                            }
                                                        });
                                                        if ui
                                                            .add(
                                                                egui::Button::new("✖")
                                                                    .fill(
                                                                        egui::Color32::TRANSPARENT,
                                                                    )
                                                                    .frame(false),
                                                            )
                                                            .clicked()
                                                        {
                                                            remove_idx = Some(i);
                                                        }
                                                    });
                                                });
                                            ui.add_space(6.0);
                                        }
                                        if let Some(i) = remove_idx {
                                            if self.urls.len() > 1 {
                                                self.urls.remove(i);
                                            } else {
                                                self.urls[0].clear();
                                            }
                                        }
                                    });
                            });

                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if ui
                                .add(egui::Button::new("➕ Add").min_size(egui::vec2(100.0, 28.0)))
                                .clicked()
                            {
                                self.urls.push(String::new());
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new("Done")
                                                .min_size(egui::vec2(100.0, 28.0)),
                                        )
                                        .clicked()
                                    {
                                        close_clicked = true;
                                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                    }
                                },
                            );
                        });
                    });
                if ctx.input(|i| i.viewport().close_requested()) {
                    close_clicked = true;
                }
            });
            if close_clicked {
                self.show_url_window = false;
            }
        }

        // --- ОКНО ПОДТВЕРЖДЕНИЯ ---
        if self.show_update_confirm {
            let viewport_id = Self::update_confirm_viewport_id();
            let mut approve = false;
            let mut close_confirm = false;
            let targets = self.collect_update_targets();

            let confirm_width = 470.0;
            let confirm_height = 180.0; // Чуть выше
            let mut viewport_builder = egui::ViewportBuilder::default()
                .with_title("Update")
                .with_inner_size([confirm_width, confirm_height])
                .with_min_inner_size([confirm_width, confirm_height])
                .with_resizable(false)
                .with_maximize_button(false);

            if self.center_confirm_window_on_open {
                if let Some(ms) = ctx.input(|i| i.viewport().monitor_size) {
                    let pos =
                        egui::pos2((ms.x - confirm_width) / 2.0, (ms.y - confirm_height) / 2.0);
                    viewport_builder = viewport_builder.with_position(pos);
                }
                self.center_confirm_window_on_open = false;
            }

            ctx.show_viewport_immediate(viewport_id, viewport_builder, |ctx, _class| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::none().fill(UiTheme::BG).inner_margin(12.0))
                    .show(ctx, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label("Подтвердите обновление компонентов:");
                            ui.add_space(12.0);

                            // "Терминальный" список
                            egui::Frame::none()
                                .fill(UiTheme::INPUT_BG)
                                .stroke(egui::Stroke::new(1.0, UiTheme::STROKE))
                                .rounding(4.0)
                                .inner_margin(10.0)
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    egui::ScrollArea::vertical()
                                        .auto_shrink([false, true])
                                        .max_height(60.0)
                                        .show(ui, |ui| {
                                            for item in &targets {
                                                ui.horizontal(|ui| {
                                                    ui.painter().circle_filled(
                                                        ui.cursor().min + egui::vec2(4.0, 10.0),
                                                        3.0,
                                                        egui::Color32::from_rgb(255, 165, 0),
                                                    ); // Оранжевая точка
                                                    ui.add_space(10.0);
                                                    ui.label(
                                                        egui::RichText::new(&item.title)
                                                            .monospace(),
                                                    );
                                                });
                                            }
                                        });
                                });

                            ui.add_space(16.0);
                            ui.horizontal(|ui| {
                                let w = 110.0;
                                ui.add_space((ui.available_width() - (w * 2.0 + 10.0)) / 2.0);
                                if ui
                                    .add(egui::Button::new("Cancel").min_size(egui::vec2(w, 30.0)))
                                    .clicked()
                                {
                                    close_confirm = true;
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                                ui.add_space(10.0);
                                if ui
                                    .add(egui::Button::new("Update").min_size(egui::vec2(w, 30.0)))
                                    .clicked()
                                {
                                    approve = true;
                                    close_confirm = true;
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            });
                        });
                    });
                if ctx.input(|i| i.viewport().close_requested()) {
                    close_confirm = true;
                }
            });
            if close_confirm {
                self.show_update_confirm = false;
            }
            if approve {
                self.start_update(ctx);
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([500.0, 640.0])
            .with_min_inner_size([500.0, 640.0])
            .with_resizable(false)
            .with_maximize_button(false),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "YouTube Downloader",
        options,
        Box::new(|cc| Box::new(YtDlpApp::new(cc))),
    )
}

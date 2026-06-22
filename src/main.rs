#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod updater;

use eframe::egui;
use encoding_rs::WINDOWS_1251;
use std::env;
use std::fs::{self};
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use arboard::Clipboard;
use serde::{Deserialize, Serialize};

// --- КОНФИГУРАЦИЯ ---
const CONFIG_FILE: &str = "config.toml";
const APP_CONFIG_DIR: &str = "ytdlp-ui";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const WINDOWS_MONOSPACE_FONT_CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\consola.ttf",
    r"C:\Windows\Fonts\CascadiaMono.ttf",
    r"C:\Windows\Fonts\cour.ttf",
    r"C:\Windows\Fonts\lucon.ttf",
];

fn current_app_dir() -> PathBuf {
    env::current_exe().ok().map_or_else(
        || PathBuf::from("."),
        |mut path| {
            path.pop();
            path
        },
    )
}

fn resolve_config_path(app_dir: &Path) -> PathBuf {
    if env::var_os("YTDLP_UI_PORTABLE").is_some() {
        return app_dir.join(CONFIG_FILE);
    }

    if let Some(base_dir) = env::var_os("LOCALAPPDATA").or_else(|| env::var_os("APPDATA")) {
        return PathBuf::from(base_dir)
            .join(APP_CONFIG_DIR)
            .join(CONFIG_FILE);
    }

    app_dir.join(CONFIG_FILE)
}

fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    if let Some(font_bytes) = WINDOWS_MONOSPACE_FONT_CANDIDATES
        .iter()
        .find_map(|path| fs::read(path).ok())
    {
        let font_name = "windows-monospace".to_string();
        fonts.font_data.insert(
            font_name.clone(),
            egui::FontData::from_owned(font_bytes).into(),
        );

        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, font_name);
    }

    ctx.set_fonts(fonts);
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct AppConfig {
    output_path: String,
    yt_dlp_args: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            output_path: String::new(),
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
    fn default_for(app_dir: &Path) -> (Self, Vec<String>) {
        let video_path = app_dir.join("Video");
        let mut messages = Vec::new();

        if let Err(err) = fs::create_dir_all(&video_path) {
            messages.push(format!(
                ">>> Не удалось создать папку загрузок {}: {err}",
                video_path.display()
            ));
        }

        (
            Self {
                output_path: video_path.to_string_lossy().to_string(),
                ..Self::default()
            },
            messages,
        )
    }

    fn load(config_path: &Path, app_dir: &Path) -> (Self, Vec<String>) {
        let mut messages = Vec::new();

        match fs::read_to_string(config_path) {
            Ok(content) => match toml::from_str::<AppConfig>(&content) {
                Ok(cfg) => {
                    if let Err(err) = fs::create_dir_all(&cfg.output_path) {
                        messages.push(format!(
                            ">>> Не удалось подготовить папку загрузок {}: {err}",
                            cfg.output_path
                        ));
                    }
                    return (cfg, messages);
                }
                Err(err) => {
                    messages.push(format!(
                        ">>> Конфиг {} поврежден, будет создан новый: {err}",
                        config_path.display()
                    ));
                }
            },
            Err(err) if err.kind() != ErrorKind::NotFound => {
                messages.push(format!(
                    ">>> Не удалось прочитать конфиг {}: {err}",
                    config_path.display()
                ));
            }
            Err(_) => {}
        }

        let (cfg, default_messages) = Self::default_for(app_dir);
        messages.extend(default_messages);

        if let Err(err) = cfg.save(config_path) {
            messages.push(format!(
                ">>> Не удалось сохранить конфиг {}: {err}",
                config_path.display()
            ));
        }

        (cfg, messages)
    }

    fn save(&self, config_path: &Path) -> Result<(), String> {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }

        let content = toml::to_string_pretty(self).map_err(|err| err.to_string())?;
        fs::write(config_path, content).map_err(|err| err.to_string())
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
    show_url_editor: bool,
    show_update_confirm: bool,
    center_confirm_window_on_open: bool,

    receiver: Receiver<AppMessage>,
    sender: Sender<AppMessage>,
    component_states: Vec<updater::ComponentInfo>,
    updating_component: Option<String>,
    app_dir: PathBuf,
    config_path: PathBuf,
}

impl YtDlpApp {
    fn update_confirm_viewport_id() -> egui::ViewportId {
        egui::ViewportId::from_hash_of("update_confirm_viewport")
    }

    fn send_log(sender: &Sender<AppMessage>, ctx: &egui::Context, message: impl Into<String>) {
        let _ = sender.send(AppMessage::Log(message.into()));
        ctx.request_repaint();
    }

    fn decode_process_line(bytes: &[u8]) -> String {
        match std::str::from_utf8(bytes) {
            Ok(text) => text.to_owned(),
            Err(_) => {
                let (decoded, _, _) = WINDOWS_1251.decode(bytes);
                decoded.into_owned()
            }
        }
    }

    fn spawn_pipe_reader<R>(
        reader: R,
        sender: Sender<AppMessage>,
        ctx: egui::Context,
        prefix: &'static str,
    ) -> thread::JoinHandle<()>
    where
        R: Read + Send + 'static,
    {
        thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut buffer = Vec::new();

            loop {
                buffer.clear();

                match reader.read_until(b'\n', &mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {
                        while matches!(buffer.last(), Some(b'\n' | b'\r')) {
                            buffer.pop();
                        }

                        let line = Self::decode_process_line(&buffer);
                        let rendered = if prefix.is_empty() {
                            line
                        } else {
                            format!("{prefix}{line}")
                        };
                        Self::send_log(&sender, &ctx, rendered);
                    }
                    Err(err) => {
                        Self::send_log(
                            &sender,
                            &ctx,
                            format!("❌ Ошибка чтения вывода процесса: {err}"),
                        );
                        break;
                    }
                }
            }
        })
    }

    fn managed_yt_dlp_path(&self) -> PathBuf {
        self.app_dir.join("yt-dlp.exe")
    }

    fn spawn_update_check(sender: Sender<AppMessage>, ctx: egui::Context, app_dir: PathBuf) {
        thread::spawn(move || {
            let report = updater::check_for_updates(&app_dir);
            for warning in report.warnings {
                Self::send_log(&sender, &ctx, warning);
            }
            let _ = sender.send(AppMessage::UpdateSnapshot(report.components));
            ctx.request_repaint();
        });
    }

    fn new(cc: &eframe::CreationContext) -> Self {
        let (sender, receiver) = channel();
        let app_dir = current_app_dir();
        let config_path = resolve_config_path(&app_dir);
        let (config, config_messages) = AppConfig::load(&config_path, &app_dir);
        let ctx = cc.egui_ctx.clone();
        let mut logs = String::new();

        configure_fonts(&ctx);
        configure_global_style(&ctx);

        for message in config_messages {
            logs.push_str(&message);
            logs.push('\n');
        }

        Self::spawn_update_check(sender.clone(), ctx, app_dir.clone());

        Self {
            urls: vec![String::new()],
            config,
            logs,
            is_working: false,
            show_url_editor: false,
            show_update_confirm: false,
            center_confirm_window_on_open: false,
            receiver,
            sender,
            component_states: Vec::new(),
            updating_component: None,
            app_dir,
            config_path,
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
            let template = self
                .component_states
                .iter()
                .find(|comp| {
                    matches!(
                        comp.kind,
                        updater::ComponentKind::Ffmpeg | updater::ComponentKind::Ffprobe
                    ) && comp.download_url.is_some()
                })
                .or_else(|| {
                    self.component_states.iter().find(|comp| {
                        matches!(
                            comp.kind,
                            updater::ComponentKind::Ffmpeg | updater::ComponentKind::Ffprobe
                        )
                    })
                });

            if let Some(template) = template {
                let mut bundled = template.clone();
                bundled.kind = updater::ComponentKind::Ffmpeg;
                bundled.title = "ffmpeg/ffprobe".to_string();
                result.push(bundled);
            }
        }
        result
    }

    fn start_download(&mut self, ctx: &egui::Context) {
        if let Err(err) = fs::create_dir_all(&self.config.output_path) {
            self.logs.push_str(&format!(
                ">>> Не удалось создать папку загрузок {}: {err}\n",
                self.config.output_path
            ));
            return;
        }

        if let Err(err) = self.config.save(&self.config_path) {
            self.logs.push_str(&format!(
                ">>> Не удалось сохранить конфиг {}: {err}\n",
                self.config_path.display()
            ));
            return;
        }

        let yt_dlp_path = self.managed_yt_dlp_path();
        if !yt_dlp_path.is_file() {
            self.logs.push_str(
                ">>> yt-dlp.exe не найден. Сначала установите его через кнопку обновления.\n",
            );
            return;
        }

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
        let yt_dlp_path = yt_dlp_path.clone();
        let config_args = self.config.yt_dlp_args.clone();
        let sender = self.sender.clone();
        let thread_ctx = ctx.clone();
        thread::spawn(move || {
            let clean_path = path.trim_end_matches('\\');
            for (i, url) in valid_urls.iter().enumerate() {
                Self::send_log(
                    &sender,
                    &thread_ctx,
                    format!(">>> [{}/{}] {}", i + 1, total, url),
                );
                let output_template = format!(r"{clean_path}/%(title)s.%(ext)s");
                let mut args = vec!["--newline".to_string()];
                for arg_line in &config_args {
                    for part in arg_line.split_whitespace() {
                        args.push(part.to_string());
                    }
                }
                args.push("-o".to_string());
                args.push(output_template);
                args.push(url.clone());
                let child = Command::new(&yt_dlp_path)
                    .args(&args)
                    .env("PYTHONUTF8", "1")
                    .env("PYTHONIOENCODING", "utf-8")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn();
                match child {
                    Ok(mut child_process) => {
                        let stdout_handle = child_process.stdout.take().map(|stdout| {
                            Self::spawn_pipe_reader(stdout, sender.clone(), thread_ctx.clone(), "")
                        });
                        let stderr_handle = child_process.stderr.take().map(|stderr| {
                            Self::spawn_pipe_reader(
                                stderr,
                                sender.clone(),
                                thread_ctx.clone(),
                                "stderr | ",
                            )
                        });

                        match child_process.wait() {
                            Ok(status) => {
                                if let Some(handle) = stdout_handle {
                                    let _ = handle.join();
                                }
                                if let Some(handle) = stderr_handle {
                                    let _ = handle.join();
                                }

                                if !status.success() {
                                    let exit_code = status.code().map_or_else(
                                        || "без кода".to_string(),
                                        |code| code.to_string(),
                                    );
                                    Self::send_log(
                                        &sender,
                                        &thread_ctx,
                                        format!("❌ yt-dlp завершился с кодом {exit_code}"),
                                    );
                                }
                            }
                            Err(err) => {
                                Self::send_log(
                                    &sender,
                                    &thread_ctx,
                                    format!("❌ Не удалось дождаться завершения yt-dlp: {err}"),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        Self::send_log(
                            &sender,
                            &thread_ctx,
                            format!("❌ Ошибка запуска yt-dlp: {e}"),
                        );
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
            for component in &to_update {
                let _ = sender.send(AppMessage::UpdatingComponent(Some(component.title.clone())));
                match updater::install_component(&app_dir, component) {
                    Ok(updater::InstallResult::Installed(msg)) => {
                        let _ = sender.send(AppMessage::Log(format!("✅ {msg}")));
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
            let report = updater::check_for_updates(&app_dir);
            for warning in report.warnings {
                Self::send_log(&sender, &thread_ctx, warning);
            }
            let _ = sender.send(AppMessage::UpdateSnapshot(report.components));
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

    fn draw_button_with_icon(
        ui: &mut egui::Ui,
        text: &str,
        min_size: egui::Vec2,
        icon: impl FnOnce(&egui::Painter, egui::Rect, egui::Color32),
    ) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(min_size, egui::Sense::click());

        if ui.is_rect_visible(rect) {
            let visuals = ui.style().interact(&response);
            let painter = ui.painter();
            let rounding = egui::Rounding::same(4.0);

            painter.rect_filled(rect, rounding, visuals.bg_fill);
            painter.rect_stroke(rect, rounding, visuals.bg_stroke);

            let icon_rect = egui::Rect::from_center_size(
                egui::pos2(rect.left() + 16.0, rect.center().y),
                egui::vec2(14.0, 14.0),
            );
            icon(painter, icon_rect, visuals.fg_stroke.color);

            painter.text(
                egui::pos2(icon_rect.right() + 8.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                text,
                egui::TextStyle::Button.resolve(ui.style()),
                visuals.fg_stroke.color,
            );
        }

        response
    }

    fn draw_icon_only_button(
        ui: &mut egui::Ui,
        size: egui::Vec2,
        icon: impl FnOnce(&egui::Painter, egui::Rect, egui::Color32),
    ) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());

        if ui.is_rect_visible(rect) {
            let visuals = ui.style().interact(&response);
            let painter = ui.painter();
            let rounding = egui::Rounding::same(4.0);

            painter.rect_filled(rect, rounding, visuals.bg_fill);
            painter.rect_stroke(rect, rounding, visuals.bg_stroke);

            let icon_rect = rect.shrink2(egui::vec2(7.0, 7.0));
            icon(painter, icon_rect, visuals.fg_stroke.color);
        }

        response
    }

    fn paint_back_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(1.8, color);
        let center_y = rect.center().y;
        let left = rect.left();
        let right = rect.right();

        painter.line_segment(
            [egui::pos2(left, center_y), egui::pos2(right, center_y)],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(left, center_y),
                egui::pos2(left + 5.0, rect.top() + 2.0),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(left, center_y),
                egui::pos2(left + 5.0, rect.bottom() - 2.0),
            ],
            stroke,
        );
    }

    fn paint_plus_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(1.8, color);
        painter.line_segment(
            [
                egui::pos2(rect.left(), rect.center().y),
                egui::pos2(rect.right(), rect.center().y),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(rect.center().x, rect.top()),
                egui::pos2(rect.center().x, rect.bottom()),
            ],
            stroke,
        );
    }

    fn paint_trash_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(1.6, color);

        let body = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 2.5, rect.top() + 4.5),
            egui::pos2(rect.right() - 2.5, rect.bottom() - 1.0),
        );
        painter.rect_stroke(body, egui::Rounding::same(1.5), stroke);

        painter.line_segment(
            [
                egui::pos2(rect.left() + 1.5, rect.top() + 3.5),
                egui::pos2(rect.right() - 1.5, rect.top() + 3.5),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(rect.center().x - 3.0, rect.top() + 1.5),
                egui::pos2(rect.center().x + 3.0, rect.top() + 1.5),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(rect.center().x, rect.top() + 1.5),
                egui::pos2(rect.center().x, rect.top() + 3.0),
            ],
            stroke,
        );

        for x in [-3.0, 0.0, 3.0] {
            painter.line_segment(
                [
                    egui::pos2(rect.center().x + x, body.top() + 2.0),
                    egui::pos2(rect.center().x + x, body.bottom() - 1.5),
                ],
                stroke,
            );
        }
    }

    fn paint_close_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(1.8, color);
        painter.line_segment(
            [
                egui::pos2(rect.left(), rect.top()),
                egui::pos2(rect.right(), rect.bottom()),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(rect.left(), rect.bottom()),
                egui::pos2(rect.right(), rect.top()),
            ],
            stroke,
        );
    }

    fn draw_url_editor(&mut self, ui: &mut egui::Ui) {
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            if Self::draw_button_with_icon(
                ui,
                "Назад",
                egui::vec2(84.0, 28.0),
                Self::paint_back_icon,
            )
            .clicked()
            {
                self.show_url_editor = false;
            }
            ui.add_space(8.0);
            ui.heading("Редактор ссылок");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("{} ссылок", self.urls.len()))
                        .color(egui::Color32::GRAY),
                );
            });
        });

        ui.add_space(10.0);

        egui::Frame::none()
            .fill(UiTheme::GROUP_BG)
            .stroke(egui::Stroke::new(1.0, UiTheme::STROKE))
            .inner_margin(10.0)
            .rounding(4.0)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Добавьте одну или несколько ссылок")
                        .strong()
                        .color(egui::Color32::GRAY),
                );
                ui.add_space(8.0);

                egui::ScrollArea::vertical()
                    .id_salt("url_list_editor")
                    .auto_shrink([false, true])
                    .max_height(ui.available_height() - 70.0)
                    .show(ui, |ui| {
                        let mut remove_idx = None;

                        for (i, url) in self.urls.iter_mut().enumerate() {
                            egui::Frame::none()
                                .fill(UiTheme::INPUT_BG)
                                .stroke(egui::Stroke::new(1.0, UiTheme::STROKE))
                                .inner_margin(6.0)
                                .rounding(4.0)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!("{:02}.", i + 1))
                                                .color(egui::Color32::GRAY),
                                        );
                                        let width = (ui.available_width() - 34.0).max(100.0);
                                        let text_edit = ui.add(
                                            egui::TextEdit::singleline(url)
                                                .desired_width(width)
                                                .frame(false)
                                                .hint_text("https://...")
                                                .margin(egui::vec2(0.0, 0.0)),
                                        );
                                        text_edit.context_menu(|ui| {
                                            if ui.button("Вставить").clicked() {
                                                if let Ok(mut clipboard) = Clipboard::new() {
                                                    if let Ok(text) = clipboard.get_text() {
                                                        *url = text;
                                                    }
                                                }
                                                ui.close_menu();
                                            }
                                            if ui.button("Очистить").clicked() {
                                                url.clear();
                                                ui.close_menu();
                                            }
                                        });

                                        if Self::draw_icon_only_button(
                                            ui,
                                            egui::vec2(24.0, 24.0),
                                            Self::paint_close_icon,
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
            if Self::draw_button_with_icon(
                ui,
                "Добавить строку",
                egui::vec2(140.0, 30.0),
                Self::paint_plus_icon,
            )
            .clicked()
            {
                self.urls.push(String::new());
            }

            if Self::draw_button_with_icon(
                ui,
                "Убрать пустые",
                egui::vec2(144.0, 30.0),
                Self::paint_trash_icon,
            )
            .clicked()
            {
                self.urls.retain(|url| !url.trim().is_empty());
                if self.urls.is_empty() {
                    self.urls.push(String::new());
                }
            }
        });
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
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.show_url_editor {
                self.draw_url_editor(ui);
                return;
            }

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
                        self.show_url_editor = true;
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
                        .id_salt("components_scroll")
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
                    let downloader_ready = self.managed_yt_dlp_path().is_file();
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

                    let button_enabled = downloader_ready && !has_missing && !is_checking;
                    let btn = egui::Button::new(label).min_size(egui::vec2(120.0, 36.0));

                    if ui.add_enabled(button_enabled, btn).clicked() {
                        self.start_download(ctx);
                    }

                    if !downloader_ready && !is_checking {
                        ui.label(
                            egui::RichText::new("Сначала установите yt-dlp через Обновить.").weak(),
                        );
                    }
                } else {
                    ui.spinner();
                    ui.label("Работаю...");
                }
            });

            ui.add_space(10.0);

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
                                    .font(egui::TextStyle::Body)
                                    .desired_width(f32::INFINITY)
                                    .frame(false)
                                    .interactive(false),
                            );
                        });
                });
        });

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
        Box::new(|cc| Ok(Box::new(YtDlpApp::new(cc)))),
    )
}

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::io::{BufRead, BufReader, Write, Read};
use std::fs::{self, File};
use std::path::{Path, PathBuf}; 
use std::os::windows::process::CommandExt;
use std::env;

use arboard::Clipboard;
use regex::Regex;
use serde::{Serialize, Deserialize};

// --- КОНФИГУРАЦИЯ ---
const CONFIG_FILE: &str = "config.toml";

#[derive(Serialize, Deserialize, Clone, Debug)]
struct AppConfig {
    output_path: String,
    yt_dlp_args: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        // 1. Получаем путь к текущему .exe
        let mut video_path = env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        video_path.pop();
        video_path.push("Video");

        // 2. Создаем папку
        let _ = fs::create_dir_all(&video_path);

        Self {
            output_path: video_path.to_string_lossy().to_string(),
            // ТЕПЕРЬ АРГУМЕНТЫ В ЧЕЛОВЕЧЕСКОМ ВИДЕ
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

// --- ТИПЫ СООБЩЕНИЙ ---
enum AppMessage {
    Log(String),
    ProgressUpdate(f32, f32, String), 
    AllFinished,
}

struct YtDlpApp {
    urls: Vec<String>,
    config: AppConfig,
    logs: String,
    
    is_working: bool,
    show_url_window: bool, 
    
    target_current: f32,
    target_total: f32,
    disp_current: f32,
    disp_total: f32,
    
    status_text: String,

    receiver: Receiver<AppMessage>,
    sender: Sender<AppMessage>,
    re_percent: Regex,
    first_run: bool,
}

impl YtDlpApp {
    fn new(cc: &eframe::CreationContext) -> Self {
        let (sender, receiver) = channel();
        let config = AppConfig::load();
        let ctx = cc.egui_ctx.clone(); 
        let check_sender = sender.clone();

        thread::spawn(move || {
            let dependencies = vec![
                ("yt-dlp", "--version"), 
                ("ffmpeg", "-version"), 
                ("ffprobe", "-version")
            ];
            
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            let mut all_success = true; 

            for (name, arg) in dependencies {
                let cmd = Command::new(name).arg(arg).creation_flags(CREATE_NO_WINDOW).output();
                match cmd {
                    Ok(output) => {
                        if output.status.success() {
                            let s = String::from_utf8_lossy(&output.stdout);
                            let first_line = s.lines().next().unwrap_or("OK").trim(); 
                            let clean_ver = if name.contains("ff") {
                                if let Some(pos) = first_line.find("version") {
                                    let remainder = &first_line[pos + 7..]; 
                                    let raw_ver = remainder.trim().split_whitespace().next().unwrap_or("?");
                                    raw_ver.split('-').next().unwrap_or(raw_ver).to_string()
                                } else { first_line.to_string() }
                            } else { first_line.to_string() };
                            let _ = check_sender.send(AppMessage::Log(format!("{}: {}", name, clean_ver)));
                        } else {
                            all_success = false;
                            let _ = check_sender.send(AppMessage::Log(format!("{}: Не найден", name)));
                        }
                    }
                    Err(_) => {
                        all_success = false;
                        let _ = check_sender.send(AppMessage::Log(format!("{}: Не установлен", name)));
                    }
                }
                ctx.request_repaint();
            }
            if all_success { 
                let _ = check_sender.send(AppMessage::Log(String::from("\nВсе компоненты найдены. Готов к работе.\n"))); 
            } else {
                let _ = check_sender.send(AppMessage::Log(String::from("\nНажмите 'Обновить', чтобы скачать yt-dlp и ffmpeg.\n"))); 
            }
            ctx.request_repaint();
        });

        Self {
            urls: vec![String::new()], 
            config, 
            logs: String::new(), 
            is_working: false,
            show_url_window: false,
            target_current: 0.0,
            target_total: 0.0,
            disp_current: 0.0,
            disp_total: 0.0,
            status_text: String::from("Ожидание..."),
            receiver,
            sender,
            re_percent: Regex::new(r"(\d+(?:\.\d+)?)%").unwrap(),
            first_run: true,
        }
    }

    fn start_download(&mut self, ctx: &egui::Context) {
        let _ = fs::create_dir_all(&self.config.output_path);
        self.config.save();

        let valid_urls: Vec<String> = self.urls.iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if valid_urls.is_empty() { 
            self.logs.push_str(">>> Ошибка: Список ссылок пуст!\n"); 
            return; 
        }

        self.is_working = true;
        self.target_current = 0.0;
        self.target_total = 0.0;
        self.disp_current = 0.0;
        self.disp_total = 0.0;
        self.logs.clear();
        
        let total_files = valid_urls.len();
        self.status_text = format!("Подготовка к скачиванию {} файлов...", total_files);
        self.logs.push_str(&format!(">>> Старт очереди: {} видео\n", total_files));

        let path = self.config.output_path.clone(); 
        let config_args = self.config.yt_dlp_args.clone(); 
        
        let sender = self.sender.clone();
        let re_percent = self.re_percent.clone();
        let thread_ctx = ctx.clone();

        thread::spawn(move || {
            let clean_path = path.trim_end_matches('\\');
            const CREATE_NO_WINDOW: u32 = 0x08000000;

            for (i, url) in valid_urls.iter().enumerate() {
                let current_file_num = i + 1;
                let _ = sender.send(AppMessage::Log(format!(">>> [{}/{}] Запуск: {}", current_file_num, total_files, url)));
                thread_ctx.request_repaint();
                
                let base_total_progress = i as f32 / total_files as f32;
                let _ = sender.send(AppMessage::ProgressUpdate(0.0, base_total_progress, format!("Файл {}/{}: Старт...", current_file_num, total_files)));
                thread_ctx.request_repaint();

                let output_template = format!(r"{}/%(title)s.%(ext)s", clean_path);

                // --- НОВАЯ ЛОГИКА СБОРКИ АРГУМЕНТОВ ---
                let mut args = vec!["--newline".to_string()];
                
                // Проходимся по строкам конфига и разбиваем их по пробелам
                // Например: "--merge-output-format mp4" -> "--merge-output-format", "mp4"
                for arg_line in config_args.iter() {
                    for part in arg_line.split_whitespace() {
                        args.push(part.to_string());
                    }
                }

                args.push("-o".to_string());
                args.push(output_template);
                args.push(url.to_string());
                // --------------------------------------

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
                                    if l.contains("%") && l.contains("download") {
                                        if let Some(caps) = re_percent.captures(&l) {
                                            if let Some(m) = caps.get(1) {
                                                if let Ok(p) = m.as_str().parse::<f32>() {
                                                    let cur_p = p / 100.0;
                                                    let tot_p = (i as f32 + cur_p) / total_files as f32;
                                                    let status = format!("Файл {}/{}: {:.1}%", current_file_num, total_files, p);
                                                    let _ = sender.send(AppMessage::ProgressUpdate(cur_p, tot_p, status));
                                                }
                                            }
                                        }
                                    } else {
                                        let _ = sender.send(AppMessage::Log(l));
                                        thread_ctx.request_repaint();
                                    }
                                }
                            }
                        }
                        let _ = child_process.wait();
                    }
                    Err(e) => {
                        let _ = sender.send(AppMessage::Log(format!("❌ Ошибка запуска: {}", e)));
                        thread_ctx.request_repaint();
                    }
                }
            }
            let _ = sender.send(AppMessage::AllFinished);
            thread_ctx.request_repaint();
        });
    }

    fn start_update(&mut self, ctx: &egui::Context) {
        self.is_working = true;
        self.target_current = 0.0;
        self.target_total = 0.0;
        self.status_text = String::from("Обновление...");
        self.logs.push_str("\n>>> START UPDATE\n");

        let sender = self.sender.clone();
        let thread_ctx = ctx.clone();

        thread::spawn(move || {
            let download_file = |url: &str, filename: &str, sender: &Sender<AppMessage>, ctx: &egui::Context| -> Result<(), String> {
                let _ = sender.send(AppMessage::Log(format!("Скачивание {}...", filename)));
                ctx.request_repaint();

                let mut response = reqwest::blocking::get(url).map_err(|e| e.to_string())?;
                let total_size = response.content_length().unwrap_or(0);
                let mut file = File::create(filename).map_err(|e| e.to_string())?;
                let mut buffer = [0; 8192];
                let mut downloaded: u64 = 0;
                let mut last_repaint = 0;

                loop {
                    let bytes_read = response.read(&mut buffer).map_err(|e| e.to_string())?;
                    if bytes_read == 0 { break; }
                    file.write_all(&buffer[..bytes_read]).map_err(|e| e.to_string())?;
                    downloaded += bytes_read as u64;
                    if total_size > 0 {
                        let percent = downloaded as f32 / total_size as f32;
                        let status = format!("Загрузка {}: {:.0}%", filename, percent * 100.0);
                        let _ = sender.send(AppMessage::ProgressUpdate(percent, percent, status));
                        
                        if downloaded - last_repaint > 100_000 { 
                            ctx.request_repaint();
                            last_repaint = downloaded;
                        }
                    }
                }
                ctx.request_repaint();
                Ok(())
            };

            let _ = sender.send(AppMessage::ProgressUpdate(0.0, 0.0, "Очистка...".to_string()));
            thread_ctx.request_repaint();

            for f in ["yt-dlp.exe", "ffmpeg.exe", "ffprobe.exe"] {
                if Path::new(f).exists() { let _ = fs::remove_file(f); }
            }
            
            if let Err(e) = download_file("https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe", "yt-dlp.exe", &sender, &thread_ctx) {
                let _ = sender.send(AppMessage::Log(format!("Err yt-dlp: {}", e)));
                let _ = sender.send(AppMessage::AllFinished); 
                thread_ctx.request_repaint();
                return;
            }

            let zip_name = "ffmpeg_temp.zip";
            if let Err(e) = download_file("https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip", zip_name, &sender, &thread_ctx) {
                let _ = sender.send(AppMessage::Log(format!("Err ffmpeg: {}", e)));
                let _ = sender.send(AppMessage::AllFinished); 
                thread_ctx.request_repaint();
                return;
            }

            let _ = sender.send(AppMessage::ProgressUpdate(1.0, 1.0, "Распаковка...".to_string()));
            thread_ctx.request_repaint();

            match File::open(zip_name) {
                Ok(file) => {
                    match zip::ZipArchive::new(file) {
                        Ok(mut archive) => {
                            for i in 0..archive.len() {
                                if let Ok(mut file) = archive.by_index(i) {
                                    let name = file.name().to_string();
                                    if name.ends_with("bin/ffmpeg.exe") || name.ends_with("bin/ffprobe.exe") {
                                        let file_name = Path::new(&name).file_name().unwrap().to_str().unwrap();
                                        if let Ok(mut out_file) = File::create(file_name) {
                                            let _ = std::io::copy(&mut file, &mut out_file);
                                        }
                                    }
                                }
                            }
                        },
                        Err(_) => {}
                    }
                },
                Err(_) => {}
            }
            if Path::new(zip_name).exists() { let _ = fs::remove_file(zip_name); }

            let _ = sender.send(AppMessage::Log(">>> Обновление завершено.".to_string()));
            let _ = sender.send(AppMessage::AllFinished);
            thread_ctx.request_repaint();
        });
    }

    fn animate_progress(&mut self, ctx: &egui::Context) {
        let speed = 0.1;
        let diff_cur = self.target_current - self.disp_current;
        let diff_tot = self.target_total - self.disp_total;
        if diff_cur.abs() > 0.001 || diff_tot.abs() > 0.001 {
            self.disp_current += diff_cur * speed;
            self.disp_total += diff_tot * speed;
            ctx.request_repaint(); 
        } else {
            self.disp_current = self.target_current;
            self.disp_total = self.target_total;
        }
    }
}

impl eframe::App for YtDlpApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.first_run {
            let width = 500.0;
            let height = 500.0;
            if let Some(ms) = ctx.input(|i| i.viewport().monitor_size) {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2((ms.x - width)/2.0, (ms.y - height)/2.0)));
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(width, height)));
            self.first_run = false;
        }

        while let Ok(msg) = self.receiver.try_recv() {
            match msg {
                AppMessage::Log(line) => {
                    self.logs.push_str(&line);
                    self.logs.push('\n');
                }
                AppMessage::ProgressUpdate(cur, tot, text) => {
                    self.target_current = cur;
                    self.status_text = text;
                    if tot > self.target_total { self.target_total = tot; }
                }
                AppMessage::AllFinished => {
                    self.is_working = false;
                    self.target_current = 1.0;
                    self.target_total = 1.0;
                    self.status_text = String::from("Готово!");
                    self.logs.push_str(">>> Очередь завершена.\n");
                }
            }
        }

        self.animate_progress(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(5.0);
            
            ui.horizontal(|ui| {
                ui.heading("YouTube Downloader");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !self.is_working {
                        if ui.button("🔄 Обновить").clicked() { self.start_update(ctx); }
                    }
                });
            });
            
            ui.separator();
            ui.add_space(10.0);

            let btn_text = format!("📝 Список ссылок ({})", self.urls.len());
            if ui.add(egui::Button::new(btn_text).min_size(egui::vec2(200.0, 25.0))).clicked() {
                self.show_url_window = true;
            }

            ui.add_space(10.0);
            ui.label("Ссылка (или первая из списка):");
            
            if self.urls.is_empty() { self.urls.push(String::new()); }
            
            let url_edit = ui.add(egui::TextEdit::singleline(&mut self.urls[0]).desired_width(f32::INFINITY));
            url_edit.context_menu(|ui| {
                 if ui.button("Вставить").clicked() {
                    if let Ok(mut c) = Clipboard::new() { if let Ok(t) = c.get_text() { self.urls[0] = t; } }
                    ui.close_menu();
                }
                if ui.button("Очистить").clicked() { self.urls[0].clear(); ui.close_menu(); }
            });

            ui.add_space(10.0);
            ui.label("Папка сохранения (меняется в config.toml):");
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut self.config.output_path);
            });

            ui.add_space(20.0);

            if self.is_working {
                ui.label(&self.status_text);
                ui.label("Общий прогресс:");
                ui.add(egui::ProgressBar::new(self.disp_total).show_percentage().animate(true));
                ui.add_space(5.0);
                ui.label("Текущая операция:");
                ui.add(egui::ProgressBar::new(self.disp_current).show_percentage().animate(true));
            } else {
                let label = if self.urls.len() > 1 && !self.urls[1].is_empty() { "СКАЧАТЬ ВСЕ" } else { "СКАЧАТЬ" };
                if ui.add(egui::Button::new(label).min_size(egui::vec2(120.0, 40.0))).clicked() {
                    self.start_download(ctx);
                }
            }

            ui.separator();
            ui.label("Лог событий:");
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                ui.add_sized(
                    ui.available_size(), 
                    egui::TextEdit::multiline(&mut self.logs)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .interactive(false) 
                );
            });
        });

        if self.show_url_window {
            let mut show_window = true;
            let mut close_clicked = false;
            egui::Window::new("Управление списком ссылок")
                .open(&mut show_window)
                .collapsible(false)
                .resizable(true)
                .default_size([400.0, 300.0])
                .show(ctx, |ui| {
                    ui.label("Вы можете добавить несколько ссылок здесь:");
                    ui.add_space(5.0);
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        let mut remove_idx = None;
                        for (i, url) in self.urls.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(format!("{}.", i + 1));
                                let te = ui.add(egui::TextEdit::singleline(url).desired_width(280.0));
                                te.context_menu(|ui| {
                                    if ui.button("Вставить").clicked() {
                                        if let Ok(mut c) = Clipboard::new() { if let Ok(t) = c.get_text() { *url = t; } }
                                        ui.close_menu();
                                    }
                                    if ui.button("Очистить").clicked() { url.clear(); ui.close_menu(); }
                                });
                                if ui.button("❌").clicked() { remove_idx = Some(i); }
                            });
                        }
                        if let Some(i) = remove_idx {
                            if self.urls.len() > 1 { self.urls.remove(i); } else { self.urls[0].clear(); }
                        }
                        ui.add_space(5.0);
                        if ui.button("➕ Добавить поле").clicked() { self.urls.push(String::new()); }
                    });
                    ui.add_space(10.0);
                    if ui.button("Готово / Закрыть").clicked() { close_clicked = true; }
                });
            if !show_window || close_clicked { self.show_url_window = false; }
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([500.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native("Multi Downloader", options, Box::new(|cc| Box::new(YtDlpApp::new(cc))))
}
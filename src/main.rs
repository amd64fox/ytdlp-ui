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
use std::sync::{
    mpsc::{channel, Receiver, Sender},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use arboard::Clipboard;
use serde::{Deserialize, Serialize};

const CONFIG_FILE: &str = "config.toml";
const APP_CONFIG_DIR: &str = "ytdlp-ui";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAIN_WINDOW_SIZE: [f32; 2] = [500.0, 356.0];
const PAGE_WINDOW_SIZE: [f32; 2] = [500.0, 476.0];
const LOG_WINDOW_SIZE: [f32; 2] = [500.0, 556.0];
const LOG_AREA_HEIGHT: f32 = 160.0;
const ICON_STROKE_WIDTH: f32 = 1.2;
const NATIVE_COLOR_DEFAULT: u32 = 0xFFFF_FFFF;
const RAW_PROFILE_ID: &str = "builtin.raw";
const MP4_1080_PROFILE_ID: &str = "builtin.mp4_1080";
const MP4_720_PROFILE_ID: &str = "builtin.mp4_720";
const NO_SPONSORS_PROFILE_ID: &str = "builtin.no_sponsors";
const AUDIO_MP3_PROFILE_ID: &str = "builtin.audio_mp3";
const AUDIO_M4A_PROFILE_ID: &str = "builtin.audio_m4a";
const LEGACY_PROFILE_ID: &str = "custom.legacy";
const DOWNLOAD_ARCHIVE_FILE: &str = "download-archive.txt";
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

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    const OPTIONS: &'static [(Self, &'static str)] = &[
        (Self::System, "Системная"),
        (Self::Light, "Светлая"),
        (Self::Dark, "Тёмная"),
    ];

    fn tooltip(self) -> &'static str {
        match self {
            Self::System => "Тема: системная, переключается вместе с Windows",
            Self::Light => "Тема: светлая, зафиксирована",
            Self::Dark => "Тема: тёмная, зафиксирована",
        }
    }

    fn preference(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn system_theme(self) -> egui::SystemTheme {
        match self {
            Self::System => egui::SystemTheme::SystemDefault,
            Self::Light => egui::SystemTheme::Light,
            Self::Dark => egui::SystemTheme::Dark,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct AppConfig {
    #[serde(default)]
    output_path: String,
    #[serde(default)]
    theme_mode: ThemeMode,
    #[serde(default = "default_active_profile_id")]
    active_profile_id: String,
    #[serde(default)]
    custom_profiles: Vec<DownloadProfile>,
    #[serde(default, rename = "yt_dlp_args", skip_serializing)]
    legacy_yt_dlp_args: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct DownloadProfile {
    id: String,
    name: String,
    #[serde(default)]
    kind: DownloadKind,
    #[serde(default)]
    video_resolution: VideoResolution,
    #[serde(default)]
    container: ContainerFormat,
    #[serde(default)]
    audio_format: AudioFormat,
    #[serde(default)]
    file_name_template: FileNameTemplate,
    #[serde(default)]
    sponsorblock: SponsorBlockMode,
    #[serde(default = "default_sponsorblock_categories")]
    sponsorblock_categories: Vec<String>,
    #[serde(default)]
    subtitles: SubtitleMode,
    #[serde(default = "default_subtitle_langs")]
    subtitle_langs: String,
    #[serde(default)]
    embed_metadata: bool,
    #[serde(default)]
    embed_thumbnail: bool,
    #[serde(default)]
    embed_chapters: bool,
    #[serde(default)]
    playlist_mode: PlaylistMode,
    #[serde(default)]
    use_download_archive: bool,
    #[serde(default)]
    extra_args: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DownloadKind {
    Video,
    AudioOnly,
}

impl Default for DownloadKind {
    fn default() -> Self {
        Self::Video
    }
}

impl DownloadKind {
    const OPTIONS: &'static [(Self, &'static str)] =
        &[(Self::Video, "Видео"), (Self::AudioOnly, "Только аудио")];
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
enum VideoResolution {
    #[serde(rename = "best")]
    Best,
    #[serde(rename = "2160p")]
    P2160,
    #[serde(rename = "1440p")]
    P1440,
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "720p")]
    P720,
    #[serde(rename = "480p")]
    P480,
    #[serde(rename = "360p")]
    P360,
}

impl Default for VideoResolution {
    fn default() -> Self {
        Self::Best
    }
}

impl VideoResolution {
    const OPTIONS: &'static [(Self, &'static str)] = &[
        (Self::Best, "Лучшее"),
        (Self::P2160, "2160p"),
        (Self::P1440, "1440p"),
        (Self::P1080, "1080p"),
        (Self::P720, "720p"),
        (Self::P480, "480p"),
        (Self::P360, "360p"),
    ];

    fn sort_value(self) -> Option<&'static str> {
        match self {
            Self::Best => None,
            Self::P2160 => Some("res:2160"),
            Self::P1440 => Some("res:1440"),
            Self::P1080 => Some("res:1080"),
            Self::P720 => Some("res:720"),
            Self::P480 => Some("res:480"),
            Self::P360 => Some("res:360"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ContainerFormat {
    Auto,
    Mp4,
    Mkv,
    Webm,
}

impl Default for ContainerFormat {
    fn default() -> Self {
        Self::Auto
    }
}

impl ContainerFormat {
    const OPTIONS: &'static [(Self, &'static str)] = &[
        (Self::Auto, "Авто"),
        (Self::Mp4, "MP4"),
        (Self::Mkv, "MKV"),
        (Self::Webm, "WebM"),
    ];

    fn arg_value(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Mp4 => Some("mp4"),
            Self::Mkv => Some("mkv"),
            Self::Webm => Some("webm"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AudioFormat {
    Best,
    Mp3,
    M4a,
    Opus,
    Flac,
    Wav,
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self::Best
    }
}

impl AudioFormat {
    const OPTIONS: &'static [(Self, &'static str)] = &[
        (Self::Best, "Лучшее"),
        (Self::Mp3, "MP3"),
        (Self::M4a, "M4A"),
        (Self::Opus, "Opus"),
        (Self::Flac, "FLAC"),
        (Self::Wav, "WAV"),
    ];

    fn arg_value(self) -> Option<&'static str> {
        match self {
            Self::Best => None,
            Self::Mp3 => Some("mp3"),
            Self::M4a => Some("m4a"),
            Self::Opus => Some("opus"),
            Self::Flac => Some("flac"),
            Self::Wav => Some("wav"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FileNameTemplate {
    Title,
    ArtistTrack,
}

impl Default for FileNameTemplate {
    fn default() -> Self {
        Self::Title
    }
}

impl FileNameTemplate {
    const OPTIONS: &'static [(Self, &'static str)] = &[
        (Self::Title, "Название"),
        (Self::ArtistTrack, "Исполнитель - трек"),
    ];
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SponsorBlockMode {
    Off,
    Mark,
    Remove,
}

impl Default for SponsorBlockMode {
    fn default() -> Self {
        Self::Off
    }
}

impl SponsorBlockMode {
    const OPTIONS: &'static [(Self, &'static str)] = &[
        (Self::Off, "Выкл"),
        (Self::Mark, "Отметить"),
        (Self::Remove, "Вырезать"),
    ];

    fn flag(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Mark => Some("--sponsorblock-mark"),
            Self::Remove => Some("--sponsorblock-remove"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SubtitleMode {
    Off,
    Manual,
    Auto,
}

impl Default for SubtitleMode {
    fn default() -> Self {
        Self::Off
    }
}

impl SubtitleMode {
    const OPTIONS: &'static [(Self, &'static str)] = &[
        (Self::Off, "Выкл"),
        (Self::Manual, "Обычные"),
        (Self::Auto, "Авто"),
    ];
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PlaylistMode {
    Auto,
    SingleVideo,
    Playlist,
}

impl Default for PlaylistMode {
    fn default() -> Self {
        Self::Auto
    }
}

impl PlaylistMode {
    const OPTIONS: &'static [(Self, &'static str)] = &[
        (Self::Auto, "Авто"),
        (Self::SingleVideo, "Одно видео"),
        (Self::Playlist, "Плейлист"),
    ];

    fn flag(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::SingleVideo => Some("--no-playlist"),
            Self::Playlist => Some("--yes-playlist"),
        }
    }
}

fn default_active_profile_id() -> String {
    RAW_PROFILE_ID.to_string()
}

fn default_sponsorblock_categories() -> Vec<String> {
    vec!["sponsor".to_string(), "selfpromo".to_string()]
}

fn default_subtitle_langs() -> String {
    "ru,en".to_string()
}

fn sponsorblock_category_options() -> &'static [(&'static str, &'static str)] {
    &[
        ("sponsor", "Спонсор"),
        ("selfpromo", "Самореклама"),
        ("intro", "Интро"),
        ("outro", "Аутро"),
        ("interaction", "Лайки/подписка"),
        ("preview", "Превью"),
        ("music_offtopic", "Оффтоп"),
        ("filler", "Филлер"),
    ]
}

impl DownloadProfile {
    fn base(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: DownloadKind::Video,
            video_resolution: VideoResolution::Best,
            container: ContainerFormat::Auto,
            audio_format: AudioFormat::Best,
            file_name_template: FileNameTemplate::Title,
            sponsorblock: SponsorBlockMode::Off,
            sponsorblock_categories: default_sponsorblock_categories(),
            subtitles: SubtitleMode::Off,
            subtitle_langs: default_subtitle_langs(),
            embed_metadata: false,
            embed_thumbnail: false,
            embed_chapters: false,
            playlist_mode: PlaylistMode::Auto,
            use_download_archive: false,
            extra_args: Vec::new(),
        }
    }

    fn custom_default(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self::base(id, name)
    }

    fn to_yt_dlp_args(&self, archive_path: &Path) -> Vec<String> {
        let mut args = Vec::new();

        match self.kind {
            DownloadKind::Video => {
                if let Some(sort_value) = self.video_resolution.sort_value() {
                    args.push("-S".to_string());
                    args.push(sort_value.to_string());
                }
                if let Some(container) = self.container.arg_value() {
                    args.push("--merge-output-format".to_string());
                    args.push(container.to_string());
                    args.push("--remux-video".to_string());
                    args.push(container.to_string());
                }
            }
            DownloadKind::AudioOnly => {
                args.push("-x".to_string());
                if let Some(audio_format) = self.audio_format.arg_value() {
                    args.push("--audio-format".to_string());
                    args.push(audio_format.to_string());
                }
            }
        }

        if let Some(flag) = self.sponsorblock.flag() {
            let categories = self.sponsorblock_categories_arg();
            args.push(flag.to_string());
            args.push(categories);
        }

        match self.subtitles {
            SubtitleMode::Off => {}
            SubtitleMode::Manual => args.push("--write-subs".to_string()),
            SubtitleMode::Auto => args.push("--write-auto-subs".to_string()),
        }

        if self.subtitles != SubtitleMode::Off && !self.subtitle_langs.trim().is_empty() {
            args.push("--sub-langs".to_string());
            args.push(self.subtitle_langs.trim().to_string());
        }

        if self.embed_metadata {
            args.push("--embed-metadata".to_string());
        }
        if self.embed_thumbnail {
            args.push("--embed-thumbnail".to_string());
        }
        if self.embed_chapters {
            args.push("--embed-chapters".to_string());
        }
        if let Some(flag) = self.playlist_mode.flag() {
            args.push(flag.to_string());
        }
        if self.use_download_archive {
            args.push("--download-archive".to_string());
            args.push(archive_path.to_string_lossy().to_string());
        }

        args.extend(
            self.extra_args
                .iter()
                .map(|arg| arg.trim())
                .filter(|arg| !arg.is_empty())
                .map(ToString::to_string),
        );
        args
    }

    fn output_template(&self, output_path: &str) -> String {
        let clean_path = output_path.trim_end_matches(['\\', '/']);
        match (self.kind, self.file_name_template) {
            (DownloadKind::AudioOnly, FileNameTemplate::ArtistTrack) => format!(
                r"{clean_path}/%(artist,uploader|Unknown Artist)s - %(track,title)s.%(ext)s"
            ),
            _ => format!(r"{clean_path}/%(title)s.%(ext)s"),
        }
    }

    fn sponsorblock_categories_arg(&self) -> String {
        let categories: Vec<&str> = self
            .sponsorblock_categories
            .iter()
            .map(|category| category.trim())
            .filter(|category| !category.is_empty())
            .collect();

        if categories.is_empty() {
            "sponsor,selfpromo".to_string()
        } else {
            categories.join(",")
        }
    }
}

fn builtin_profiles() -> Vec<DownloadProfile> {
    let mut mp4_1080 = DownloadProfile::base(MP4_1080_PROFILE_ID, "MP4 1080p");
    mp4_1080.video_resolution = VideoResolution::P1080;
    mp4_1080.container = ContainerFormat::Mp4;

    let mut mp4_720 = DownloadProfile::base(MP4_720_PROFILE_ID, "MP4 720p");
    mp4_720.video_resolution = VideoResolution::P720;
    mp4_720.container = ContainerFormat::Mp4;

    let mut no_sponsors = DownloadProfile::base(NO_SPONSORS_PROFILE_ID, "Видео без спонсоров");
    no_sponsors.video_resolution = VideoResolution::P1080;
    no_sponsors.container = ContainerFormat::Mp4;
    no_sponsors.sponsorblock = SponsorBlockMode::Remove;

    let mut audio_mp3 = DownloadProfile::base(AUDIO_MP3_PROFILE_ID, "Аудио MP3");
    audio_mp3.kind = DownloadKind::AudioOnly;
    audio_mp3.audio_format = AudioFormat::Mp3;

    let mut audio_m4a = DownloadProfile::base(AUDIO_M4A_PROFILE_ID, "Аудио M4A");
    audio_m4a.kind = DownloadKind::AudioOnly;
    audio_m4a.audio_format = AudioFormat::M4a;

    vec![
        DownloadProfile::base(RAW_PROFILE_ID, "Обычное видео"),
        mp4_1080,
        mp4_720,
        no_sponsors,
        audio_mp3,
        audio_m4a,
    ]
}

fn builtin_profile_by_id(id: &str) -> Option<DownloadProfile> {
    builtin_profiles()
        .into_iter()
        .find(|profile| profile.id == id)
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            output_path: String::new(),
            theme_mode: ThemeMode::System,
            active_profile_id: default_active_profile_id(),
            custom_profiles: Vec::new(),
            legacy_yt_dlp_args: Vec::new(),
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
                Ok(mut cfg) => {
                    let (normalized, normalize_messages) = cfg.normalize_after_load(app_dir);
                    messages.extend(normalize_messages);
                    if let Err(err) = fs::create_dir_all(&cfg.output_path) {
                        messages.push(format!(
                            ">>> Не удалось подготовить папку загрузок {}: {err}",
                            cfg.output_path
                        ));
                    }
                    if normalized {
                        if let Err(err) = cfg.save(config_path) {
                            messages.push(format!(
                                ">>> Не удалось сохранить конфиг {}: {err}",
                                config_path.display()
                            ));
                        }
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

    fn normalize_after_load(&mut self, app_dir: &Path) -> (bool, Vec<String>) {
        let mut changed = false;
        let mut messages = Vec::new();

        if self.output_path.trim().is_empty() {
            self.output_path = app_dir.join("Video").to_string_lossy().to_string();
            changed = true;
        }

        if !self.legacy_yt_dlp_args.is_empty() {
            let mut profile =
                DownloadProfile::custom_default(LEGACY_PROFILE_ID, "Старые настройки");
            profile.extra_args = Self::legacy_args_to_extra_args(&self.legacy_yt_dlp_args);

            match self
                .custom_profiles
                .iter_mut()
                .find(|custom| custom.id == LEGACY_PROFILE_ID)
            {
                Some(existing) => *existing = profile,
                None => self.custom_profiles.push(profile),
            }
            self.active_profile_id = LEGACY_PROFILE_ID.to_string();
            self.legacy_yt_dlp_args.clear();
            messages.push(
                ">>> Старые параметры yt-dlp перенесены в профиль \"Старые настройки\"".to_string(),
            );
            changed = true;
        }

        if self.active_profile_id.trim().is_empty() || !self.has_profile(&self.active_profile_id) {
            self.active_profile_id = RAW_PROFILE_ID.to_string();
            changed = true;
        }

        (changed, messages)
    }

    fn legacy_args_to_extra_args(args: &[String]) -> Vec<String> {
        args.iter()
            .flat_map(|line| line.split_whitespace())
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(ToString::to_string)
            .collect()
    }

    fn has_profile(&self, id: &str) -> bool {
        builtin_profile_by_id(id).is_some() || self.custom_profiles.iter().any(|p| p.id == id)
    }

    fn active_profile(&self) -> DownloadProfile {
        self.custom_profiles
            .iter()
            .find(|profile| profile.id == self.active_profile_id)
            .cloned()
            .or_else(|| builtin_profile_by_id(&self.active_profile_id))
            .unwrap_or_else(|| DownloadProfile::base(RAW_PROFILE_ID, "Обычное видео"))
    }

    fn active_profile_name(&self) -> String {
        self.active_profile().name
    }

    fn active_custom_profile_index(&self) -> Option<usize> {
        self.custom_profiles
            .iter()
            .position(|profile| profile.id == self.active_profile_id)
    }

    fn next_custom_profile_id(&self) -> String {
        let mut index = self.custom_profiles.len() + 1;
        loop {
            let id = format!("custom-{index}");
            if !self.has_profile(&id) {
                return id;
            }
            index += 1;
        }
    }
}

enum AppMessage {
    Log(String),
    Status(StatusMessage),
    UpdateSnapshot(Vec<updater::ComponentInfo>),
    UpdatingComponent(Option<updater::ComponentKind>),
    AllFinished(FinishState),
}

#[derive(Clone)]
struct StatusMessage {
    tone: StatusTone,
    title: String,
    detail: String,
    progress: Option<(usize, usize)>,
}

#[derive(Clone, Copy)]
enum StatusTone {
    Idle,
    Running,
    Success,
    Warning,
    Error,
}

struct FinishState {
    had_error: bool,
    restart_required: bool,
    title: String,
    detail: String,
}

impl StatusMessage {
    fn idle() -> Self {
        Self::new(
            StatusTone::Idle,
            "Готов к загрузке",
            "Добавьте ссылку и нажмите Скачать",
            None,
        )
    }

    fn new(
        tone: StatusTone,
        title: impl Into<String>,
        detail: impl Into<String>,
        progress: Option<(usize, usize)>,
    ) -> Self {
        Self {
            tone,
            title: title.into(),
            detail: detail.into(),
            progress,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UiPalette {
    background: egui::Color32,
    group_bg: egui::Color32,
    input_bg: egui::Color32,
    stroke: egui::Color32,
    button_bg: egui::Color32,
    button_hover: egui::Color32,
    button_active: egui::Color32,
    button_text: egui::Color32,
    secondary_text: egui::Color32,
    disabled_fade: egui::Color32,
    hover_stroke: egui::Color32,
    skeleton_dot: egui::Color32,
    skeleton_base: egui::Color32,
    skeleton_highlight: egui::Color32,
    success: egui::Color32,
}

struct UiTheme;

impl UiTheme {
    const DARK: UiPalette = UiPalette {
        background: egui::Color32::from_rgb(18, 18, 18),
        group_bg: egui::Color32::from_rgb(24, 24, 24),
        input_bg: egui::Color32::from_rgb(10, 10, 10),
        stroke: egui::Color32::from_rgb(65, 65, 65),
        button_bg: egui::Color32::from_rgb(45, 45, 45),
        button_hover: egui::Color32::from_rgb(70, 70, 70),
        button_active: egui::Color32::from_rgb(90, 90, 90),
        button_text: egui::Color32::from_gray(220),
        secondary_text: egui::Color32::from_gray(84),
        disabled_fade: egui::Color32::from_gray(27),
        hover_stroke: egui::Color32::WHITE,
        skeleton_dot: egui::Color32::from_rgb(42, 42, 42),
        skeleton_base: egui::Color32::from_rgb(36, 36, 36),
        skeleton_highlight: egui::Color32::from_rgb(76, 76, 76),
        success: egui::Color32::from_rgb(100, 200, 100),
    };

    const LIGHT: UiPalette = UiPalette {
        background: egui::Color32::from_rgb(245, 245, 245),
        group_bg: egui::Color32::WHITE,
        input_bg: egui::Color32::from_rgb(248, 248, 248),
        stroke: egui::Color32::from_rgb(190, 190, 190),
        button_bg: egui::Color32::from_rgb(232, 232, 232),
        button_hover: egui::Color32::from_rgb(218, 218, 218),
        button_active: egui::Color32::from_rgb(205, 205, 205),
        button_text: egui::Color32::from_gray(35),
        secondary_text: egui::Color32::from_gray(105),
        disabled_fade: egui::Color32::from_gray(190),
        hover_stroke: egui::Color32::from_gray(105),
        skeleton_dot: egui::Color32::from_rgb(210, 210, 210),
        skeleton_base: egui::Color32::from_rgb(220, 220, 220),
        skeleton_highlight: egui::Color32::from_rgb(242, 242, 242),
        success: egui::Color32::from_rgb(35, 140, 60),
    };

    fn for_dark_mode(dark_mode: bool) -> UiPalette {
        if dark_mode {
            Self::DARK
        } else {
            Self::LIGHT
        }
    }

    fn for_ui(ui: &egui::Ui) -> UiPalette {
        Self::for_dark_mode(ui.visuals().dark_mode)
    }

    fn for_ctx(ctx: &egui::Context) -> UiPalette {
        Self::for_dark_mode(ctx.theme() == egui::Theme::Dark)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NativeTitleBarStyle {
    dark_mode: bool,
    caption_color: u32,
    text_color: u32,
    use_system_theme: bool,
}

impl NativeTitleBarStyle {
    fn for_theme(mode: ThemeMode, effective_dark_mode: bool) -> Self {
        let dark_mode = match mode {
            ThemeMode::System => effective_dark_mode,
            ThemeMode::Light => false,
            ThemeMode::Dark => true,
        };

        if mode == ThemeMode::System {
            return Self {
                dark_mode,
                caption_color: NATIVE_COLOR_DEFAULT,
                text_color: NATIVE_COLOR_DEFAULT,
                use_system_theme: true,
            };
        }

        let palette = UiTheme::for_dark_mode(dark_mode);
        Self {
            dark_mode,
            caption_color: colorref(palette.background),
            text_color: colorref(palette.button_text),
            use_system_theme: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeTitleBarApplyResult {
    Applied,
    UseSystemTheme,
    Retry,
}

fn colorref(color: egui::Color32) -> u32 {
    u32::from(color.r()) | (u32::from(color.g()) << 8) | (u32::from(color.b()) << 16)
}

fn selected_text(text: &str, cursor_range: Option<egui::text::CCursorRange>) -> Option<&str> {
    cursor_range
        .filter(|range| !range.is_empty())
        .map(|range| range.slice_str(text))
}

fn restore_text_selection(
    ctx: &egui::Context,
    id: egui::Id,
    cursor_range: Option<egui::text::CCursorRange>,
) {
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        state.cursor.set_char_range(cursor_range);
        egui::TextEdit::store_state(ctx, id, state);
    }
}

fn preserve_text_selection_for_context_menu(
    ui: &egui::Ui,
    response: &egui::Response,
    id: egui::Id,
    cursor_range: Option<egui::text::CCursorRange>,
) -> bool {
    let secondary_pressed =
        response.hovered() && ui.input(|input| input.pointer.secondary_pressed());
    let preserve_selection = secondary_pressed || response.secondary_clicked();

    if preserve_selection {
        restore_text_selection(ui.ctx(), id, cursor_range);
    }

    preserve_selection
}

#[cfg(target_os = "windows")]
fn apply_native_title_bar_style(
    frame: &eframe::Frame,
    style: NativeTitleBarStyle,
) -> NativeTitleBarApplyResult {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    };
    use windows_sys::Win32::Graphics::Gdi::{RedrawWindow, RDW_FRAME, RDW_INVALIDATE};

    let Ok(window_handle) = frame.window_handle() else {
        return NativeTitleBarApplyResult::Retry;
    };
    let RawWindowHandle::Win32(window_handle) = window_handle.as_raw() else {
        return NativeTitleBarApplyResult::Retry;
    };

    let hwnd = window_handle.hwnd.get() as windows_sys::Win32::Foundation::HWND;
    let dark_mode: BOOL = style.dark_mode.into();

    // SAFETY: hwnd belongs to the live eframe window and attribute pointers are valid for each call
    unsafe {
        let caption_result = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR as u32,
            &style.caption_color as *const _ as _,
            std::mem::size_of_val(&style.caption_color) as u32,
        );
        let text_result = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TEXT_COLOR as u32,
            &style.text_color as *const _ as _,
            std::mem::size_of_val(&style.text_color) as u32,
        );

        if caption_result < 0 || text_result < 0 {
            let default_color = NATIVE_COLOR_DEFAULT;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_CAPTION_COLOR as u32,
                &default_color as *const _ as _,
                std::mem::size_of_val(&default_color) as u32,
            );
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_TEXT_COLOR as u32,
                &default_color as *const _ as _,
                std::mem::size_of_val(&default_color) as u32,
            );
            return NativeTitleBarApplyResult::UseSystemTheme;
        }

        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &dark_mode as *const _ as _,
            std::mem::size_of_val(&dark_mode) as u32,
        );
        RedrawWindow(
            hwnd,
            std::ptr::null(),
            std::ptr::null_mut(),
            RDW_FRAME | RDW_INVALIDATE,
        );
    }

    NativeTitleBarApplyResult::Applied
}

#[cfg(not(target_os = "windows"))]
fn apply_native_title_bar_style(
    _frame: &eframe::Frame,
    _style: NativeTitleBarStyle,
) -> NativeTitleBarApplyResult {
    NativeTitleBarApplyResult::Applied
}

fn configure_global_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        let palette = UiTheme::for_dark_mode(style.visuals.dark_mode);
        let corner_radius = egui::CornerRadius::same(4);

        style.visuals.window_corner_radius = corner_radius;
        style.visuals.widgets.noninteractive.corner_radius = corner_radius;
        style.visuals.widgets.inactive.corner_radius = corner_radius;
        style.visuals.widgets.hovered.corner_radius = corner_radius;
        style.visuals.widgets.active.corner_radius = corner_radius;

        style.visuals.widgets.inactive.bg_fill = palette.button_bg;
        style.visuals.widgets.inactive.weak_bg_fill = palette.button_bg;
        style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, palette.stroke);
        style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, palette.button_text);
        style.visuals.widgets.noninteractive.weak_bg_fill = palette.disabled_fade;

        style.visuals.widgets.hovered.bg_fill = palette.button_hover;
        style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, palette.hover_stroke);
        style.visuals.widgets.active.bg_fill = palette.button_active;

        style.visuals.panel_fill = palette.background;
        style.visuals.window_fill = palette.background;
        style.visuals.window_stroke = egui::Stroke::new(1.0, palette.stroke);
        style.visuals.extreme_bg_color = palette.input_bg;
    });
}

fn apply_theme(ctx: &egui::Context, mode: ThemeMode) {
    ctx.set_theme(mode.preference());
}

fn selected_update_targets(
    candidates: &[updater::ComponentInfo],
    selected: &[updater::ComponentKind],
) -> Vec<updater::ComponentInfo> {
    let mut result = Vec::new();

    if let Some(component) = candidates.iter().find(|component| {
        component.kind == updater::ComponentKind::YtDlp && selected.contains(&component.kind)
    }) {
        result.push(component.clone());
    }

    let ffmpeg_selected = selected.contains(&updater::ComponentKind::Ffmpeg);
    let ffprobe_selected = selected.contains(&updater::ComponentKind::Ffprobe);

    if ffmpeg_selected || ffprobe_selected {
        let template = candidates
            .iter()
            .find(|component| {
                matches!(
                    component.kind,
                    updater::ComponentKind::Ffmpeg | updater::ComponentKind::Ffprobe
                ) && component.download_url.is_some()
            })
            .or_else(|| {
                candidates.iter().find(|component| {
                    matches!(
                        component.kind,
                        updater::ComponentKind::Ffmpeg | updater::ComponentKind::Ffprobe
                    )
                })
            });

        if let Some(template) = template {
            let mut bundled = template.clone();
            bundled.kind = match (ffmpeg_selected, ffprobe_selected) {
                (true, true) => updater::ComponentKind::FfmpegBundle,
                (true, false) => updater::ComponentKind::Ffmpeg,
                (false, true) => updater::ComponentKind::Ffprobe,
                (false, false) => unreachable!(),
            };
            bundled.title = match bundled.kind {
                updater::ComponentKind::FfmpegBundle => "ffmpeg/ffprobe",
                updater::ComponentKind::Ffmpeg => "ffmpeg",
                updater::ComponentKind::Ffprobe => "ffprobe",
                updater::ComponentKind::YtDlpGui | updater::ComponentKind::YtDlp => unreachable!(),
            }
            .to_string();
            result.push(bundled);
        }
    }

    if let Some(component) = candidates.iter().find(|component| {
        component.kind == updater::ComponentKind::YtDlpGui && selected.contains(&component.kind)
    }) {
        result.push(component.clone());
    }

    result
}

struct YtDlpApp {
    urls: Vec<String>,
    config: AppConfig,
    logs: String,
    status: StatusMessage,

    is_working: bool,
    show_logs: bool,
    show_url_editor: bool,
    show_settings: bool,
    show_about: bool,
    show_update_confirm: bool,
    center_confirm_window_on_open: bool,
    profile_dialog: Option<ProfileDialog>,
    profile_dialog_text: String,
    profile_dialog_needs_focus: bool,

    receiver: Receiver<AppMessage>,
    sender: Sender<AppMessage>,
    component_states: Vec<updater::ComponentInfo>,
    selected_update_components: Vec<updater::ComponentKind>,
    updating_component: Option<updater::ComponentKind>,
    app_dir: PathBuf,
    config_path: PathBuf,
    native_title_bar_sync_delay: u8,
    last_native_title_bar_style: Option<NativeTitleBarStyle>,
}

#[derive(Clone)]
enum ProfileDialog {
    Create,
    Duplicate(DownloadProfile),
    Rename { id: String },
    Delete { id: String, name: String },
}

impl YtDlpApp {
    fn update_confirm_viewport_id() -> egui::ViewportId {
        egui::ViewportId::from_hash_of("update_confirm_viewport")
    }

    fn send_log(sender: &Sender<AppMessage>, ctx: &egui::Context, message: impl Into<String>) {
        let _ = sender.send(AppMessage::Log(message.into()));
        ctx.request_repaint();
    }

    fn send_status(sender: &Sender<AppMessage>, ctx: &egui::Context, status: StatusMessage) {
        let _ = sender.send(AppMessage::Status(status));
        ctx.request_repaint();
    }

    fn set_local_error(&mut self, title: impl Into<String>, detail: impl Into<String>) {
        let title = title.into();
        let detail = detail.into();
        self.logs.push_str(&format!(">>> {title}: {detail}\n"));
        self.status = StatusMessage::new(StatusTone::Error, title, detail, None);
    }

    fn extract_yt_dlp_error_line(line: &str) -> Option<String> {
        let clean = line
            .trim()
            .strip_prefix("stderr | ")
            .unwrap_or_else(|| line.trim())
            .trim();
        let (_, message) = clean.split_once("ERROR:")?;
        let message = message.trim();
        if message.is_empty() {
            None
        } else {
            Some(message.to_string())
        }
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
        error_sink: Option<Arc<Mutex<Option<String>>>>,
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
                        if let Some(error) = Self::extract_yt_dlp_error_line(&rendered) {
                            if let Some(error_sink) = &error_sink {
                                if let Ok(mut latest_error) = error_sink.lock() {
                                    *latest_error = Some(error);
                                }
                            }
                        }
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

    fn open_in_explorer(path: &Path) -> Result<(), String> {
        Command::new("explorer")
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    fn open_output_path(&mut self) {
        let path = PathBuf::from(&self.config.output_path);
        if let Err(err) = fs::create_dir_all(&path) {
            self.set_local_error("Не удалось открыть папку", err.to_string());
            return;
        }

        if let Err(err) = Self::open_in_explorer(&path) {
            self.set_local_error("Не удалось открыть папку", err);
        }
    }

    fn open_config_file(&mut self) {
        if let Err(err) = self.config.save(&self.config_path) {
            self.set_local_error("Не удалось сохранить конфиг", err);
            return;
        }

        if let Err(err) = Self::open_in_explorer(&self.config_path) {
            self.set_local_error("Не удалось открыть конфиг", err);
        }
    }

    fn choose_output_path(&mut self) {
        let start_dir = PathBuf::from(&self.config.output_path);
        let mut dialog = rfd::FileDialog::new().set_title("Выберите папку сохранения");

        if start_dir.is_dir() {
            dialog = dialog.set_directory(&start_dir);
        }

        if let Some(path) = dialog.pick_folder() {
            self.config.output_path = path.to_string_lossy().to_string();
            match self.config.save(&self.config_path) {
                Ok(()) => {
                    self.status = StatusMessage::new(
                        StatusTone::Success,
                        "Путь сохранения обновлен",
                        self.config.output_path.clone(),
                        None,
                    );
                    self.logs.push_str(&format!(
                        ">>> Путь сохранения: {}\n",
                        self.config.output_path
                    ));
                }
                Err(err) => self.set_local_error("Не удалось сохранить конфиг", err),
            }
        }
    }

    fn save_config_to_disk(&mut self) {
        if let Err(err) = self.config.save(&self.config_path) {
            self.set_local_error("Не удалось сохранить конфиг", err);
        }
    }

    fn spawn_update_check(sender: Sender<AppMessage>, ctx: egui::Context, app_dir: PathBuf) {
        thread::spawn(move || {
            let report = updater::check_for_updates(&app_dir);
            for warning in report.warnings {
                let detail = warning.trim_start_matches(">>>").trim().to_string();
                Self::send_log(&sender, &ctx, warning);
                Self::send_status(
                    &sender,
                    &ctx,
                    StatusMessage::new(
                        StatusTone::Warning,
                        "Проверка обновлений не завершена",
                        detail,
                        None,
                    ),
                );
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
        let mut status = StatusMessage::idle();

        configure_fonts(&ctx);
        configure_global_style(&ctx);
        #[cfg(target_os = "windows")]
        ctx.options_mut(|options| options.sync_window_theme = false);
        apply_theme(&ctx, config.theme_mode);

        for message in config_messages {
            status = StatusMessage::new(
                StatusTone::Warning,
                "Проверьте конфигурацию",
                message.trim_start_matches(">>>").trim(),
                None,
            );
            logs.push_str(&message);
            logs.push('\n');
        }

        Self::spawn_update_check(sender.clone(), ctx, app_dir.clone());

        Self {
            urls: vec![String::new()],
            config,
            logs,
            status,
            is_working: false,
            show_logs: false,
            show_url_editor: false,
            show_settings: false,
            show_about: false,
            show_update_confirm: false,
            center_confirm_window_on_open: false,
            profile_dialog: None,
            profile_dialog_text: String::new(),
            profile_dialog_needs_focus: false,
            receiver,
            sender,
            component_states: Vec::new(),
            selected_update_components: Vec::new(),
            updating_component: None,
            app_dir,
            config_path,
            native_title_bar_sync_delay: 2,
            last_native_title_bar_style: None,
        }
    }

    fn schedule_native_title_bar_sync(&mut self, ctx: &egui::Context) {
        self.native_title_bar_sync_delay = 1;
        ctx.request_repaint();
    }

    fn sync_native_title_bar(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let style = NativeTitleBarStyle::for_theme(
            self.config.theme_mode,
            ctx.theme() == egui::Theme::Dark,
        );

        if self.native_title_bar_sync_delay > 0 {
            self.native_title_bar_sync_delay -= 1;
            if self.native_title_bar_sync_delay == 0 {
                match apply_native_title_bar_style(frame, style) {
                    NativeTitleBarApplyResult::Applied => {
                        if style.use_system_theme {
                            ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(
                                egui::SystemTheme::SystemDefault,
                            ));
                        }
                        self.last_native_title_bar_style = Some(style);
                    }
                    NativeTitleBarApplyResult::UseSystemTheme => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(
                            egui::SystemTheme::SystemDefault,
                        ));
                        self.last_native_title_bar_style = Some(style);
                    }
                    NativeTitleBarApplyResult::Retry => {
                        self.native_title_bar_sync_delay = 1;
                    }
                }
            }
            if self.native_title_bar_sync_delay > 0 {
                ctx.request_repaint();
            }
        } else if self.last_native_title_bar_style != Some(style) {
            self.schedule_native_title_bar_sync(ctx);
        }
    }

    fn update_candidates(&self) -> Vec<updater::ComponentInfo> {
        self.component_states
            .iter()
            .filter(|component| {
                matches!(
                    component.status,
                    updater::ComponentStatus::Missing | updater::ComponentStatus::UpdateAvailable
                )
            })
            .cloned()
            .collect()
    }

    fn collect_update_targets(
        &self,
        selected: &[updater::ComponentKind],
    ) -> Vec<updater::ComponentInfo> {
        let candidates = self.update_candidates();
        selected_update_targets(&candidates, selected)
    }

    fn start_download(&mut self, ctx: &egui::Context) {
        if let Err(err) = fs::create_dir_all(&self.config.output_path) {
            self.set_local_error(
                "Не удалось создать папку загрузок",
                format!("{}: {err}", self.config.output_path),
            );
            return;
        }

        if let Err(err) = self.config.save(&self.config_path) {
            self.set_local_error(
                "Не удалось сохранить конфиг",
                format!("{}: {err}", self.config_path.display()),
            );
            return;
        }

        let yt_dlp_path = self.managed_yt_dlp_path();
        if !yt_dlp_path.is_file() {
            self.set_local_error(
                "yt-dlp.exe не найден",
                "Сначала установите его через Обновить",
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
            self.set_local_error("Список ссылок пуст", "Добавьте хотя бы одну ссылку");
            return;
        }
        self.is_working = true;
        self.logs.clear();
        let total = valid_urls.len();
        self.logs
            .push_str(&format!(">>> Старт: {} файл(ов)\n", total));
        self.status = StatusMessage::new(
            StatusTone::Running,
            "Подготовка загрузки",
            format!("В очереди: {} файл(ов)", total),
            Some((0, total)),
        );
        let path = self.config.output_path.clone();
        let yt_dlp_path = yt_dlp_path.clone();
        let active_profile = self.config.active_profile();
        let archive_path = self.app_dir.join(DOWNLOAD_ARCHIVE_FILE);
        let config_args = active_profile.to_yt_dlp_args(&archive_path);
        let output_template = active_profile.output_template(&path);
        let sender = self.sender.clone();
        let thread_ctx = ctx.clone();
        thread::spawn(move || {
            let mut had_error = false;
            let mut last_error = None;
            for (i, url) in valid_urls.iter().enumerate() {
                Self::send_status(
                    &sender,
                    &thread_ctx,
                    StatusMessage::new(
                        StatusTone::Running,
                        format!("Скачивание {} из {}", i + 1, total),
                        url.clone(),
                        Some((i + 1, total)),
                    ),
                );
                Self::send_log(
                    &sender,
                    &thread_ctx,
                    format!(">>> [{}/{}] {}", i + 1, total, url),
                );
                let mut args = vec!["--newline".to_string()];
                args.extend(config_args.iter().cloned());
                args.push("-o".to_string());
                args.push(output_template.clone());
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
                        let last_stderr_error = Arc::new(Mutex::new(None));
                        let stdout_handle = child_process.stdout.take().map(|stdout| {
                            Self::spawn_pipe_reader(
                                stdout,
                                sender.clone(),
                                thread_ctx.clone(),
                                "",
                                None,
                            )
                        });
                        let stderr_handle = child_process.stderr.take().map(|stderr| {
                            Self::spawn_pipe_reader(
                                stderr,
                                sender.clone(),
                                thread_ctx.clone(),
                                "stderr | ",
                                Some(last_stderr_error.clone()),
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
                                    let detail = last_stderr_error
                                        .lock()
                                        .ok()
                                        .and_then(|error| error.clone())
                                        .unwrap_or_else(|| {
                                            format!("yt-dlp завершился с кодом {exit_code}")
                                        });
                                    had_error = true;
                                    last_error = Some(detail.clone());
                                    Self::send_log(
                                        &sender,
                                        &thread_ctx,
                                        format!("❌ yt-dlp завершился с кодом {exit_code}"),
                                    );
                                    Self::send_status(
                                        &sender,
                                        &thread_ctx,
                                        StatusMessage::new(
                                            StatusTone::Error,
                                            "Ошибка загрузки",
                                            detail,
                                            Some((i + 1, total)),
                                        ),
                                    );
                                } else {
                                    Self::send_status(
                                        &sender,
                                        &thread_ctx,
                                        StatusMessage::new(
                                            StatusTone::Success,
                                            "Ссылка загружена",
                                            url.clone(),
                                            Some((i + 1, total)),
                                        ),
                                    );
                                }
                            }
                            Err(err) => {
                                let detail =
                                    format!("Не удалось дождаться завершения yt-dlp: {err}");
                                had_error = true;
                                last_error = Some(detail.clone());
                                Self::send_log(&sender, &thread_ctx, format!("❌ {detail}"));
                                Self::send_status(
                                    &sender,
                                    &thread_ctx,
                                    StatusMessage::new(
                                        StatusTone::Error,
                                        "Ошибка загрузки",
                                        detail,
                                        Some((i + 1, total)),
                                    ),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        let detail = format!("Ошибка запуска yt-dlp: {e}");
                        had_error = true;
                        last_error = Some(detail.clone());
                        Self::send_log(&sender, &thread_ctx, format!("❌ {detail}"));
                        Self::send_status(
                            &sender,
                            &thread_ctx,
                            StatusMessage::new(
                                StatusTone::Error,
                                "Ошибка загрузки",
                                detail,
                                Some((i + 1, total)),
                            ),
                        );
                    }
                }
            }
            let finish = if had_error {
                FinishState {
                    had_error: true,
                    restart_required: false,
                    title: "Загрузка завершена с ошибкой".to_string(),
                    detail: last_error.unwrap_or_else(|| "Проверьте полный лог".to_string()),
                }
            } else {
                FinishState {
                    had_error: false,
                    restart_required: false,
                    title: "Загрузка завершена".to_string(),
                    detail: format!("Скачано: {} файл(ов)", total),
                }
            };
            let _ = sender.send(AppMessage::AllFinished(finish));
            thread_ctx.request_repaint();
        });
    }

    fn start_update(&mut self, ctx: &egui::Context, to_update: Vec<updater::ComponentInfo>) {
        if to_update.is_empty() {
            self.logs.push_str(">>> Обновления не требуются.\n");
            self.status = StatusMessage::new(
                StatusTone::Success,
                "Обновления не требуются",
                "Все компоненты уже актуальны",
                None,
            );
            return;
        }
        self.is_working = true;
        let total = to_update.len();
        self.status = StatusMessage::new(
            StatusTone::Running,
            "Обновление компонентов",
            format!("В очереди: {total}"),
            Some((0, total)),
        );
        let sender = self.sender.clone();
        let thread_ctx = ctx.clone();
        let app_dir = self.app_dir.clone();
        thread::spawn(move || {
            let mut had_error = false;
            let mut restart_required = false;
            let mut last_error = None;
            for (index, component) in to_update.iter().enumerate() {
                let _ = sender.send(AppMessage::UpdatingComponent(Some(component.kind)));
                Self::send_status(
                    &sender,
                    &thread_ctx,
                    StatusMessage::new(
                        StatusTone::Running,
                        format!("Обновление {}", component.title),
                        "Загрузка и установка",
                        Some((index + 1, total)),
                    ),
                );
                match updater::install_component(&app_dir, component) {
                    Ok(result) => {
                        let (msg, component_restart_required) = match result {
                            updater::InstallResult::Installed(msg) => (msg, false),
                            updater::InstallResult::RestartRequired(msg) => (msg, true),
                        };
                        restart_required |= component_restart_required;
                        let _ = sender.send(AppMessage::Log(format!("✅ {msg}")));
                        Self::send_status(
                            &sender,
                            &thread_ctx,
                            StatusMessage::new(
                                StatusTone::Success,
                                "Компонент обновлен",
                                msg,
                                Some((index + 1, total)),
                            ),
                        );
                    }
                    Err(err) => {
                        let detail = format!("{}: {}", component.title, err);
                        had_error = true;
                        last_error = Some(detail.clone());
                        let _ = sender.send(AppMessage::Log(format!("❌ {detail}")));
                        Self::send_status(
                            &sender,
                            &thread_ctx,
                            StatusMessage::new(
                                StatusTone::Error,
                                "Ошибка обновления",
                                detail,
                                Some((index + 1, total)),
                            ),
                        );
                    }
                }
                thread_ctx.request_repaint();
            }
            let _ = sender.send(AppMessage::UpdatingComponent(None));
            let _ = sender.send(AppMessage::Log("✅ Обновление завершено.".to_string()));
            if !restart_required {
                let report = updater::check_for_updates(&app_dir);
                for warning in report.warnings {
                    Self::send_log(&sender, &thread_ctx, warning);
                }
                let _ = sender.send(AppMessage::UpdateSnapshot(report.components));
            }
            let finish = if had_error {
                FinishState {
                    had_error: true,
                    restart_required,
                    title: "Обновление завершено с ошибкой".to_string(),
                    detail: last_error.unwrap_or_else(|| "Проверьте полный лог".to_string()),
                }
            } else {
                FinishState {
                    had_error: false,
                    restart_required,
                    title: "Обновление завершено".to_string(),
                    detail: if restart_required {
                        "Приложение будет перезапущено".to_string()
                    } else {
                        "Компоненты готовы к работе".to_string()
                    },
                }
            };
            let _ = sender.send(AppMessage::AllFinished(finish));
            thread_ctx.request_repaint();
        });
    }

    fn component_badge(
        &self,
        ui: &egui::Ui,
        component: &updater::ComponentInfo,
    ) -> (egui::Color32, String) {
        let visuals = ui.visuals();
        let is_updating = self.updating_component.is_some_and(|kind| {
            kind == component.kind
                || kind == updater::ComponentKind::FfmpegBundle
                    && matches!(
                        component.kind,
                        updater::ComponentKind::Ffmpeg | updater::ComponentKind::Ffprobe
                    )
        });
        if self.is_working && is_updating {
            return (visuals.warn_fg_color, "обновляется".to_string());
        }
        match component.status {
            updater::ComponentStatus::Missing => {
                (visuals.error_fg_color, "не установлен".to_string())
            }
            updater::ComponentStatus::UpdateAvailable => {
                (visuals.warn_fg_color, "доступно обновление".to_string())
            }
            updater::ComponentStatus::UpToDate => {
                (UiTheme::for_ui(ui).success, "актуален".to_string())
            }
            updater::ComponentStatus::Unknown => {
                (UiTheme::for_ui(ui).secondary_text, "неизвестно".to_string())
            }
        }
    }

    fn draw_status_dot(ui: &mut egui::Ui, color: egui::Color32) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.0, color);
    }

    fn draw_component_skeleton_row(ui: &mut egui::Ui, row: usize) {
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::hover());
        let painter = ui.painter();
        let palette = UiTheme::for_ui(ui);
        let time = ui.input(|input| input.time);
        let y = rect.center().y;
        let title_width = [62.0, 74.0, 70.0][row.min(2)];
        let title_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 18.0, y - 4.0),
            egui::vec2(title_width, 8.0),
        );
        let version_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 96.0, y - 3.0),
            egui::vec2(38.0, 6.0),
        );

        painter.circle_filled(egui::pos2(rect.left() + 5.0, y), 3.0, palette.skeleton_dot);

        Self::paint_skeleton_bar(painter, title_rect, time, row as f64 * 0.12, palette);
        Self::paint_skeleton_bar(
            painter,
            version_rect,
            time,
            row as f64 * 0.12 + 0.08,
            palette,
        );
    }

    fn paint_skeleton_bar(
        painter: &egui::Painter,
        rect: egui::Rect,
        time: f64,
        phase_offset: f64,
        palette: UiPalette,
    ) {
        let corner_radius = egui::CornerRadius::same(2);
        let shimmer_width = rect.width() * 0.42;
        let cycle = (time * 1.35 + phase_offset).rem_euclid(1.0) as f32;
        let shimmer_x = rect.left() - shimmer_width + cycle * (rect.width() + shimmer_width * 2.0);

        painter.rect_filled(rect, corner_radius, palette.skeleton_base);

        let shimmer_rect = egui::Rect::from_min_size(
            egui::pos2(shimmer_x, rect.top()),
            egui::vec2(shimmer_width, rect.height()),
        );
        painter.with_clip_rect(rect).rect_filled(
            shimmer_rect,
            corner_radius,
            palette.skeleton_highlight,
        );
    }

    fn status_color(ui: &egui::Ui, tone: StatusTone) -> egui::Color32 {
        match tone {
            StatusTone::Idle => UiTheme::for_ui(ui).secondary_text,
            StatusTone::Running => egui::Color32::from_rgb(245, 184, 82),
            StatusTone::Success => UiTheme::for_ui(ui).success,
            StatusTone::Warning => ui.visuals().warn_fg_color,
            StatusTone::Error => ui.visuals().error_fg_color,
        }
    }

    fn apply_window_size(ctx: &egui::Context, size: [f32; 2]) {
        let [width, height] = size;
        let size = egui::vec2(width, height);
        ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::MaxInnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
    }

    fn apply_log_window_size(ctx: &egui::Context, show_logs: bool) {
        let size = if show_logs {
            LOG_WINDOW_SIZE
        } else {
            MAIN_WINDOW_SIZE
        };
        Self::apply_window_size(ctx, size);
    }

    fn draw_status_panel(&self, ui: &mut egui::Ui) {
        let color = Self::status_color(ui, self.status.tone);
        let palette = UiTheme::for_ui(ui);

        egui::Frame::new()
            .fill(palette.group_bg)
            .stroke(egui::Stroke::new(1.0, palette.stroke))
            .inner_margin(10.0)
            .corner_radius(4.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    if matches!(self.status.tone, StatusTone::Running) {
                        ui.spinner();
                    } else {
                        Self::draw_status_dot(ui, color);
                    }
                    ui.label(
                        egui::RichText::new(&self.status.title)
                            .strong()
                            .color(ui.visuals().strong_text_color()),
                    );
                    if let Some((current, total)) = self.status.progress {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(format!("{current} из {total}"))
                                    .small()
                                    .color(color),
                            );
                        });
                    }
                });

                if !self.status.detail.is_empty() {
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&self.status.detail)
                                .small()
                                .color(UiTheme::for_ui(ui).secondary_text),
                        )
                        .truncate(),
                    );
                }
            });
    }

    fn draw_button_with_icon(
        ui: &mut egui::Ui,
        text: &str,
        min_size: egui::Vec2,
        icon: impl FnOnce(&egui::Painter, egui::Rect, egui::Color32),
    ) -> egui::Response {
        let is_enabled = ui.is_enabled();
        let sense = if is_enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(min_size, sense);

        if ui.is_rect_visible(rect) {
            let visuals = ui.style().interact(&response);
            let painter = ui.painter();
            let corner_radius = egui::CornerRadius::same(4);
            let fg_color = if is_enabled {
                visuals.fg_stroke.color
            } else {
                UiTheme::for_ui(ui).secondary_text
            };

            painter.rect_filled(rect, corner_radius, visuals.bg_fill);
            painter.rect_stroke(
                rect,
                corner_radius,
                visuals.bg_stroke,
                egui::StrokeKind::Inside,
            );

            let icon_rect = egui::Rect::from_center_size(
                egui::pos2(rect.left() + 16.0, rect.center().y),
                egui::vec2(14.0, 14.0),
            );
            icon(painter, icon_rect, fg_color);

            painter.text(
                egui::pos2(icon_rect.right() + 8.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                text,
                egui::TextStyle::Button.resolve(ui.style()),
                fg_color,
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
            let corner_radius = egui::CornerRadius::same(4);

            painter.rect_filled(rect, corner_radius, visuals.bg_fill);
            painter.rect_stroke(
                rect,
                corner_radius,
                visuals.bg_stroke,
                egui::StrokeKind::Inside,
            );

            let icon_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(14.0, 14.0));
            icon(painter, icon_rect, visuals.fg_stroke.color);
        }

        response
    }

    fn draw_theme_selector(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let previous = self.config.theme_mode;
        let mut selected = previous;
        let popup_id = ui.make_persistent_id("theme_mode_selector");
        let paint_icon: fn(&egui::Painter, egui::Rect, egui::Color32) = if ui.visuals().dark_mode {
            Self::paint_moon_icon
        } else {
            Self::paint_sun_icon
        };
        let response = Self::draw_icon_only_button(ui, egui::vec2(30.0, 30.0), paint_icon)
            .on_hover_text(selected.tooltip());

        egui::Popup::from_response(&response)
            .id(popup_id)
            .open_memory(response.clicked().then_some(egui::SetOpenCommand::Toggle))
            .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
            .show(|ui| {
                ui.set_min_width(112.0);
                for (mode, label) in ThemeMode::OPTIONS {
                    if ui.selectable_label(selected == *mode, *label).clicked() {
                        selected = *mode;
                    }
                }
            });

        if selected != previous {
            self.config.theme_mode = selected;
            apply_theme(ctx, selected);
            self.schedule_native_title_bar_sync(ctx);
            self.save_config_to_disk();
        }
    }

    fn paint_back_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
        Self::svg_polyline(
            painter,
            rect,
            stroke,
            &[(15.0, 18.0), (9.0, 12.0), (15.0, 6.0)],
        );
    }

    fn paint_plus_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
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

    fn paint_refresh_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
        let mut path = Vec::with_capacity(35);
        let arc_steps = 24;

        for step in 0..=arc_steps {
            let angle = step as f32 / arc_steps as f32 * std::f32::consts::FRAC_PI_2 * 3.0;
            path.push((12.0 + 9.0 * angle.cos(), 12.0 + 9.0 * angle.sin()));
        }

        for step in 1..=8 {
            let t = step as f32 / 8.0;
            let mt = 1.0 - t;
            let x = mt * mt * mt * 12.0
                + 3.0 * mt * mt * t * 14.52
                + 3.0 * mt * t * t * 16.93
                + t * t * t * 18.74;
            let y = mt * mt * mt * 3.0
                + 3.0 * mt * mt * t * 3.0
                + 3.0 * mt * t * t * 4.0
                + t * t * t * 5.74;
            path.push((x, y));
        }

        path.push((21.0, 8.0));

        Self::svg_polyline(painter, rect, stroke, &path);
        Self::svg_polyline(
            painter,
            rect,
            stroke,
            &[(21.0, 3.0), (21.0, 8.0), (16.0, 8.0)],
        );
    }

    fn paint_log_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
        painter.rect_stroke(
            rect,
            egui::CornerRadius::same(2),
            stroke,
            egui::StrokeKind::Inside,
        );

        for offset in [4.0, 8.0, 12.0] {
            painter.line_segment(
                [
                    egui::pos2(rect.left() + 3.5, rect.top() + offset),
                    egui::pos2(rect.right() - 3.5, rect.top() + offset),
                ],
                stroke,
            );
        }
    }

    fn svg_pos(rect: egui::Rect, x: f32, y: f32) -> egui::Pos2 {
        egui::pos2(
            rect.left() + rect.width() * x / 24.0,
            rect.top() + rect.height() * y / 24.0,
        )
    }

    fn svg_line(
        painter: &egui::Painter,
        rect: egui::Rect,
        stroke: egui::Stroke,
        from: (f32, f32),
        to: (f32, f32),
    ) {
        painter.line_segment(
            [
                Self::svg_pos(rect, from.0, from.1),
                Self::svg_pos(rect, to.0, to.1),
            ],
            stroke,
        );
    }

    fn svg_polyline(
        painter: &egui::Painter,
        rect: egui::Rect,
        stroke: egui::Stroke,
        points: &[(f32, f32)],
    ) {
        for pair in points.windows(2) {
            Self::svg_line(painter, rect, stroke, pair[0], pair[1]);
        }
    }

    fn svg_circle(
        painter: &egui::Painter,
        rect: egui::Rect,
        center: (f32, f32),
        radius: f32,
        stroke: egui::Stroke,
    ) {
        let scale = rect.width().min(rect.height()) / 24.0;
        painter.circle_stroke(
            Self::svg_pos(rect, center.0, center.1),
            radius * scale,
            stroke,
        );
    }

    fn append_svg_arc(
        points: &mut Vec<(f32, f32)>,
        start: (f32, f32),
        end: (f32, f32),
        radius: f32,
        large_arc: bool,
        sweep: bool,
        steps: usize,
    ) {
        let dx = (start.0 - end.0) * 0.5;
        let dy = (start.1 - end.1) * 0.5;
        let distance = dx * dx + dy * dy;
        let sign = if large_arc == sweep { -1.0 } else { 1.0 };
        let factor = sign * ((radius * radius - distance).max(0.0) / distance).sqrt();
        let center = (
            factor * dy + (start.0 + end.0) * 0.5,
            factor * -dx + (start.1 + end.1) * 0.5,
        );
        let start_angle = (start.1 - center.1).atan2(start.0 - center.0);
        let mut delta = (end.1 - center.1).atan2(end.0 - center.0) - start_angle;

        if sweep && delta < 0.0 {
            delta += std::f32::consts::TAU;
        } else if !sweep && delta > 0.0 {
            delta -= std::f32::consts::TAU;
        }

        for step in 1..=steps {
            let angle = start_angle + delta * step as f32 / steps as f32;
            points.push((
                center.0 + radius * angle.cos(),
                center.1 + radius * angle.sin(),
            ));
        }
    }

    fn append_svg_cubic(
        points: &mut Vec<(f32, f32)>,
        start: (f32, f32),
        control_a: (f32, f32),
        control_b: (f32, f32),
        end: (f32, f32),
        steps: usize,
    ) {
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            let mt = 1.0 - t;
            points.push((
                mt.powi(3) * start.0
                    + 3.0 * mt.powi(2) * t * control_a.0
                    + 3.0 * mt * t.powi(2) * control_b.0
                    + t.powi(3) * end.0,
                mt.powi(3) * start.1
                    + 3.0 * mt.powi(2) * t * control_a.1
                    + 3.0 * mt * t.powi(2) * control_b.1
                    + t.powi(3) * end.1,
            ));
        }
    }

    fn paint_sun_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
        Self::svg_circle(painter, rect, (12.0, 12.0), 4.0, stroke);

        for (from, to) in [
            ((12.0, 2.0), (12.0, 4.0)),
            ((12.0, 20.0), (12.0, 22.0)),
            ((4.93, 4.93), (6.34, 6.34)),
            ((17.66, 17.66), (19.07, 19.07)),
            ((2.0, 12.0), (4.0, 12.0)),
            ((20.0, 12.0), (22.0, 12.0)),
            ((6.34, 17.66), (4.93, 19.07)),
            ((19.07, 4.93), (17.66, 6.34)),
        ] {
            Self::svg_line(painter, rect, stroke, from, to);
        }
    }

    fn paint_moon_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
        let start = (20.985, 12.486);
        let outer_end = (11.512, 3.014);
        let inner_start = (11.914, 3.817);
        let inner_end = (20.182, 12.085);
        let mut points = vec![start];

        Self::append_svg_arc(&mut points, start, outer_end, 9.0, true, true, 36);
        Self::append_svg_cubic(
            &mut points,
            outer_end,
            (11.917, 2.992),
            (12.129, 3.474),
            inner_start,
            6,
        );
        Self::append_svg_arc(&mut points, inner_start, inner_end, 6.0, false, false, 20);
        Self::append_svg_cubic(
            &mut points,
            inner_end,
            (20.526, 11.87),
            (21.007, 12.081),
            start,
            6,
        );
        Self::svg_polyline(painter, rect, stroke, &points);
    }

    fn paint_folder_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
        Self::svg_polyline(
            painter,
            rect,
            stroke,
            &[
                (2.0, 11.5),
                (2.0, 5.0),
                (4.0, 3.0),
                (7.9, 3.0),
                (9.6, 3.9),
                (10.4, 5.1),
                (12.1, 6.0),
                (20.0, 6.0),
                (22.0, 8.0),
                (22.0, 18.0),
                (20.0, 20.0),
                (10.5, 20.0),
            ],
        );
    }

    fn paint_folder_edit_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        Self::paint_folder_icon(painter, rect, color);
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
        Self::svg_polyline(
            painter,
            rect,
            stroke,
            &[
                (11.4, 13.6),
                (8.4, 10.6),
                (3.4, 15.6),
                (2.9, 16.5),
                (2.0, 19.4),
                (2.6, 20.0),
                (5.5, 19.1),
                (6.4, 18.6),
                (11.4, 13.6),
            ],
        );
        Self::svg_line(painter, rect, stroke, (8.4, 10.6), (11.4, 13.6));
    }

    fn paint_open_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
        Self::svg_polyline(
            painter,
            rect,
            stroke,
            &[
                (6.0, 14.0),
                (7.5, 11.1),
                (9.2, 10.0),
                (20.0, 10.0),
                (21.9, 12.5),
                (20.4, 18.5),
                (18.5, 20.0),
                (4.0, 20.0),
                (2.0, 18.0),
                (2.0, 5.0),
                (4.0, 3.0),
                (7.9, 3.0),
                (9.6, 3.9),
                (10.4, 5.1),
                (12.1, 6.0),
                (18.0, 6.0),
                (20.0, 8.0),
                (20.0, 10.0),
            ],
        );
    }

    fn paint_config_file_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
        Self::svg_polyline(
            painter,
            rect,
            stroke,
            &[
                (10.3, 20.0),
                (4.0, 20.0),
                (2.0, 18.0),
                (2.0, 5.0),
                (4.0, 3.0),
                (8.0, 3.0),
                (9.7, 3.9),
                (10.3, 5.1),
                (12.0, 6.0),
                (20.0, 6.0),
                (22.0, 8.0),
                (22.0, 11.3),
            ],
        );

        for (from, to) in [
            ((14.3, 19.5), (15.2, 19.1)),
            ((15.2, 16.9), (14.3, 16.5)),
            ((16.9, 15.2), (16.5, 14.3)),
            ((16.9, 20.8), (16.5, 21.7)),
            ((19.1, 15.2), (19.5, 14.3)),
            ((19.5, 21.7), (19.1, 20.8)),
            ((20.8, 16.9), (21.7, 16.5)),
            ((20.8, 19.1), (21.7, 19.5)),
        ] {
            Self::svg_line(painter, rect, stroke, from, to);
        }
        Self::svg_circle(painter, rect, (18.0, 18.0), 3.0, stroke);
    }

    fn paint_settings_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
        Self::svg_polyline(
            painter,
            rect,
            stroke,
            &[
                (9.7, 4.1),
                (11.0, 2.1),
                (13.0, 2.1),
                (14.3, 4.1),
                (14.9, 5.8),
                (16.8, 6.0),
                (19.0, 5.3),
                (20.3, 6.8),
                (20.7, 8.7),
                (19.6, 10.1),
                (18.6, 12.0),
                (19.6, 13.9),
                (20.7, 15.3),
                (20.3, 17.2),
                (19.0, 18.7),
                (16.8, 18.0),
                (14.9, 18.2),
                (14.3, 19.9),
                (13.0, 21.9),
                (11.0, 21.9),
                (9.7, 19.9),
                (9.1, 18.2),
                (7.2, 18.0),
                (5.0, 18.7),
                (3.7, 17.2),
                (3.3, 15.3),
                (4.4, 13.9),
                (5.4, 12.0),
                (4.4, 10.1),
                (3.3, 8.7),
                (3.7, 6.8),
                (5.0, 5.3),
                (7.2, 6.0),
                (9.1, 5.8),
                (9.7, 4.1),
            ],
        );
        Self::svg_circle(painter, rect, (12.0, 12.0), 3.0, stroke);
    }

    fn paint_info_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
        Self::svg_circle(painter, rect, (12.0, 12.0), 9.0, stroke);
        Self::svg_line(painter, rect, stroke, (12.0, 11.0), (12.0, 17.0));
        painter.circle_filled(
            Self::svg_pos(rect, 12.0, 7.0),
            ICON_STROKE_WIDTH * 0.75,
            color,
        );
    }

    fn paint_trash_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);

        let body = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 2.5, rect.top() + 4.5),
            egui::pos2(rect.right() - 2.5, rect.bottom() - 1.0),
        );
        painter.rect_stroke(
            body,
            egui::CornerRadius::same(2),
            stroke,
            egui::StrokeKind::Inside,
        );

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
        let stroke = egui::Stroke::new(ICON_STROKE_WIDTH, color);
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

    fn draw_labeled_combo_value<T: Copy + PartialEq>(
        ui: &mut egui::Ui,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
        label: &str,
        value: &mut T,
        options: &'static [(T, &'static str)],
    ) -> bool {
        let mut changed = false;
        let selected = options
            .iter()
            .find_map(|(candidate, name)| (*candidate == *value).then_some(*name))
            .unwrap_or("?");

        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(label)
                    .small()
                    .color(UiTheme::for_ui(ui).secondary_text),
            );
            egui::ComboBox::from_id_salt(id_salt)
                .selected_text(selected)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for (option, name) in options {
                        changed |= ui.selectable_value(value, *option, *name).changed();
                    }
                });
        });
        changed
    }

    fn draw_settings_section(
        ui: &mut egui::Ui,
        title: &str,
        add_contents: impl FnOnce(&mut egui::Ui),
    ) {
        let palette = UiTheme::for_ui(ui);
        egui::Frame::new()
            .fill(palette.group_bg)
            .stroke(egui::Stroke::new(1.0, palette.stroke))
            .inner_margin(12.0)
            .corner_radius(4.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    egui::RichText::new(title)
                        .strong()
                        .color(UiTheme::for_ui(ui).secondary_text),
                );
                ui.add_space(8.0);
                add_contents(ui);
            });
    }

    fn draw_profile_selector_combo(
        &mut self,
        ui: &mut egui::Ui,
        id_salt: &'static str,
        width: f32,
    ) -> bool {
        let mut active_id = self.config.active_profile_id.clone();
        let selected_name = self.config.active_profile_name();

        egui::ComboBox::from_id_salt(id_salt)
            .selected_text(selected_name)
            .width(width)
            .show_ui(ui, |ui| {
                for profile in builtin_profiles() {
                    ui.selectable_value(&mut active_id, profile.id, profile.name);
                }
                if !self.config.custom_profiles.is_empty() {
                    ui.separator();
                }
                for profile in &self.config.custom_profiles {
                    ui.selectable_value(&mut active_id, profile.id.clone(), profile.name.clone());
                }
            });

        if active_id != self.config.active_profile_id {
            self.config.active_profile_id = active_id;
            self.close_profile_dialog();
            return true;
        }

        false
    }

    fn draw_profile_selector_button(
        &mut self,
        ui: &mut egui::Ui,
        id_salt: &'static str,
        width: f32,
    ) -> bool {
        let popup_id = ui.make_persistent_id(id_salt);
        let selected_name = self.config.active_profile_name();
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, 24.0), egui::Sense::click());

        if ui.is_rect_visible(rect) {
            let visuals = ui.style().interact(&response);
            let painter = ui.painter();
            let corner_radius = egui::CornerRadius::same(4);

            painter.rect_filled(rect, corner_radius, visuals.bg_fill);
            painter.rect_stroke(
                rect,
                corner_radius,
                visuals.bg_stroke,
                egui::StrokeKind::Inside,
            );

            let text_rect = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 8.0, rect.top()),
                egui::pos2(rect.right() - 28.0, rect.bottom()),
            );
            let text_painter = painter.with_clip_rect(text_rect);
            text_painter.text(
                egui::pos2(text_rect.left(), rect.center().y),
                egui::Align2::LEFT_CENTER,
                selected_name,
                egui::TextStyle::Button.resolve(ui.style()),
                visuals.text_color(),
            );

            let icon_center = egui::pos2(rect.right() - 13.0, rect.center().y + 1.0);
            let stroke = egui::Stroke::new(1.6, visuals.fg_stroke.color);
            painter.line_segment(
                [
                    icon_center + egui::vec2(-4.0, -2.0),
                    icon_center + egui::vec2(0.0, 3.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    icon_center + egui::vec2(4.0, -2.0),
                    icon_center + egui::vec2(0.0, 3.0),
                ],
                stroke,
            );
        }

        let mut active_id = self.config.active_profile_id.clone();
        egui::Popup::from_response(&response)
            .id(popup_id)
            .open_memory(response.clicked().then_some(egui::SetOpenCommand::Toggle))
            .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
            .show(|ui| {
                ui.set_min_width(width);
                for profile in builtin_profiles() {
                    let is_selected = active_id == profile.id;
                    if ui.selectable_label(is_selected, profile.name).clicked() {
                        active_id = profile.id;
                    }
                }
                if !self.config.custom_profiles.is_empty() {
                    ui.separator();
                }
                for profile in &self.config.custom_profiles {
                    let is_selected = active_id == profile.id;
                    if ui.selectable_label(is_selected, &profile.name).clicked() {
                        active_id = profile.id.clone();
                    }
                }
            });

        if active_id != self.config.active_profile_id {
            self.config.active_profile_id = active_id;
            self.close_profile_dialog();
            return true;
        }

        false
    }

    fn close_profile_dialog(&mut self) {
        self.profile_dialog = None;
        self.profile_dialog_text.clear();
        self.profile_dialog_needs_focus = false;
    }

    fn open_profile_name_dialog(&mut self, dialog: ProfileDialog, name: impl Into<String>) {
        self.profile_dialog = Some(dialog);
        self.profile_dialog_text = name.into();
        self.profile_dialog_needs_focus = true;
    }

    fn normalized_profile_dialog_name(&self) -> String {
        match self.profile_dialog_text.trim() {
            "" => "Профиль".to_string(),
            name => name.to_string(),
        }
    }

    fn draw_settings_profile_selector(&mut self, ui: &mut egui::Ui, config_changed: &mut bool) {
        let width = ui.available_width().min(220.0);
        if self.draw_profile_selector_button(ui, "settings_download_profile_selector", width) {
            *config_changed = true;
        }
    }

    fn draw_profile_actions(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let small_button_size = egui::vec2(96.0, 26.0);
            let medium_button_size = egui::vec2(112.0, 26.0);
            let rename_button_size = egui::vec2(126.0, 26.0);
            if ui
                .add(egui::Button::new("Создать").min_size(small_button_size))
                .clicked()
            {
                self.open_profile_name_dialog(ProfileDialog::Create, "Новый профиль");
            }

            if ui
                .add(egui::Button::new("Дублировать").min_size(medium_button_size))
                .clicked()
            {
                let profile = self.config.active_profile();
                let source_name = profile.name.trim();
                let copy_name = if source_name.is_empty() {
                    "Профиль копия".to_string()
                } else {
                    format!("{source_name} копия")
                };
                self.open_profile_name_dialog(ProfileDialog::Duplicate(profile), copy_name);
            }

            let can_edit_custom = self.config.active_custom_profile_index().is_some();
            if ui
                .add_enabled(
                    can_edit_custom,
                    egui::Button::new("Переименовать").min_size(rename_button_size),
                )
                .clicked()
            {
                if let Some(index) = self.config.active_custom_profile_index() {
                    let profile = &self.config.custom_profiles[index];
                    self.open_profile_name_dialog(
                        ProfileDialog::Rename {
                            id: profile.id.clone(),
                        },
                        profile.name.clone(),
                    );
                }
            }

            if ui
                .add_enabled(
                    can_edit_custom,
                    egui::Button::new("Удалить").min_size(small_button_size),
                )
                .clicked()
            {
                if let Some(index) = self.config.active_custom_profile_index() {
                    let profile = &self.config.custom_profiles[index];
                    self.profile_dialog = Some(ProfileDialog::Delete {
                        id: profile.id.clone(),
                        name: profile.name.clone(),
                    });
                    self.profile_dialog_text.clear();
                    self.profile_dialog_needs_focus = false;
                }
            }
        });
    }

    fn apply_profile_name_dialog(&mut self, dialog: ProfileDialog) -> bool {
        let name = self.normalized_profile_dialog_name();
        match dialog {
            ProfileDialog::Create => {
                let id = self.config.next_custom_profile_id();
                self.config
                    .custom_profiles
                    .push(DownloadProfile::custom_default(id.clone(), name));
                self.config.active_profile_id = id;
                self.close_profile_dialog();
                true
            }
            ProfileDialog::Duplicate(mut profile) => {
                let id = self.config.next_custom_profile_id();
                profile.id = id.clone();
                profile.name = name;
                self.config.custom_profiles.push(profile);
                self.config.active_profile_id = id;
                self.close_profile_dialog();
                true
            }
            ProfileDialog::Rename { id } => {
                let mut changed = false;
                if let Some(profile) = self
                    .config
                    .custom_profiles
                    .iter_mut()
                    .find(|profile| profile.id == id)
                {
                    profile.name = name;
                    changed = true;
                }
                self.close_profile_dialog();
                changed
            }
            ProfileDialog::Delete { .. } => false,
        }
    }

    fn apply_profile_delete_dialog(&mut self, id: String) -> bool {
        let before_len = self.config.custom_profiles.len();
        self.config
            .custom_profiles
            .retain(|profile| profile.id != id);
        let changed = self.config.custom_profiles.len() != before_len;
        if changed && self.config.active_profile_id == id {
            self.config.active_profile_id = RAW_PROFILE_ID.to_string();
        }
        self.close_profile_dialog();
        changed
    }

    fn draw_profile_dialog(&mut self, ctx: &egui::Context) -> bool {
        let Some(dialog) = self.profile_dialog.clone() else {
            return false;
        };

        let mut changed = false;
        let mut apply_name = false;
        let mut approve_delete = false;
        let mut cancel = false;
        let title = match &dialog {
            ProfileDialog::Create => "Новый профиль",
            ProfileDialog::Duplicate(_) => "Дублировать профиль",
            ProfileDialog::Rename { .. } => "Переименовать профиль",
            ProfileDialog::Delete { .. } => "Удалить профиль",
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .default_width(360.0)
            .show(ctx, |ui| {
                ui.set_width(340.0);
                match &dialog {
                    ProfileDialog::Create
                    | ProfileDialog::Duplicate(_)
                    | ProfileDialog::Rename { .. } => {
                        ui.label("Имя профиля");
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.profile_dialog_text)
                                .desired_width(f32::INFINITY),
                        );
                        if self.profile_dialog_needs_focus {
                            response.request_focus();
                            self.profile_dialog_needs_focus = false;
                        }
                        if response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter))
                        {
                            apply_name = true;
                        }
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            let button_width = 96.0;
                            ui.add_space(
                                (ui.available_width() - (button_width * 2.0 + 8.0)).max(0.0),
                            );
                            if ui
                                .add(
                                    egui::Button::new("Отмена")
                                        .min_size(egui::vec2(button_width, 30.0)),
                                )
                                .clicked()
                            {
                                cancel = true;
                            }
                            if ui
                                .add(
                                    egui::Button::new("OK")
                                        .min_size(egui::vec2(button_width, 30.0)),
                                )
                                .clicked()
                            {
                                apply_name = true;
                            }
                        });
                    }
                    ProfileDialog::Delete { name, .. } => {
                        ui.label(format!("Удалить профиль \"{name}\"?"));
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new("Это действие нельзя отменить")
                                .small()
                                .color(UiTheme::for_ui(ui).secondary_text),
                        );
                        ui.add_space(16.0);
                        ui.horizontal(|ui| {
                            let button_width = 112.0;
                            ui.add_space(
                                (ui.available_width() - (button_width * 2.0 + 8.0)).max(0.0),
                            );
                            if ui
                                .add(
                                    egui::Button::new("Отмена")
                                        .min_size(egui::vec2(button_width, 30.0)),
                                )
                                .clicked()
                            {
                                cancel = true;
                            }
                            if ui
                                .add(
                                    egui::Button::new("Удалить")
                                        .min_size(egui::vec2(button_width, 30.0)),
                                )
                                .clicked()
                            {
                                approve_delete = true;
                            }
                        });
                    }
                }
            });

        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }

        if apply_name {
            changed = self.apply_profile_name_dialog(dialog);
        } else if approve_delete {
            if let ProfileDialog::Delete { id, .. } = dialog {
                changed = self.apply_profile_delete_dialog(id);
            }
        } else if cancel {
            self.close_profile_dialog();
        }

        changed
    }

    fn draw_active_profile_options(&mut self, ui: &mut egui::Ui, config_changed: &mut bool) {
        if let Some(index) = self.config.active_custom_profile_index() {
            if Self::draw_profile_controls(ui, &mut self.config.custom_profiles[index]) {
                *config_changed = true;
            }
        } else {
            let mut preview = self.config.active_profile();
            ui.add_enabled_ui(false, |ui| {
                let _ = Self::draw_profile_controls(ui, &mut preview);
            });
        }
    }

    fn set_sponsorblock_category(profile: &mut DownloadProfile, category: &str, enabled: bool) {
        let exists = profile
            .sponsorblock_categories
            .iter()
            .any(|current| current == category);
        if enabled && !exists {
            profile.sponsorblock_categories.push(category.to_string());
        } else if !enabled && exists {
            profile
                .sponsorblock_categories
                .retain(|current| current != category);
        }
    }

    fn draw_sponsorblock_categories(ui: &mut egui::Ui, profile: &mut DownloadProfile) -> bool {
        let mut changed = false;
        egui::Grid::new("sponsorblock_categories_grid")
            .num_columns(2)
            .spacing([18.0, 6.0])
            .show(ui, |ui| {
                for (index, (category, label)) in sponsorblock_category_options().iter().enumerate()
                {
                    let mut enabled = profile
                        .sponsorblock_categories
                        .iter()
                        .any(|current| current == *category);
                    if ui.checkbox(&mut enabled, *label).changed() {
                        Self::set_sponsorblock_category(profile, category, enabled);
                        changed = true;
                    }
                    if (index + 1) % 2 == 0 {
                        ui.end_row();
                    }
                }
            });
        changed
    }

    fn draw_profile_controls(ui: &mut egui::Ui, profile: &mut DownloadProfile) -> bool {
        let mut changed = false;

        ui.label(
            egui::RichText::new("Формат")
                .strong()
                .color(UiTheme::for_ui(ui).secondary_text),
        );
        ui.add_space(4.0);
        ui.columns(3, |columns| {
            changed |= Self::draw_labeled_combo_value(
                &mut columns[0],
                "profile_kind",
                "Тип",
                &mut profile.kind,
                DownloadKind::OPTIONS,
            );
            match profile.kind {
                DownloadKind::Video => {
                    changed |= Self::draw_labeled_combo_value(
                        &mut columns[1],
                        "profile_video_resolution",
                        "Разрешение",
                        &mut profile.video_resolution,
                        VideoResolution::OPTIONS,
                    );
                    changed |= Self::draw_labeled_combo_value(
                        &mut columns[2],
                        "profile_container",
                        "Контейнер",
                        &mut profile.container,
                        ContainerFormat::OPTIONS,
                    );
                }
                DownloadKind::AudioOnly => {
                    changed |= Self::draw_labeled_combo_value(
                        &mut columns[1],
                        "profile_audio_format",
                        "Аудио",
                        &mut profile.audio_format,
                        AudioFormat::OPTIONS,
                    );
                    changed |= Self::draw_labeled_combo_value(
                        &mut columns[2],
                        "profile_file_name_template",
                        "Имя файла",
                        &mut profile.file_name_template,
                        FileNameTemplate::OPTIONS,
                    );
                }
            }
        });

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new("SponsorBlock")
                .strong()
                .color(UiTheme::for_ui(ui).secondary_text),
        );
        ui.add_space(4.0);
        ui.columns(2, |columns| {
            changed |= Self::draw_labeled_combo_value(
                &mut columns[0],
                "profile_sponsorblock",
                "Режим",
                &mut profile.sponsorblock,
                SponsorBlockMode::OPTIONS,
            );
        });
        ui.add_space(6.0);
        ui.add_enabled_ui(profile.sponsorblock != SponsorBlockMode::Off, |ui| {
            changed |= Self::draw_sponsorblock_categories(ui, profile);
        });

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new("Субтитры и плейлисты")
                .strong()
                .color(UiTheme::for_ui(ui).secondary_text),
        );
        ui.add_space(4.0);
        ui.columns(3, |columns| {
            changed |= Self::draw_labeled_combo_value(
                &mut columns[0],
                "profile_subtitles",
                "Субтитры",
                &mut profile.subtitles,
                SubtitleMode::OPTIONS,
            );
            columns[1].add_enabled_ui(profile.subtitles != SubtitleMode::Off, |ui| {
                ui.label(
                    egui::RichText::new("Языки")
                        .small()
                        .color(UiTheme::for_ui(ui).button_text),
                );
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut profile.subtitle_langs)
                            .desired_width(f32::INFINITY),
                    )
                    .changed();
            });
            changed |= Self::draw_labeled_combo_value(
                &mut columns[2],
                "profile_playlist_mode",
                "Плейлисты",
                &mut profile.playlist_mode,
                PlaylistMode::OPTIONS,
            );
        });

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new("Постобработка")
                .strong()
                .color(UiTheme::for_ui(ui).secondary_text),
        );
        ui.add_space(4.0);
        egui::Grid::new("postprocess_options_grid")
            .num_columns(2)
            .spacing([18.0, 6.0])
            .show(ui, |ui| {
                changed |= ui
                    .checkbox(&mut profile.embed_metadata, "Метаданные")
                    .changed();
                changed |= ui
                    .checkbox(&mut profile.embed_thumbnail, "Обложка")
                    .changed();
                ui.end_row();
                changed |= ui.checkbox(&mut profile.embed_chapters, "Главы").changed();
                changed |= ui
                    .checkbox(&mut profile.use_download_archive, "Архив загрузок")
                    .changed();
                ui.end_row();
            });

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new("Дополнительные args")
                .strong()
                .color(UiTheme::for_ui(ui).secondary_text),
        );
        ui.add_space(4.0);
        let palette = UiTheme::for_ui(ui);
        egui::Frame::new()
            .fill(palette.input_bg)
            .stroke(egui::Stroke::new(1.0, palette.stroke))
            .corner_radius(4.0)
            .inner_margin(6.0)
            .show(ui, |ui| {
                let mut extra_args = profile.extra_args.join("\n");
                let response = ui.add(
                    egui::TextEdit::multiline(&mut extra_args)
                        .frame(egui::Frame::NONE)
                        .desired_rows(5)
                        .desired_width(f32::INFINITY),
                );
                if response.changed() {
                    profile.extra_args = extra_args
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(ToString::to_string)
                        .collect();
                    changed = true;
                }
            });

        changed
    }

    fn draw_settings_panel(&mut self, ui: &mut egui::Ui) {
        let mut config_changed = false;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Self::draw_settings_section(ui, "Файлы", |ui| {
                    let palette = UiTheme::for_ui(ui);
                    ui.label(
                        egui::RichText::new("Файл конфигурации")
                            .small()
                            .color(UiTheme::for_ui(ui).secondary_text),
                    );
                    let config_input_width = (ui.available_width() - 54.0).max(220.0);
                    ui.horizontal(|ui| {
                        egui::Frame::new()
                            .fill(palette.input_bg)
                            .stroke(egui::Stroke::new(1.0, palette.stroke))
                            .corner_radius(4.0)
                            .inner_margin(4.0)
                            .show(ui, |ui| {
                                let mut config_path =
                                    self.config_path.to_string_lossy().to_string();
                                ui.add(
                                    egui::TextEdit::singleline(&mut config_path)
                                        .frame(egui::Frame::NONE)
                                        .interactive(false)
                                        .desired_width(config_input_width),
                                );
                            });

                        if Self::draw_icon_only_button(
                            ui,
                            egui::vec2(30.0, 30.0),
                            Self::paint_config_file_icon,
                        )
                        .on_hover_text("Открыть файл конфигурации")
                        .clicked()
                        {
                            self.open_config_file();
                        }
                    });

                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new("Путь сохранения")
                            .small()
                            .color(UiTheme::for_ui(ui).secondary_text),
                    );
                    let output_input_width = (ui.available_width() - 92.0).max(220.0);
                    ui.horizontal(|ui| {
                        egui::Frame::new()
                            .fill(palette.input_bg)
                            .stroke(egui::Stroke::new(1.0, palette.stroke))
                            .corner_radius(4.0)
                            .inner_margin(4.0)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.config.output_path)
                                        .frame(egui::Frame::NONE)
                                        .interactive(false)
                                        .desired_width(output_input_width),
                                );
                            });

                        if Self::draw_icon_only_button(
                            ui,
                            egui::vec2(30.0, 30.0),
                            Self::paint_open_icon,
                        )
                        .on_hover_text("Открыть папку сохранения")
                        .clicked()
                        {
                            self.open_output_path();
                        }
                        if Self::draw_icon_only_button(
                            ui,
                            egui::vec2(30.0, 30.0),
                            Self::paint_folder_edit_icon,
                        )
                        .on_hover_text("Изменить путь сохранения")
                        .clicked()
                        {
                            self.choose_output_path();
                        }
                    });
                });

                ui.add_space(10.0);

                Self::draw_settings_section(ui, "Профили загрузки", |ui| {
                    self.draw_settings_profile_selector(ui, &mut config_changed);
                    ui.add_space(8.0);
                    self.draw_profile_actions(ui);
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(12.0);
                    self.draw_active_profile_options(ui, &mut config_changed);
                });
            });

        if config_changed {
            self.save_config_to_disk();
        }
    }

    fn draw_settings_editor(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            if Self::draw_icon_only_button(ui, egui::vec2(28.0, 28.0), Self::paint_back_icon)
                .on_hover_text("Назад")
                .clicked()
            {
                self.show_settings = false;
                Self::apply_log_window_size(ctx, self.show_logs);
            }
            ui.add_space(8.0);
            ui.heading("Настройки");
        });

        ui.add_space(10.0);
        self.draw_settings_panel(ui);
    }

    fn draw_about_page(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            if Self::draw_icon_only_button(ui, egui::vec2(28.0, 28.0), Self::paint_back_icon)
                .on_hover_text("Назад")
                .clicked()
            {
                self.show_about = false;
                Self::apply_log_window_size(ctx, self.show_logs);
            }
            ui.add_space(8.0);
            ui.heading("О программе");
        });

        ui.add_space(10.0);
        let palette = UiTheme::for_ui(ui);
        egui::Frame::new()
            .fill(palette.group_bg)
            .stroke(egui::Stroke::new(1.0, palette.stroke))
            .inner_margin(12.0)
            .corner_radius(4.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.heading("YouTube Downloader");
                ui.label(format!("Версия {}", updater::APP_VERSION));
                ui.add_space(4.0);
                ui.hyperlink_to("github.com/amd64fox/ytdlp-ui", updater::APP_REPOSITORY_URL)
                    .on_hover_text("Открыть репозиторий GitHub");
            });

        ui.add_space(10.0);
        let update_candidates = self.update_candidates();
        let update_enabled =
            !self.is_working && !self.component_states.is_empty() && !update_candidates.is_empty();
        let update_tooltip = if self.component_states.is_empty() {
            "Проверка обновлений"
        } else if self.is_working {
            "Дождитесь завершения текущей операции"
        } else if update_candidates.is_empty() {
            if self
                .component_states
                .iter()
                .all(|component| component.status == updater::ComponentStatus::UpToDate)
            {
                "Все компоненты актуальны"
            } else {
                "Нет доступных обновлений"
            }
        } else {
            "Обновить компоненты"
        };
        let mut update_clicked = false;
        egui::Frame::new()
            .fill(palette.group_bg)
            .stroke(egui::Stroke::new(1.0, palette.stroke))
            .inner_margin(12.0)
            .corner_radius(4.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Компоненты")
                            .strong()
                            .color(UiTheme::for_ui(ui).secondary_text),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let response = ui
                            .add_enabled_ui(update_enabled, |ui| {
                                Self::draw_button_with_icon(
                                    ui,
                                    "Обновить",
                                    egui::vec2(112.0, 28.0),
                                    Self::paint_refresh_icon,
                                )
                            })
                            .inner
                            .on_hover_text(update_tooltip);
                        update_clicked = response.clicked();
                    });
                });
                ui.add_space(8.0);

                if self.component_states.is_empty() {
                    for row in 0..4 {
                        Self::draw_component_skeleton_row(ui, row);
                    }
                    return;
                }

                egui::Grid::new("about_component_versions")
                    .num_columns(4)
                    .spacing(egui::vec2(12.0, 8.0))
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong("Компонент");
                        ui.strong("Установлена");
                        ui.strong("Последняя");
                        ui.strong("Состояние");
                        ui.end_row();

                        for component in &self.component_states {
                            let (color, status) = self.component_badge(ui, component);
                            ui.label(egui::RichText::new(&component.title).strong());
                            ui.label(component.local_version.as_deref().unwrap_or("—"));
                            ui.label(component.latest_version.as_deref().unwrap_or("—"));
                            ui.colored_label(color, status);
                            ui.end_row();
                        }
                    });
            });

        if update_clicked {
            self.selected_update_components = update_candidates
                .iter()
                .map(|component| component.kind)
                .collect();
            self.show_update_confirm = true;
            self.center_confirm_window_on_open = true;
        }
    }

    fn draw_url_editor(&mut self, ui: &mut egui::Ui) {
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            if Self::draw_icon_only_button(ui, egui::vec2(28.0, 28.0), Self::paint_back_icon)
                .on_hover_text("Назад")
                .clicked()
            {
                self.show_url_editor = false;
            }
            ui.add_space(8.0);
            ui.heading("Редактор ссылок");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("{} ссылок", self.urls.len()))
                        .color(UiTheme::for_ui(ui).secondary_text),
                );
            });
        });

        ui.add_space(10.0);

        let palette = UiTheme::for_ui(ui);
        egui::Frame::new()
            .fill(palette.group_bg)
            .stroke(egui::Stroke::new(1.0, palette.stroke))
            .inner_margin(10.0)
            .corner_radius(4.0)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Добавьте одну или несколько ссылок")
                        .strong()
                        .color(UiTheme::for_ui(ui).secondary_text),
                );
                ui.add_space(8.0);

                egui::ScrollArea::vertical()
                    .id_salt("url_list_editor")
                    .auto_shrink([false, true])
                    .max_height(ui.available_height() - 70.0)
                    .show(ui, |ui| {
                        let mut remove_idx = None;

                        for (i, url) in self.urls.iter_mut().enumerate() {
                            egui::Frame::new()
                                .fill(palette.input_bg)
                                .stroke(egui::Stroke::new(1.0, palette.stroke))
                                .inner_margin(6.0)
                                .corner_radius(4.0)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!("{:02}.", i + 1))
                                                .color(UiTheme::for_ui(ui).secondary_text),
                                        );
                                        let width = (ui.available_width() - 34.0).max(100.0);
                                        let text_edit = ui.add(
                                            egui::TextEdit::singleline(url)
                                                .desired_width(width)
                                                .frame(egui::Frame::NONE)
                                                .hint_text("https://www.youtube.com/watch?v=...")
                                                .margin(egui::vec2(0.0, 0.0)),
                                        );
                                        text_edit.context_menu(|ui| {
                                            if ui.button("Вставить").clicked() {
                                                if let Ok(mut clipboard) = Clipboard::new() {
                                                    if let Ok(text) = clipboard.get_text() {
                                                        *url = text;
                                                    }
                                                }
                                                ui.close();
                                            }
                                            if ui.button("Очистить").clicked() {
                                                url.clear();
                                                ui.close();
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
    fn ui(&mut self, root_ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();
        self.sync_native_title_bar(&ctx, frame);

        while let Ok(msg) = self.receiver.try_recv() {
            match msg {
                AppMessage::Log(line) => {
                    self.logs.push_str(&line);
                    self.logs.push('\n');
                }
                AppMessage::Status(status) => {
                    self.status = status;
                }
                AppMessage::UpdateSnapshot(states) => {
                    self.component_states = states;
                }
                AppMessage::UpdatingComponent(current) => {
                    self.updating_component = current;
                }
                AppMessage::AllFinished(finish) => {
                    self.is_working = false;
                    let restart_required = finish.restart_required;
                    self.status = StatusMessage::new(
                        if finish.had_error {
                            StatusTone::Error
                        } else {
                            StatusTone::Success
                        },
                        finish.title,
                        finish.detail,
                        None,
                    );
                    self.logs.push_str(">>> Готово.\n");
                    if restart_required {
                        self.logs
                            .push_str(">>> Перезапуск для установки новой версии.\n");
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        }
        if self.show_logs && self.logs.is_empty() {
            self.show_logs = false;
            if !self.show_settings && !self.show_about {
                Self::apply_log_window_size(&ctx, false);
            }
        }
        if self.component_states.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
        egui::CentralPanel::default().show(root_ui, |ui| {
            if self.show_settings {
                self.draw_settings_editor(&ctx, ui);
                return;
            }

            if self.show_about {
                self.draw_about_page(&ctx, ui);
                return;
            }

            if self.show_url_editor {
                self.draw_url_editor(ui);
                return;
            }

            ui.add_space(5.0);

            ui.horizontal(|ui| {
                ui.heading("YouTube Downloader");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if Self::draw_icon_only_button(
                        ui,
                        egui::vec2(30.0, 30.0),
                        Self::paint_settings_icon,
                    )
                    .on_hover_text("Настройки")
                    .clicked()
                    {
                        self.show_settings = true;
                        Self::apply_window_size(&ctx, PAGE_WINDOW_SIZE);
                    }

                    ui.add_space(6.0);
                    if Self::draw_icon_only_button(
                        ui,
                        egui::vec2(30.0, 30.0),
                        Self::paint_info_icon,
                    )
                    .on_hover_text("О программе")
                    .clicked()
                    {
                        self.show_about = true;
                        Self::apply_window_size(&ctx, PAGE_WINDOW_SIZE);
                    }

                    ui.add_space(6.0);
                    self.draw_theme_selector(&ctx, ui);
                });
            });

            ui.add_space(10.0);

            let palette = UiTheme::for_ui(ui);
            egui::Frame::new()
                .fill(palette.group_bg)
                .stroke(egui::Stroke::new(1.0, palette.stroke))
                .inner_margin(8.0)
                .corner_radius(4.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Входящие ссылки")
                                .strong()
                                .color(UiTheme::for_ui(ui).secondary_text),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.draw_profile_selector_combo(
                                ui,
                                "main_download_profile_selector",
                                180.0,
                            ) {
                                self.save_config_to_disk();
                            }
                            ui.label(
                                egui::RichText::new("Профиль")
                                    .small()
                                    .color(UiTheme::for_ui(ui).secondary_text),
                            );
                        });
                    });
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

                    egui::Frame::new()
                        .fill(palette.input_bg)
                        .stroke(egui::Stroke::new(1.0, palette.stroke))
                        .corner_radius(4.0)
                        .inner_margin(4.0)
                        .show(ui, |ui| {
                            let url_edit = ui.add(
                                egui::TextEdit::singleline(&mut self.urls[0])
                                    .desired_width(f32::INFINITY)
                                    .frame(egui::Frame::NONE)
                                    .hint_text("https://www.youtube.com/watch?v=..."),
                            );
                            url_edit.context_menu(|ui| {
                                if ui.button("Вставить").clicked() {
                                    if let Ok(mut c) = Clipboard::new() {
                                        if let Ok(t) = c.get_text() {
                                            self.urls[0] = t;
                                        }
                                    }
                                    ui.close();
                                }
                                if ui.button("Очистить").clicked() {
                                    self.urls[0].clear();
                                    ui.close();
                                }
                            });
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
                        self.start_download(&ctx);
                    }

                    if !downloader_ready && !is_checking {
                        ui.label(
                            egui::RichText::new("Установите yt-dlp в разделе «О программе».")
                                .weak(),
                        );
                    }
                } else {
                    ui.add_enabled(
                        false,
                        egui::Button::new("СКАЧИВАЕТСЯ").min_size(egui::vec2(120.0, 36.0)),
                    );
                }

                if Self::draw_icon_only_button(ui, egui::vec2(36.0, 36.0), Self::paint_open_icon)
                    .on_hover_text("Открыть папку сохранения")
                    .clicked()
                {
                    self.open_output_path();
                }
            });

            ui.add_space(10.0);

            self.draw_status_panel(ui);

            ui.add_space(8.0);

            ui.horizontal(|ui| {
                let line_count = self.logs.lines().count();
                let has_logs = line_count > 0;
                let label = if self.show_logs {
                    "Скрыть лог"
                } else {
                    "Показать лог"
                };
                ui.add_enabled_ui(has_logs, |ui| {
                    if Self::draw_button_with_icon(
                        ui,
                        label,
                        egui::vec2(126.0, 30.0),
                        Self::paint_log_icon,
                    )
                    .clicked()
                    {
                        self.show_logs = !self.show_logs;
                        Self::apply_log_window_size(&ctx, self.show_logs);
                    }
                });

                let log_hint = if line_count == 0 {
                    "Лог пуст".to_string()
                } else {
                    format!("Строк в логе: {line_count}")
                };
                ui.label(
                    egui::RichText::new(log_hint)
                        .small()
                        .color(UiTheme::for_ui(ui).secondary_text),
                );
            });
            ui.add_space(8.0);

            if self.show_logs {
                egui::Frame::new()
                    .fill(palette.input_bg)
                    .stroke(egui::Stroke::new(1.0, palette.stroke))
                    .inner_margin(4.0)
                    .corner_radius(4.0)
                    .show(ui, |ui| {
                        ui.set_min_height(LOG_AREA_HEIGHT);
                        egui::ScrollArea::vertical()
                            .id_salt("download_log")
                            .stick_to_bottom(true)
                            .max_height(LOG_AREA_HEIGHT)
                            .show(ui, |ui| {
                                let log_id = ui.make_persistent_id("download_log_text");
                                let previous_cursor_range =
                                    egui::TextEdit::load_state(ui.ctx(), log_id)
                                        .and_then(|state| state.cursor.char_range());
                                let mut log_text = self.logs.as_str();
                                let response = ui.add_sized(
                                    [ui.available_width(), LOG_AREA_HEIGHT],
                                    egui::TextEdit::multiline(&mut log_text)
                                        .id(log_id)
                                        .font(egui::TextStyle::Body)
                                        .desired_width(f32::INFINITY)
                                        .frame(egui::Frame::NONE),
                                );

                                let preserve_selection = preserve_text_selection_for_context_menu(
                                    ui,
                                    &response,
                                    log_id,
                                    previous_cursor_range,
                                );
                                let cursor_range = if preserve_selection {
                                    previous_cursor_range
                                } else {
                                    egui::TextEdit::load_state(ui.ctx(), log_id)
                                        .and_then(|state| state.cursor.char_range())
                                };
                                let selected =
                                    selected_text(&self.logs, cursor_range).map(str::to_owned);

                                response.context_menu(|ui| {
                                    if ui
                                        .add_enabled(
                                            selected.is_some(),
                                            egui::Button::new("Копировать"),
                                        )
                                        .clicked()
                                    {
                                        if let Some(text) = &selected {
                                            ui.ctx().copy_text(text.clone());
                                        }
                                        ui.close();
                                    }
                                });
                            });
                    });
                ui.add_space(8.0);
            }
        });

        if self.draw_profile_dialog(&ctx) {
            self.save_config_to_disk();
        }

        if self.show_update_confirm {
            let viewport_id = Self::update_confirm_viewport_id();
            let mut approve = false;
            let mut close_confirm = false;
            let candidates = self.update_candidates();

            let confirm_width = 470.0;
            let confirm_height = 260.0;
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

            ctx.show_viewport_immediate(viewport_id, viewport_builder, |viewport_ui, _class| {
                let viewport_ctx = viewport_ui.ctx().clone();
                #[cfg(target_os = "windows")]
                viewport_ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(
                    egui::SystemTheme::SystemDefault,
                ));
                #[cfg(not(target_os = "windows"))]
                viewport_ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(
                    self.config.theme_mode.system_theme(),
                ));
                let palette = UiTheme::for_ctx(&viewport_ctx);
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(palette.background)
                            .inner_margin(12.0),
                    )
                    .show(viewport_ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label("Подтвердите обновление компонентов:");
                            ui.add_space(12.0);

                            egui::Frame::new()
                                .fill(palette.input_bg)
                                .stroke(egui::Stroke::new(1.0, palette.stroke))
                                .corner_radius(4.0)
                                .inner_margin(10.0)
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    egui::ScrollArea::vertical()
                                        .auto_shrink([false, true])
                                        .max_height(130.0)
                                        .show(ui, |ui| {
                                            for item in &candidates {
                                                let mut selected = self
                                                    .selected_update_components
                                                    .contains(&item.kind);
                                                ui.horizontal(|ui| {
                                                    if ui.checkbox(&mut selected, "").changed() {
                                                        if selected {
                                                            self.selected_update_components
                                                                .push(item.kind);
                                                        } else {
                                                            self.selected_update_components
                                                                .retain(|kind| *kind != item.kind);
                                                        }
                                                    }
                                                    ui.label(
                                                        egui::RichText::new(&item.title)
                                                            .monospace(),
                                                    );
                                                    ui.with_layout(
                                                        egui::Layout::right_to_left(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            ui.label(format!(
                                                                "{} → {}",
                                                                item.local_version
                                                                    .as_deref()
                                                                    .unwrap_or("не установлен"),
                                                                item.latest_version
                                                                    .as_deref()
                                                                    .unwrap_or("?")
                                                            ));
                                                        },
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
                                    .add(egui::Button::new("Отмена").min_size(egui::vec2(w, 30.0)))
                                    .clicked()
                                {
                                    close_confirm = true;
                                    viewport_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                                ui.add_space(10.0);
                                if ui
                                    .add_enabled(
                                        !self.selected_update_components.is_empty(),
                                        egui::Button::new("Обновить").min_size(egui::vec2(w, 30.0)),
                                    )
                                    .clicked()
                                {
                                    approve = true;
                                    close_confirm = true;
                                    viewport_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            });
                        });
                    });
                if viewport_ctx.input(|i| i.viewport().close_requested()) {
                    close_confirm = true;
                }
            });
            if approve {
                let selected = std::mem::take(&mut self.selected_update_components);
                let targets = self.collect_update_targets(&selected);
                self.start_update(&ctx, targets);
            } else if close_confirm {
                self.selected_update_components.clear();
            }
            if close_confirm {
                self.show_update_confirm = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_theme, builtin_profile_by_id, preserve_text_selection_for_context_menu,
        restore_text_selection, selected_text, selected_update_targets, AppConfig, AudioFormat,
        DownloadKind, DownloadProfile, FileNameTemplate, NativeTitleBarStyle, ThemeMode, UiTheme,
        AUDIO_MP3_PROFILE_ID, MP4_1080_PROFILE_ID, NATIVE_COLOR_DEFAULT, NO_SPONSORS_PROFILE_ID,
    };
    use super::{YtDlpApp, RAW_PROFILE_ID};
    use crate::egui;
    use std::path::Path;

    #[test]
    fn extracts_plain_yt_dlp_error() {
        assert_eq!(
            YtDlpApp::extract_yt_dlp_error_line("ERROR: video unavailable"),
            Some("video unavailable".to_string())
        );
    }

    #[test]
    fn extracts_prefixed_stderr_error() {
        assert_eq!(
            YtDlpApp::extract_yt_dlp_error_line("stderr | ERROR: private video"),
            Some("private video".to_string())
        );
    }

    #[test]
    fn ignores_regular_output() {
        assert_eq!(
            YtDlpApp::extract_yt_dlp_error_line("[download] 42.0% of 12.00MiB"),
            None
        );
    }

    #[test]
    fn default_config_uses_raw_profile_without_yt_dlp_args() {
        let cfg = AppConfig::default();

        assert_eq!(cfg.active_profile_id, RAW_PROFILE_ID);
        assert_eq!(cfg.theme_mode, ThemeMode::System);
        assert!(cfg
            .active_profile()
            .to_yt_dlp_args(Path::new("download-archive.txt"))
            .is_empty());
    }

    #[test]
    fn migrates_legacy_yt_dlp_args_to_custom_profile() {
        let mut cfg: AppConfig = toml::from_str(
            r#"
output_path = "Video"
yt_dlp_args = [
    "--sponsorblock-remove sponsor,selfpromo",
    "--merge-output-format mp4",
]
"#,
        )
        .unwrap();

        let (changed, _) = cfg.normalize_after_load(Path::new("."));
        let args = cfg
            .active_profile()
            .to_yt_dlp_args(Path::new("download-archive.txt"));
        let serialized = toml::to_string(&cfg).unwrap();

        assert!(changed);
        assert_eq!(cfg.theme_mode, ThemeMode::System);
        assert_eq!(cfg.custom_profiles.len(), 1);
        assert_eq!(cfg.custom_profiles[0].name, "Старые настройки");
        assert_eq!(
            args,
            vec![
                "--sponsorblock-remove",
                "sponsor,selfpromo",
                "--merge-output-format",
                "mp4"
            ]
        );
        assert!(!serialized.contains("yt_dlp_args"));
    }

    #[test]
    fn preset_mp4_1080_builds_expected_args() {
        let profile = builtin_profile_by_id(MP4_1080_PROFILE_ID).unwrap();

        assert_eq!(
            profile.to_yt_dlp_args(Path::new("download-archive.txt")),
            vec![
                "-S",
                "res:1080",
                "--merge-output-format",
                "mp4",
                "--remux-video",
                "mp4"
            ]
        );
    }

    #[test]
    fn preset_sponsorblock_remove_builds_expected_args() {
        let profile = builtin_profile_by_id(NO_SPONSORS_PROFILE_ID).unwrap();

        assert_eq!(
            profile.to_yt_dlp_args(Path::new("download-archive.txt")),
            vec![
                "-S",
                "res:1080",
                "--merge-output-format",
                "mp4",
                "--remux-video",
                "mp4",
                "--sponsorblock-remove",
                "sponsor,selfpromo"
            ]
        );
    }

    #[test]
    fn preset_audio_mp3_builds_expected_args() {
        let profile = builtin_profile_by_id(AUDIO_MP3_PROFILE_ID).unwrap();

        assert_eq!(
            profile.to_yt_dlp_args(Path::new("download-archive.txt")),
            vec!["-x", "--audio-format", "mp3"]
        );
    }

    #[test]
    fn video_profile_uses_title_output_template() {
        let profile = builtin_profile_by_id(MP4_1080_PROFILE_ID).unwrap();

        assert_eq!(
            profile.output_template(r"C:\Music\"),
            r"C:\Music/%(title)s.%(ext)s"
        );
    }

    #[test]
    fn audio_profile_uses_title_output_template_by_default() {
        let profile = builtin_profile_by_id(AUDIO_MP3_PROFILE_ID).unwrap();

        assert_eq!(
            profile.output_template(r"C:\Music\"),
            r"C:\Music/%(title)s.%(ext)s"
        );
    }

    #[test]
    fn audio_profile_can_use_artist_track_output_template() {
        let mut profile = builtin_profile_by_id(AUDIO_MP3_PROFILE_ID).unwrap();
        profile.file_name_template = FileNameTemplate::ArtistTrack;

        assert_eq!(
            profile.output_template(r"C:\Music\"),
            r"C:\Music/%(artist,uploader|Unknown Artist)s - %(track,title)s.%(ext)s"
        );
    }

    #[test]
    fn extra_args_are_preserved_as_single_arguments() {
        let mut profile = DownloadProfile::custom_default("custom-test", "Custom");
        profile.kind = DownloadKind::AudioOnly;
        profile.audio_format = AudioFormat::Best;
        profile.extra_args = vec![
            "--cookies-from-browser".to_string(),
            "firefox:default profile".to_string(),
            "  ".to_string(),
        ];

        assert_eq!(
            profile.to_yt_dlp_args(Path::new("download-archive.txt")),
            vec!["-x", "--cookies-from-browser", "firefox:default profile"]
        );
    }

    #[test]
    fn global_style_configures_dark_and_light_palettes() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::Theme::Light);

        super::configure_global_style(&ctx);

        let dark = ctx.style_of(egui::Theme::Dark);
        let light = ctx.style_of(egui::Theme::Light);

        assert_eq!(ctx.theme(), egui::Theme::Light);
        assert!(dark.visuals.dark_mode);
        assert!(!light.visuals.dark_mode);
        assert_eq!(dark.visuals.panel_fill, UiTheme::DARK.background);
        assert_eq!(light.visuals.panel_fill, UiTheme::LIGHT.background);
        assert_eq!(
            dark.visuals.widgets.noninteractive.weak_bg_fill,
            UiTheme::DARK.disabled_fade
        );
        assert_eq!(
            light.visuals.widgets.noninteractive.weak_bg_fill,
            UiTheme::LIGHT.disabled_fade
        );
        assert!(light.visuals.weak_text_color().r() <= 140);
        assert!(UiTheme::LIGHT.secondary_text.r() < light.visuals.text_color().r() + 40);
        assert_ne!(dark.visuals.panel_fill, light.visuals.panel_fill);
    }

    #[test]
    fn native_title_bar_styles_match_theme_modes() {
        let system_light = NativeTitleBarStyle::for_theme(ThemeMode::System, false);
        let system_dark = NativeTitleBarStyle::for_theme(ThemeMode::System, true);
        let forced_light = NativeTitleBarStyle::for_theme(ThemeMode::Light, true);
        let forced_dark = NativeTitleBarStyle::for_theme(ThemeMode::Dark, false);

        assert!(!system_light.dark_mode);
        assert!(system_light.use_system_theme);
        assert_eq!(system_light.caption_color, NATIVE_COLOR_DEFAULT);
        assert_eq!(system_light.text_color, NATIVE_COLOR_DEFAULT);
        assert!(system_dark.dark_mode);
        assert!(system_dark.use_system_theme);
        assert_eq!(system_dark.caption_color, NATIVE_COLOR_DEFAULT);
        assert_eq!(system_dark.text_color, NATIVE_COLOR_DEFAULT);
        assert!(!forced_light.dark_mode);
        assert!(!forced_light.use_system_theme);
        assert_eq!(
            forced_light.caption_color,
            super::colorref(UiTheme::LIGHT.background)
        );
        assert_eq!(
            forced_light.text_color,
            super::colorref(UiTheme::LIGHT.button_text)
        );
        assert!(forced_dark.dark_mode);
        assert!(!forced_dark.use_system_theme);
        assert_eq!(
            forced_dark.caption_color,
            super::colorref(UiTheme::DARK.background)
        );
        assert_eq!(
            forced_dark.text_color,
            super::colorref(UiTheme::DARK.button_text)
        );
    }

    #[test]
    fn colorref_uses_windows_channel_order() {
        assert_eq!(
            super::colorref(egui::Color32::from_rgb(0x12, 0x34, 0x56)),
            0x0056_3412
        );
    }

    #[test]
    fn selected_text_uses_character_indices() {
        use egui::text::{CCursor, CCursorRange};

        let range = CCursorRange::two(CCursor::new(4), CCursor::new(5));

        assert_eq!(selected_text("лог α", Some(range)), Some("α"));
        assert_eq!(selected_text("лог α", None), None);
        assert_eq!(
            selected_text("лог α", Some(CCursorRange::one(CCursor::new(2)))),
            None
        );
    }

    #[test]
    fn restores_selection_after_context_click() {
        use egui::text::{CCursor, CCursorRange};

        let ctx = egui::Context::default();
        let id = egui::Id::new("restore_log_selection_test");
        let expected = CCursorRange::two(CCursor::new(1), CCursor::new(5));
        let mut state = egui::widgets::text_edit::TextEditState::default();
        state
            .cursor
            .set_char_range(Some(CCursorRange::one(CCursor::new(8))));
        egui::TextEdit::store_state(&ctx, id, state);

        restore_text_selection(&ctx, id, Some(expected));

        assert_eq!(
            egui::TextEdit::load_state(&ctx, id).and_then(|state| state.cursor.char_range()),
            Some(expected)
        );
    }

    fn update_component(
        kind: crate::updater::ComponentKind,
        title: &str,
    ) -> crate::updater::ComponentInfo {
        crate::updater::ComponentInfo {
            kind,
            title: title.to_string(),
            local_version: Some("1.0.0".to_string()),
            latest_version: Some("2.0.0".to_string()),
            status: crate::updater::ComponentStatus::UpdateAvailable,
            asset_name: Some("asset".to_string()),
            download_url: Some("https://example.com/asset".to_string()),
            checksum_url: None,
            digest: None,
        }
    }

    #[test]
    fn selected_updates_bundle_ffmpeg_and_keep_gui_last() {
        use crate::updater::ComponentKind;

        let candidates = vec![
            update_component(ComponentKind::YtDlpGui, "yt-dlp GUI"),
            update_component(ComponentKind::YtDlp, "yt-dlp"),
            update_component(ComponentKind::Ffmpeg, "ffmpeg"),
            update_component(ComponentKind::Ffprobe, "ffprobe"),
        ];
        let selected = [
            ComponentKind::YtDlpGui,
            ComponentKind::YtDlp,
            ComponentKind::Ffmpeg,
            ComponentKind::Ffprobe,
        ];
        let targets = selected_update_targets(&candidates, &selected);

        assert_eq!(
            targets
                .iter()
                .map(|component| component.kind)
                .collect::<Vec<_>>(),
            vec![
                ComponentKind::YtDlp,
                ComponentKind::FfmpegBundle,
                ComponentKind::YtDlpGui,
            ]
        );
    }

    #[test]
    fn selected_updates_can_install_only_ffprobe() {
        use crate::updater::ComponentKind;

        let candidates = vec![
            update_component(ComponentKind::Ffmpeg, "ffmpeg"),
            update_component(ComponentKind::Ffprobe, "ffprobe"),
        ];
        let targets = selected_update_targets(&candidates, &[ComponentKind::Ffprobe]);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].kind, ComponentKind::Ffprobe);
        assert_eq!(targets[0].title, "ffprobe");
    }

    #[test]
    fn preserves_selection_when_secondary_button_is_pressed() {
        use egui::text::{CCursor, CCursorRange};

        let ctx = egui::Context::default();
        let id = egui::Id::new("secondary_press_log_selection_test");
        let logs = "строка журнала";
        let expected = CCursorRange::two(CCursor::new(0), CCursor::new(6));
        let mut text_rect = egui::Rect::NOTHING;

        let mut initial_output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut text = logs;
            let mut output = egui::TextEdit::multiline(&mut text).id(id).show(ui);
            text_rect = output.response.rect;
            output.state.cursor.set_char_range(Some(expected));
            output.state.store(ui.ctx(), id);
        });
        initial_output.textures_delta.clear();

        let pointer_pos = text_rect.center();
        let mut press_output = ctx.run_ui(
            egui::RawInput {
                events: vec![
                    egui::Event::PointerMoved(pointer_pos),
                    egui::Event::PointerButton {
                        pos: pointer_pos,
                        button: egui::PointerButton::Secondary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ui| {
                let previous_cursor_range = egui::TextEdit::load_state(ui.ctx(), id)
                    .and_then(|state| state.cursor.char_range());
                let mut text = logs;
                let response = egui::TextEdit::multiline(&mut text)
                    .id(id)
                    .show(ui)
                    .response;

                assert!(preserve_text_selection_for_context_menu(
                    ui,
                    &response,
                    id,
                    previous_cursor_range,
                ));
            },
        );
        press_output.textures_delta.clear();

        assert_eq!(
            egui::TextEdit::load_state(&ctx, id).and_then(|state| state.cursor.char_range()),
            Some(expected)
        );

        let mut release_output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::PointerButton {
                    pos: pointer_pos,
                    button: egui::PointerButton::Secondary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| {
                let previous_cursor_range = egui::TextEdit::load_state(ui.ctx(), id)
                    .and_then(|state| state.cursor.char_range());
                let mut text = logs;
                let response = egui::TextEdit::multiline(&mut text)
                    .id(id)
                    .show(ui)
                    .response;

                assert!(preserve_text_selection_for_context_menu(
                    ui,
                    &response,
                    id,
                    previous_cursor_range,
                ));
                assert!(response.context_menu(|_| {}).is_some());
            },
        );
        release_output.textures_delta.clear();

        assert_eq!(
            egui::TextEdit::load_state(&ctx, id).and_then(|state| state.cursor.char_range()),
            Some(expected)
        );
    }

    #[test]
    fn read_only_text_edit_copies_keyboard_selection() {
        use egui::text::{CCursor, CCursorRange};

        let ctx = egui::Context::default();
        let id = egui::Id::new("read_only_log_test");
        let logs = "строка журнала";

        let mut first_output = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.memory_mut(|memory| memory.request_focus(id));
            let mut text = logs;
            let mut output = egui::TextEdit::multiline(&mut text).id(id).show(ui);
            output
                .state
                .cursor
                .set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(6))));
            output.state.store(ui.ctx(), id);
        });
        first_output.textures_delta.clear();

        let mut copy_output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..Default::default()
            },
            |ui| {
                ui.memory_mut(|memory| memory.request_focus(id));
                let mut text = logs;
                egui::TextEdit::multiline(&mut text).id(id).show(ui);
            },
        );

        assert!(copy_output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == "строка")
        }));
        copy_output.textures_delta.clear();
        assert_eq!(logs, "строка журнала");
    }

    #[test]
    fn theme_modes_map_to_expected_preferences() {
        assert_eq!(
            ThemeMode::System.preference(),
            egui::ThemePreference::System
        );
        assert_eq!(ThemeMode::Light.preference(), egui::ThemePreference::Light);
        assert_eq!(ThemeMode::Dark.preference(), egui::ThemePreference::Dark);
    }

    #[test]
    fn forced_theme_modes_override_current_theme() {
        let ctx = egui::Context::default();

        apply_theme(&ctx, ThemeMode::Light);
        assert_eq!(ctx.theme(), egui::Theme::Light);

        let mut output = ctx.run_ui(
            egui::RawInput {
                system_theme: Some(egui::Theme::Dark),
                ..Default::default()
            },
            |_| {},
        );
        output.textures_delta.clear();
        assert_eq!(ctx.theme(), egui::Theme::Light);

        apply_theme(&ctx, ThemeMode::Dark);
        assert_eq!(ctx.theme(), egui::Theme::Dark);
    }

    #[test]
    fn system_theme_mode_follows_system_changes() {
        let ctx = egui::Context::default();
        apply_theme(&ctx, ThemeMode::System);

        let mut output = ctx.run_ui(
            egui::RawInput {
                system_theme: Some(egui::Theme::Light),
                ..Default::default()
            },
            |_| {},
        );
        output.textures_delta.clear();
        assert_eq!(ctx.theme(), egui::Theme::Light);

        let mut output = ctx.run_ui(
            egui::RawInput {
                system_theme: Some(egui::Theme::Dark),
                ..Default::default()
            },
            |_| {},
        );
        output.textures_delta.clear();
        assert_eq!(ctx.theme(), egui::Theme::Dark);
    }

    #[test]
    fn selected_theme_mode_is_serialized() {
        let mut cfg = AppConfig::default();
        cfg.theme_mode = ThemeMode::Dark;

        let serialized = toml::to_string(&cfg).unwrap();
        let restored: AppConfig = toml::from_str(&serialized).unwrap();

        assert!(serialized.contains("theme_mode = \"dark\""));
        assert_eq!(restored.theme_mode, ThemeMode::Dark);
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(MAIN_WINDOW_SIZE)
            .with_min_inner_size(MAIN_WINDOW_SIZE)
            .with_max_inner_size(MAIN_WINDOW_SIZE)
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

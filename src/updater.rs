use reqwest::blocking::Client;
use semver::Version;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

const YT_DLP_RELEASES_API: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";
const FFMPEG_RELEASES_API: &str = "https://api.github.com/repos/GyanD/codexffmpeg/releases/latest";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComponentKind {
    YtDlp,
    Ffmpeg,
    Ffprobe,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComponentStatus {
    UpToDate,
    Missing,
    UpdateAvailable,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct ComponentInfo {
    pub kind: ComponentKind,
    pub title: String,
    pub local_version: Option<String>,
    pub latest_version: Option<String>,
    pub status: ComponentStatus,
    pub download_url: Option<String>,
    pub checksum_url: Option<String>,
}

#[derive(Clone, Debug)]
pub enum InstallResult {
    Installed(String),
}

#[derive(Clone, Debug)]
struct ReleaseInfo {
    tag: String,
    assets: Vec<(String, String)>,
}

pub fn check_for_updates(app_dir: &Path, app_version: &str) -> Result<Vec<ComponentInfo>, String> {
    let mut components = Vec::new();

    let _ = app_version;

    let yt_local = read_version_from_binary(&app_dir.join("yt-dlp.exe"), &["--version"]);
    let yt_release = fetch_release(YT_DLP_RELEASES_API).ok();
    let yt_asset = yt_release
        .as_ref()
        .and_then(|release| release.assets.iter().find(|(name, _)| name.eq_ignore_ascii_case("yt-dlp.exe")).cloned());
    let yt_checksum = yt_release
        .as_ref()
        .and_then(|release| release.assets.iter().find(|(name, _)| name.contains("SHA2-256SUMS")).map(|(_, url)| url.clone()));

    components.push(build_component(
        ComponentKind::YtDlp,
        "yt-dlp",
        yt_local,
        yt_release.as_ref().map(|r| r.tag.clone()),
        yt_asset.map(|(_, url)| url),
        yt_checksum,
    ));

    let ffmpeg_path = app_dir.join("ffmpeg.exe");
    let ffprobe_path = app_dir.join("ffprobe.exe");
    let ffmpeg_local = read_version_from_binary(&ffmpeg_path, &["-version"]);
    let ffprobe_local = read_version_from_binary(&ffprobe_path, &["-version"]);

    let ff_release = fetch_release(FFMPEG_RELEASES_API).ok();
    let ff_asset = ff_release.as_ref().and_then(|release| {
        release
            .assets
            .iter()
            .find(|(name, _)| {
                let lower = name.to_lowercase();
                lower.contains("ffmpeg-release-essentials") && lower.ends_with(".zip")
            })
            .cloned()
    });
    let ff_checksum = ff_release.as_ref().and_then(|release| {
        release
            .assets
            .iter()
            .find(|(name, _)| {
                let lower = name.to_lowercase();
                lower.contains("ffmpeg-release-essentials") && lower.contains("sha256")
            })
            .map(|(_, url)| url.clone())
    });

    components.push(build_component(
        ComponentKind::Ffmpeg,
        "ffmpeg",
        ffmpeg_local,
        ff_release.as_ref().map(|r| r.tag.clone()),
        ff_asset.clone().map(|(_, url)| url),
        ff_checksum.clone(),
    ));

    components.push(build_component(
        ComponentKind::Ffprobe,
        "ffprobe",
        ffprobe_local,
        ff_release.as_ref().map(|r| r.tag.clone()),
        ff_asset.map(|(_, url)| url),
        ff_checksum,
    ));

    Ok(components)
}

pub fn install_component(app_dir: &Path, component: &ComponentInfo) -> Result<InstallResult, String> {
    let download_url = component
        .download_url
        .as_ref()
        .ok_or_else(|| format!("{}: ссылка на загрузку не найдена", component.title))?;

    match component.kind {
        ComponentKind::YtDlp => {
            let target = app_dir.join("yt-dlp.exe");
            let staged = app_dir.join("yt-dlp.exe.tmp");
            download_to_path(download_url, &staged)?;
            verify_checksum_if_present(&staged, component.checksum_url.as_deref(), Some("yt-dlp.exe"))?;
            atomic_replace(&staged, &target)?;
            Ok(InstallResult::Installed("yt-dlp обновлён".to_string()))
        }
        ComponentKind::Ffmpeg => {
            let zip_path = app_dir.join("ffmpeg-release-essentials.zip.tmp");
            download_to_path(download_url, &zip_path)?;
            verify_checksum_if_present(&zip_path, component.checksum_url.as_deref(), Some(".zip"))?;
            install_ffmpeg_from_zip(&zip_path, app_dir)?;
            let _ = fs::remove_file(&zip_path);
            Ok(InstallResult::Installed("ffmpeg/ffprobe обновлены".to_string()))
        }
        ComponentKind::Ffprobe => {
            let zip_path = app_dir.join("ffmpeg-release-essentials.zip.tmp");
            download_to_path(download_url, &zip_path)?;
            verify_checksum_if_present(&zip_path, component.checksum_url.as_deref(), Some(".zip"))?;
            install_ffmpeg_from_zip(&zip_path, app_dir)?;
            let _ = fs::remove_file(&zip_path);
            Ok(InstallResult::Installed("ffmpeg/ffprobe обновлены".to_string()))
        }
    }
}

fn build_component(
    kind: ComponentKind,
    title: &str,
    local_version: Option<String>,
    latest_version: Option<String>,
    download_url: Option<String>,
    checksum_url: Option<String>,
) -> ComponentInfo {
    let status = match (&local_version, &latest_version) {
        (None, Some(_)) => ComponentStatus::Missing,
        (None, None) => ComponentStatus::Unknown,
        (Some(_), None) => ComponentStatus::Unknown,
        (Some(local), Some(latest)) => match compare_versions(local, latest) {
            Some(Ordering::Less) => ComponentStatus::UpdateAvailable,
            Some(Ordering::Equal) => ComponentStatus::UpToDate,
            Some(Ordering::Greater) => ComponentStatus::UpToDate,
            None => {
                if normalize_version(local) == normalize_version(latest) {
                    ComponentStatus::UpToDate
                } else {
                    ComponentStatus::UpdateAvailable
                }
            }
        },
    };

    ComponentInfo {
        kind,
        title: title.to_string(),
        local_version,
        latest_version,
        status,
        download_url,
        checksum_url,
    }
}

fn http_client() -> Result<Client, String> {
    Client::builder()
        .user_agent("ytdlp-ui-updater")
        .build()
        .map_err(|e| e.to_string())
}

fn fetch_release(api_url: &str) -> Result<ReleaseInfo, String> {
    let client = http_client()?;
    let response = client.get(api_url).send().map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let value: Value = response.json().map_err(|e| e.to_string())?;

    let tag = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "tag_name не найден".to_string())?;

    let assets = value
        .get("assets")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "assets не найден".to_string())?
        .iter()
        .filter_map(|asset| {
            let name = asset.get("name")?.as_str()?.to_string();
            let url = asset.get("browser_download_url")?.as_str()?.to_string();
            Some((name, url))
        })
        .collect::<Vec<_>>();

    Ok(ReleaseInfo { tag, assets })
}

fn read_version_from_binary(binary_path: &Path, args: &[&str]) -> Option<String> {
    if !binary_path.exists() {
        return None;
    }

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let output = Command::new(binary_path)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or("?").trim();
    if line.contains("version") {
        let parts: Vec<&str> = line.split_whitespace().collect();
        for (idx, part) in parts.iter().enumerate() {
            if *part == "version" && idx + 1 < parts.len() {
                return Some(parts[idx + 1].split('-').next().unwrap_or(parts[idx + 1]).to_string());
            }
        }
    }

    Some(line.to_string())
}

fn normalize_version(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('v')
        .split_whitespace()
        .next()
        .unwrap_or(raw)
        .to_string()
}

fn to_semver(input: &str) -> Option<Version> {
    let mut base = normalize_version(input);
    if let Some(pos) = base.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
        base.truncate(pos);
    }

    let mut parts: Vec<&str> = base.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    while parts.len() < 3 {
        parts.push("0");
    }
    let normalized = format!("{}.{}.{}", parts[0], parts[1], parts[2]);
    Version::parse(&normalized).ok()
}

fn compare_versions(local: &str, latest: &str) -> Option<Ordering> {
    match (to_semver(local), to_semver(latest)) {
        (Some(a), Some(b)) => Some(a.cmp(&b)),
        _ => None,
    }
}

fn download_to_path(url: &str, out_path: &Path) -> Result<(), String> {
    let client = http_client()?;
    let mut response = client.get(url).send().map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("Ошибка загрузки {}: HTTP {}", url, response.status()));
    }

    let mut output = File::create(out_path).map_err(|e| e.to_string())?;
    std::io::copy(&mut response, &mut output).map_err(|e| e.to_string())?;
    Ok(())
}

fn verify_checksum_if_present(file_path: &Path, checksum_url: Option<&str>, filter_name: Option<&str>) -> Result<(), String> {
    let Some(url) = checksum_url else { return Ok(()); };
    let client = http_client()?;
    let body = client
        .get(url)
        .send()
        .and_then(|resp| resp.error_for_status())
        .map_err(|e| e.to_string())?
        .text()
        .map_err(|e| e.to_string())?;

    let file_name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_lowercase();
    let expected = body
        .lines()
        .find_map(|line| {
            let l = line.trim();
            if l.len() < 64 {
                return None;
            }
            let hash = &l[0..64];
            if !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }

            if let Some(f) = filter_name {
                let lf = f.to_lowercase();
                if lf.starts_with('.') {
                    if !l.to_lowercase().contains(&lf) {
                        return None;
                    }
                } else if !l.to_lowercase().contains(&lf) && !l.to_lowercase().contains(&file_name) {
                    return None;
                }
            }
            Some(hash.to_lowercase())
        })
        .or_else(|| {
            let clean = body.trim();
            if clean.len() >= 64 {
                let hash = &clean[0..64];
                if hash.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(hash.to_lowercase());
                }
            }
            None
        })
        .ok_or_else(|| "Не удалось извлечь checksum".to_string())?;

    let mut file = File::open(file_path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 8192];
    loop {
        let read = file.read(&mut buf).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());

    if actual != expected {
        return Err(format!("Checksum mismatch: expected {}, got {}", expected, actual));
    }

    Ok(())
}

fn atomic_replace(staged_path: &Path, target_path: &Path) -> Result<(), String> {
    let backup_path = target_path.with_extension("bak");

    if target_path.exists() {
        let _ = fs::remove_file(&backup_path);
        fs::rename(target_path, &backup_path).map_err(|e| e.to_string())?;
    }

    match fs::rename(staged_path, target_path) {
        Ok(_) => {
            let _ = fs::remove_file(&backup_path);
            Ok(())
        }
        Err(err) => {
            if backup_path.exists() {
                let _ = fs::rename(&backup_path, target_path);
            }
            Err(err.to_string())
        }
    }
}

fn install_ffmpeg_from_zip(zip_path: &Path, app_dir: &Path) -> Result<(), String> {
    let file = File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

    let mut ffmpeg_written = false;
    let mut ffprobe_written = false;

    for idx in 0..archive.len() {
        let mut entry = archive.by_index(idx).map_err(|e| e.to_string())?;
        let name = entry.name().to_string().to_lowercase();
        if name.ends_with("bin/ffmpeg.exe") {
            let staged = app_dir.join("ffmpeg.exe.tmp");
            let mut out = File::create(&staged).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            atomic_replace(&staged, &app_dir.join("ffmpeg.exe"))?;
            ffmpeg_written = true;
        }
        if name.ends_with("bin/ffprobe.exe") {
            let staged = app_dir.join("ffprobe.exe.tmp");
            let mut out = File::create(&staged).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            atomic_replace(&staged, &app_dir.join("ffprobe.exe"))?;
            ffprobe_written = true;
        }
    }

    if !ffmpeg_written || !ffprobe_written {
        return Err("ffmpeg.exe или ffprobe.exe не найдены в архиве".to_string());
    }

    Ok(())
}

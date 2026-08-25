use reqwest::blocking::Client;
use semver::Version;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const YT_DLP_RELEASES_API: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";
const FFMPEG_RELEASES_API: &str = "https://api.github.com/repos/GyanD/codexffmpeg/releases/latest";
const APP_RELEASES_API: &str = "https://api.github.com/repos/amd64fox/ytdlp-ui/releases/latest";
const APP_RELEASE_ASSET: &str = "ytdlp-ui-x64.exe";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const APP_REPOSITORY_URL: &str = "https://github.com/amd64fox/ytdlp-ui";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentKind {
    YtDlpGui,
    YtDlp,
    Ffmpeg,
    Ffprobe,
    FfmpegBundle,
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
    pub asset_name: Option<String>,
    pub download_url: Option<String>,
    pub checksum_url: Option<String>,
    pub digest: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UpdateReport {
    pub components: Vec<ComponentInfo>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum InstallResult {
    Installed(String),
    RestartRequired(String),
}

#[derive(Clone, Debug)]
struct ReleaseInfo {
    tag: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Clone, Debug)]
struct ReleaseAsset {
    name: String,
    url: String,
    digest: Option<String>,
}

pub fn check_for_updates(app_dir: &Path) -> UpdateReport {
    let mut components = Vec::new();
    let mut warnings = Vec::new();
    let client = match http_client() {
        Ok(client) => Some(client),
        Err(err) => {
            warnings.push(format!(
                ">>> Не удалось создать HTTP-клиент для проверки обновлений: {err}"
            ));
            None
        }
    };

    let app_release =
        client
            .as_ref()
            .and_then(|client| match fetch_release(client, APP_RELEASES_API) {
                Ok(release) => Some(release),
                Err(err) => {
                    warnings.push(format!(">>> Не удалось получить релиз yt-dlp GUI: {err}"));
                    None
                }
            });
    let app_asset = app_release.as_ref().and_then(|release| {
        release
            .assets
            .iter()
            .find(|asset| asset.name.eq_ignore_ascii_case(APP_RELEASE_ASSET))
            .cloned()
    });

    components.push(build_component(
        ComponentKind::YtDlpGui,
        "yt-dlp GUI",
        Some(APP_VERSION.to_string()),
        app_release.as_ref().map(|release| release.tag.clone()),
        app_asset.as_ref().map(|asset| asset.name.clone()),
        app_asset.as_ref().map(|asset| asset.url.clone()),
        None,
        app_asset.and_then(|asset| asset.digest),
    ));

    let yt_local = read_version_from_binary(&app_dir.join("yt-dlp.exe"), &["--version"]);
    let yt_release =
        client
            .as_ref()
            .and_then(|client| match fetch_release(client, YT_DLP_RELEASES_API) {
                Ok(release) => Some(release),
                Err(err) => {
                    warnings.push(format!(">>> Не удалось получить релиз yt-dlp: {err}"));
                    None
                }
            });
    let yt_asset = yt_release.as_ref().and_then(|release| {
        release
            .assets
            .iter()
            .find(|asset| asset.name.eq_ignore_ascii_case("yt-dlp.exe"))
            .cloned()
    });
    let yt_checksum = yt_release.as_ref().and_then(|release| {
        release
            .assets
            .iter()
            .find(|asset| asset.name.contains("SHA2-256SUMS"))
            .map(|asset| asset.url.clone())
    });

    components.push(build_component(
        ComponentKind::YtDlp,
        "yt-dlp",
        yt_local,
        yt_release.as_ref().map(|r| r.tag.clone()),
        yt_asset.as_ref().map(|asset| asset.name.clone()),
        yt_asset.map(|asset| asset.url),
        yt_checksum,
        None,
    ));

    let ffmpeg_path = app_dir.join("ffmpeg.exe");
    let ffprobe_path = app_dir.join("ffprobe.exe");
    let ffmpeg_local = read_version_from_binary(&ffmpeg_path, &["-version"]);
    let ffprobe_local = read_version_from_binary(&ffprobe_path, &["-version"]);

    let ff_release =
        client
            .as_ref()
            .and_then(|client| match fetch_release(client, FFMPEG_RELEASES_API) {
                Ok(release) => Some(release),
                Err(err) => {
                    warnings.push(format!(">>> Не удалось получить релиз ffmpeg: {err}"));
                    None
                }
            });
    let ff_asset = ff_release.as_ref().and_then(|release| {
        release
            .assets
            .iter()
            .find(|asset| is_ffmpeg_essentials_zip_asset(&asset.name))
            .cloned()
    });
    let ff_checksum = ff_release.as_ref().and_then(|release| {
        release
            .assets
            .iter()
            .find(|asset| is_ffmpeg_essentials_checksum_asset(&asset.name))
            .map(|asset| asset.url.clone())
    });

    components.push(build_component(
        ComponentKind::Ffmpeg,
        "ffmpeg",
        ffmpeg_local,
        ff_release.as_ref().map(|r| r.tag.clone()),
        ff_asset.as_ref().map(|asset| asset.name.clone()),
        ff_asset.as_ref().map(|asset| asset.url.clone()),
        ff_checksum.clone(),
        None,
    ));

    components.push(build_component(
        ComponentKind::Ffprobe,
        "ffprobe",
        ffprobe_local,
        ff_release.as_ref().map(|r| r.tag.clone()),
        ff_asset.as_ref().map(|asset| asset.name.clone()),
        ff_asset.map(|asset| asset.url),
        ff_checksum,
        None,
    ));

    UpdateReport {
        components,
        warnings,
    }
}

pub fn install_component(
    app_dir: &Path,
    component: &ComponentInfo,
) -> Result<InstallResult, String> {
    let client = http_client()?;
    let download_url = component
        .download_url
        .as_ref()
        .ok_or_else(|| "ссылка на загрузку не найдена".to_string())?;
    let asset_name = component.asset_name.as_deref();

    match component.kind {
        ComponentKind::YtDlpGui => {
            let target = env::current_exe().map_err(|err| err.to_string())?;
            let staged = staged_app_path(&target)?;
            let digest = component
                .digest
                .as_deref()
                .ok_or_else(|| "SHA-256 digest не найден в GitHub Release".to_string())?;
            download_to_path(&client, download_url, &staged)?;
            if let Err(err) = verify_github_digest(&staged, digest) {
                let _ = fs::remove_file(&staged);
                return Err(err);
            }
            if let Err(err) = schedule_app_replacement(&target, &staged) {
                let _ = fs::remove_file(&staged);
                return Err(err);
            }
            Ok(InstallResult::RestartRequired(
                "yt-dlp GUI обновлён, приложение будет перезапущено".to_string(),
            ))
        }
        ComponentKind::YtDlp => {
            let target = app_dir.join("yt-dlp.exe");
            let staged = app_dir.join("yt-dlp.exe.tmp");
            download_to_path(&client, download_url, &staged)?;
            verify_checksum_if_present(
                &client,
                &staged,
                component.checksum_url.as_deref(),
                asset_name.or(Some("yt-dlp.exe")),
            )?;
            atomic_replace(&staged, &target)?;
            Ok(InstallResult::Installed("yt-dlp обновлён".to_string()))
        }
        ComponentKind::Ffmpeg | ComponentKind::Ffprobe | ComponentKind::FfmpegBundle => {
            let zip_path = app_dir.join("ffmpeg-release-essentials.zip.tmp");
            let result = (|| {
                download_to_path(&client, download_url, &zip_path)?;
                verify_checksum_if_present(
                    &client,
                    &zip_path,
                    component.checksum_url.as_deref(),
                    asset_name,
                )?;
                install_ffmpeg_from_zip(&zip_path, app_dir, component.kind)
            })();
            let _ = fs::remove_file(&zip_path);
            result?;
            let message = match component.kind {
                ComponentKind::Ffmpeg => "ffmpeg обновлён",
                ComponentKind::Ffprobe => "ffprobe обновлён",
                ComponentKind::FfmpegBundle => "ffmpeg/ffprobe обновлены",
                ComponentKind::YtDlpGui | ComponentKind::YtDlp => unreachable!(),
            };
            Ok(InstallResult::Installed(message.to_string()))
        }
    }
}

fn build_component(
    kind: ComponentKind,
    title: &str,
    local_version: Option<String>,
    latest_version: Option<String>,
    asset_name: Option<String>,
    download_url: Option<String>,
    checksum_url: Option<String>,
    digest: Option<String>,
) -> ComponentInfo {
    let status = match (&local_version, &latest_version) {
        (None, Some(_)) => ComponentStatus::Missing,
        (None | Some(_), None) => ComponentStatus::Unknown,
        (Some(local), Some(latest)) => match compare_versions(local, latest) {
            Some(Ordering::Less) => ComponentStatus::UpdateAvailable,
            Some(Ordering::Equal | Ordering::Greater) => ComponentStatus::UpToDate,
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
        asset_name,
        download_url,
        checksum_url,
        digest,
    }
}

fn http_client() -> Result<Client, String> {
    Client::builder()
        .user_agent(format!("ytdlp-ui/{APP_VERSION}"))
        .build()
        .map_err(|e| e.to_string())
}

fn fetch_release(client: &Client, api_url: &str) -> Result<ReleaseInfo, String> {
    let response = client.get(api_url).send().map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let value: Value = response.json().map_err(|e| e.to_string())?;

    let tag = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| "tag_name не найден".to_string())?;

    let assets = value
        .get("assets")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "assets не найден".to_string())?
        .iter()
        .filter_map(|asset| {
            let name = asset.get("name")?.as_str()?.to_string();
            let url = asset.get("browser_download_url")?.as_str()?.to_string();
            let digest = asset
                .get("digest")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(ReleaseAsset { name, url, digest })
        })
        .collect::<Vec<_>>();

    Ok(ReleaseInfo { tag, assets })
}

fn is_zip_asset(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

fn is_ffmpeg_essentials_zip_asset(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    is_zip_asset(name)
        && lower.contains("ffmpeg")
        && (lower.contains("essentials_build") || lower.contains("release-essentials"))
}

fn is_ffmpeg_essentials_checksum_asset(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("sha256")
        && lower.contains("ffmpeg")
        && (lower.contains("essentials_build") || lower.contains("release-essentials"))
}

fn read_version_from_binary(binary_path: &Path, args: &[&str]) -> Option<String> {
    if !binary_path.exists() {
        return None;
    }

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
                return Some(
                    parts[idx + 1]
                        .split('-')
                        .next()
                        .unwrap_or(parts[idx + 1])
                        .to_string(),
                );
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

fn download_to_path(client: &Client, url: &str, out_path: &Path) -> Result<(), String> {
    let mut response = client.get(url).send().map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "Ошибка загрузки {}: HTTP {}",
            url,
            response.status()
        ));
    }

    let mut output = File::create(out_path).map_err(|e| e.to_string())?;
    std::io::copy(&mut response, &mut output).map_err(|e| e.to_string())?;
    Ok(())
}

fn verify_checksum_if_present(
    client: &Client,
    file_path: &Path,
    checksum_url: Option<&str>,
    expected_asset_name: Option<&str>,
) -> Result<(), String> {
    let Some(url) = checksum_url else {
        return Ok(());
    };
    let body = client
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| e.to_string())?
        .text()
        .map_err(|e| e.to_string())?;

    let fallback_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let expected = parse_checksum(&body, expected_asset_name, fallback_name)
        .ok_or_else(|| "Не удалось извлечь checksum".to_string())?;

    verify_sha256(file_path, &expected)
}

fn verify_github_digest(file_path: &Path, digest: &str) -> Result<(), String> {
    let expected = parse_github_sha256(digest)
        .ok_or_else(|| format!("Неподдерживаемый GitHub digest: {digest}"))?;

    verify_sha256(file_path, expected)
}

fn parse_github_sha256(digest: &str) -> Option<&str> {
    digest
        .strip_prefix("sha256:")
        .filter(|hash| hash.len() == 64 && hash.chars().all(|ch| ch.is_ascii_hexdigit()))
}

fn verify_sha256(file_path: &Path, expected: &str) -> Result<(), String> {
    let expected = expected.to_ascii_lowercase();

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
        return Err(format!(
            "Checksum mismatch: expected {expected}, got {actual}"
        ));
    }

    Ok(())
}

fn parse_checksum(
    manifest: &str,
    expected_asset_name: Option<&str>,
    fallback_name: &str,
) -> Option<String> {
    let expected_name = expected_asset_name.unwrap_or(fallback_name);
    let matched = manifest.lines().find_map(|line| {
        let line = line.trim();
        if line.len() < 64 {
            return None;
        }
        let hash = &line[..64];
        if !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return None;
        }

        let name_part = line[64..]
            .trim_start_matches(|ch: char| ch.is_whitespace() || ch == '*')
            .trim();
        let normalized_name = name_part.rsplit(['/', '\\']).next().unwrap_or(name_part);
        normalized_name
            .eq_ignore_ascii_case(expected_name)
            .then(|| hash.to_ascii_lowercase())
    });

    matched.or_else(|| {
        if expected_asset_name.is_some() {
            return None;
        }
        let clean = manifest.trim();
        let hash = clean.get(..64)?;
        hash.chars()
            .all(|ch| ch.is_ascii_hexdigit())
            .then(|| hash.to_ascii_lowercase())
    })
}

fn atomic_replace(staged_path: &Path, target_path: &Path) -> Result<(), String> {
    let backup_path = target_path.with_extension("bak");

    if target_path.exists() {
        let _ = fs::remove_file(&backup_path);
        fs::rename(target_path, &backup_path).map_err(|e| e.to_string())?;
    }

    match fs::rename(staged_path, target_path) {
        Ok(()) => {
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

fn install_ffmpeg_from_zip(
    zip_path: &Path,
    app_dir: &Path,
    component_kind: ComponentKind,
) -> Result<(), String> {
    let file = File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

    let install_ffmpeg = matches!(
        component_kind,
        ComponentKind::Ffmpeg | ComponentKind::FfmpegBundle
    );
    let install_ffprobe = matches!(
        component_kind,
        ComponentKind::Ffprobe | ComponentKind::FfmpegBundle
    );
    let mut ffmpeg_written = false;
    let mut ffprobe_written = false;

    for idx in 0..archive.len() {
        let mut entry = archive.by_index(idx).map_err(|e| e.to_string())?;
        let name = entry.name().to_string().to_lowercase();
        if install_ffmpeg && name.ends_with("bin/ffmpeg.exe") {
            let staged = app_dir.join("ffmpeg.exe.tmp");
            let mut out = File::create(&staged).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            atomic_replace(&staged, &app_dir.join("ffmpeg.exe"))?;
            ffmpeg_written = true;
        }
        if install_ffprobe && name.ends_with("bin/ffprobe.exe") {
            let staged = app_dir.join("ffprobe.exe.tmp");
            let mut out = File::create(&staged).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            atomic_replace(&staged, &app_dir.join("ffprobe.exe"))?;
            ffprobe_written = true;
        }
    }

    if (install_ffmpeg && !ffmpeg_written) || (install_ffprobe && !ffprobe_written) {
        return Err("Выбранные компоненты ffmpeg не найдены в архиве".to_string());
    }

    Ok(())
}

fn staged_app_path(current_exe: &Path) -> Result<PathBuf, String> {
    let file_stem = current_exe
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| "Не удалось определить имя приложения".to_string())?;
    Ok(current_exe.with_file_name(format!("{file_stem}.update.exe")))
}

fn schedule_app_replacement(current_exe: &Path, staged_exe: &Path) -> Result<(), String> {
    let script_path = current_exe.with_extension("update.cmd");
    fs::write(&script_path, self_update_script()).map_err(|err| err.to_string())?;

    let result = Command::new(&script_path)
        .arg(current_exe)
        .arg(staged_exe)
        .arg(std::process::id().to_string())
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|err| err.to_string());

    if result.is_err() {
        let _ = fs::remove_file(script_path);
    }
    result
}

fn self_update_script() -> &'static str {
    "@echo off\r\n\
setlocal\r\n\
set \"TARGET=%~1\"\r\n\
set \"STAGED=%~2\"\r\n\
set \"PID=%~3\"\r\n\
:wait\r\n\
tasklist /FI \"PID eq %PID%\" 2>NUL | find \"%PID%\" >NUL\r\n\
if not errorlevel 1 (\r\n\
  timeout /T 1 /NOBREAK >NUL\r\n\
  goto wait\r\n\
)\r\n\
set /A ATTEMPTS=0\r\n\
:replace\r\n\
move /Y \"%STAGED%\" \"%TARGET%\" >NUL 2>&1\r\n\
if not errorlevel 1 goto launch\r\n\
set /A ATTEMPTS+=1\r\n\
if %ATTEMPTS% GEQ 30 goto failed\r\n\
timeout /T 1 /NOBREAK >NUL\r\n\
goto replace\r\n\
:failed\r\n\
start \"\" \"%TARGET%\"\r\n\
exit /B 1\r\n\
:launch\r\n\
if /I \"%~4\"==\"--no-launch\" goto cleanup\r\n\
start \"\" \"%TARGET%\"\r\n\
:cleanup\r\n\
(goto) 2>NUL & del \"%~f0\"\r\n"
}

#[cfg(test)]
mod tests {
    use super::{
        build_component, parse_checksum, parse_github_sha256, self_update_script, staged_app_path,
        verify_github_digest, ComponentKind, ComponentStatus, APP_RELEASE_ASSET,
    };
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn checksum_requires_the_requested_release_asset() {
        let wrong_hash = "a".repeat(64);
        let expected_hash = "B".repeat(64);
        let manifest = format!("{wrong_hash}  other.exe\n{expected_hash}  {APP_RELEASE_ASSET}\n");

        assert_eq!(
            parse_checksum(&manifest, Some(APP_RELEASE_ASSET), "ignored.tmp"),
            Some(expected_hash.to_ascii_lowercase())
        );
        assert_eq!(
            parse_checksum(&manifest, Some("missing.exe"), "ignored.tmp"),
            None
        );
    }

    #[test]
    fn github_digest_requires_a_sha256_value() {
        let hash = "a".repeat(64);

        assert_eq!(
            parse_github_sha256(&format!("sha256:{hash}")),
            Some(hash.as_str())
        );
        assert_eq!(parse_github_sha256(&hash), None);
        assert_eq!(parse_github_sha256("sha512:abc"), None);
        assert_eq!(parse_github_sha256("sha256:not-a-hash"), None);
    }

    #[test]
    fn github_digest_verifies_the_downloaded_file() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let file_path = std::env::temp_dir().join(format!(
            "ytdlp-ui-digest-test-{}-{nonce}.exe",
            std::process::id()
        ));
        fs::write(&file_path, []).expect("write test file");

        assert!(verify_github_digest(
            &file_path,
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        )
        .is_ok());
        assert!(verify_github_digest(
            &file_path,
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        )
        .is_err());

        fs::remove_file(file_path).expect("remove test file");
    }

    #[test]
    fn gui_release_tags_are_compared_with_the_cargo_version() {
        let current = build_component(
            ComponentKind::YtDlpGui,
            "yt-dlp GUI",
            Some("0.1.0".to_string()),
            Some("v0.1.0".to_string()),
            None,
            None,
            None,
            None,
        );
        let newer = build_component(
            ComponentKind::YtDlpGui,
            "yt-dlp GUI",
            Some("0.1.0".to_string()),
            Some("v0.2.0".to_string()),
            None,
            None,
            None,
            None,
        );

        assert_eq!(current.status, ComponentStatus::UpToDate);
        assert_eq!(newer.status, ComponentStatus::UpdateAvailable);
    }

    #[test]
    fn staged_update_stays_next_to_the_running_executable() {
        assert_eq!(
            staged_app_path(Path::new(r"C:\Apps\ytdlp-ui.exe")),
            Ok(Path::new(r"C:\Apps\ytdlp-ui.update.exe").to_path_buf())
        );
    }

    #[test]
    fn self_update_script_replaces_the_staged_file() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let test_dir = std::env::temp_dir().join(format!(
            "ytdlp-ui-self-update-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&test_dir).expect("create test directory");

        let target = test_dir.join("app.exe");
        let staged = test_dir.join("app.update.exe");
        let script = test_dir.join("app.update.cmd");
        fs::write(&target, b"old").expect("write target");
        fs::write(&staged, b"new").expect("write staged update");
        fs::write(&script, self_update_script()).expect("write update script");

        let status = Command::new(&script)
            .arg(&target)
            .arg(&staged)
            .arg("4294967294")
            .arg("--no-launch")
            .status()
            .expect("run update script");

        assert!(status.success());
        assert_eq!(fs::read(&target).expect("read replaced target"), b"new");
        assert!(!staged.exists());
        fs::remove_dir_all(&test_dir).expect("remove test directory");
    }
}

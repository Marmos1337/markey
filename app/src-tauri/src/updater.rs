//! Update check, download and install, from MarKey's GitHub releases.
//!
//! The feed is the GitHub "latest release" API of Marmos1337/MarKey. A newer
//! version is offered with its notes ("Что нового", and "What's new" for
//! English). Before anything is opened or installed, the downloaded file is
//! checked against `SHA256SUMS` from the same release; a file that is not
//! listed there, or whose hash differs, is deleted and refused.
//!
//! The hash protects against a corrupted or swapped download, not against
//! someone who controls the release itself: MarKey has no signing key of its
//! own yet (see README, "Сборка").
//!
//! `MARKEY_UPDATE_FEED` points the check at another feed URL — a local
//! fixture in tests and for trying the dialog. Only https, or http to this
//! machine, is ever fetched.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_FEED: &str = "https://api.github.com/repos/Marmos1337/MarKey/releases/latest";
const USER_AGENT: &str = concat!("MarKey/", env!("CARGO_PKG_VERSION"), " (update check)");
const TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// No installer of ours comes close; a bigger answer is not ours.
const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;
const MAX_FEED: u64 = 2 * 1024 * 1024;
const MAX_NOTES: usize = 4000;
pub const SUMS_NAME: &str = "SHA256SUMS";

/// The feed to use: `MARKEY_UPDATE_FEED`, or GitHub.
pub fn feed_url() -> String {
    std::env::var("MARKEY_UPDATE_FEED").unwrap_or_else(|_| DEFAULT_FEED.to_string())
}

/// What the update dialog shows, and what download and install need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Update {
    pub version: String,
    pub current: String,
    /// «Что нового» from the release, or the whole text when it has no such
    /// section.
    pub notes_ru: String,
    /// "What's new", when the release has it.
    pub notes_en: String,
    pub page: String,
    pub asset_name: String,
    #[serde(skip)]
    pub asset_url: String,
    #[serde(skip)]
    pub sums_url: Option<String>,
    pub size: u64,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// `0.3.0` from `v0.3.0`; `None` for anything that is not plain x.y.z.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let mut parts = text.split('.');
    let v = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(v)
}

/// Only https, or plain http to this machine (fixtures).
fn allowed(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|host| authority == *host || authority.starts_with(&format!("{host}:")))
}

/// The installer for this machine among the release's files.
fn pick_asset<'a>(assets: &'a [Asset], os: &str, arch: &str) -> Option<&'a Asset> {
    let name = |a: &&Asset| a.name.to_lowercase();
    match os {
        "macos" => {
            let arch_tags: &[&str] = if arch == "aarch64" {
                &["aarch64", "arm64", "universal"]
            } else {
                &["x64", "x86_64", "intel", "universal"]
            };
            assets.iter().find(|a| {
                let n = name(a);
                n.ends_with(".dmg") && arch_tags.iter().any(|t| n.contains(t))
            })
        }
        "windows" => assets
            .iter()
            .find(|a| name(a).ends_with("-setup.exe"))
            .or_else(|| assets.iter().find(|a| name(a).ends_with(".msi"))),
        _ => None,
    }
}

/// The «Что нового» and "What's new" sections of release notes.
pub fn split_notes(body: &str) -> (String, String) {
    let mut ru = Vec::new();
    let mut en = Vec::new();
    let mut other = Vec::new();
    #[derive(PartialEq)]
    enum Part {
        None,
        Ru,
        En,
        Skip,
    }
    let mut part = Part::None;
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix('#') {
            let heading = heading.trim_start_matches('#').trim().to_lowercase();
            part = if heading.starts_with("что нового") {
                Part::Ru
            } else if heading.starts_with("what's new") || heading.starts_with("what’s new") {
                Part::En
            } else {
                Part::Skip
            };
            continue;
        }
        match part {
            Part::Ru => ru.push(line),
            Part::En => en.push(line),
            Part::None => other.push(line),
            Part::Skip => {}
        }
    }
    let clean = |lines: Vec<&str>| -> String {
        let text = lines.join("\n").trim().to_string();
        // A section left as the template's empty bullet says nothing.
        let meaningful = text
            .lines()
            .any(|l| !l.trim().trim_start_matches(['-', '*']).trim().is_empty());
        if !meaningful {
            return String::new();
        }
        text.chars().take(MAX_NOTES).collect()
    };
    let ru = clean(ru);
    let en = clean(en);
    if ru.is_empty() && en.is_empty() {
        return (clean(other), String::new());
    }
    (ru, en)
}

fn get(url: &str, timeout: Duration) -> Result<ureq::http::Response<ureq::Body>, String> {
    if !allowed(url) {
        return Err(format!("refusing a non-https address: {url}"));
    }
    ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .map_err(|e| e.to_string())
}

/// Ask the feed for the latest release. `Ok(None)` when it is not newer than
/// `current`, is a draft or a pre-release, or has nothing for this machine.
pub fn check(feed: &str, current: &str) -> Result<Option<Update>, String> {
    check_for(feed, current, std::env::consts::OS, std::env::consts::ARCH)
}

pub fn check_for(
    feed: &str,
    current: &str,
    os: &str,
    arch: &str,
) -> Result<Option<Update>, String> {
    let mut response = get(feed, TIMEOUT)?;
    let release: Release = response
        .body_mut()
        .with_config()
        .limit(MAX_FEED)
        .read_json()
        .map_err(|e| format!("unexpected answer from the update feed: {e}"))?;
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let (Some(latest), Some(mine)) = (parse_version(&release.tag_name), parse_version(current))
    else {
        return Ok(None);
    };
    if latest <= mine {
        return Ok(None);
    }
    let Some(asset) = pick_asset(&release.assets, os, arch) else {
        log::warn!(
            "release {} has no installer for {os}/{arch}",
            release.tag_name
        );
        return Ok(None);
    };
    let (notes_ru, notes_en) = split_notes(release.body.as_deref().unwrap_or(""));
    Ok(Some(Update {
        version: format!("{}.{}.{}", latest.0, latest.1, latest.2),
        current: current.to_string(),
        notes_ru,
        notes_en,
        page: release.html_url,
        asset_name: asset.name.clone(),
        asset_url: asset.browser_download_url.clone(),
        sums_url: release
            .assets
            .iter()
            .find(|a| a.name == SUMS_NAME)
            .map(|a| a.browser_download_url.clone()),
        size: asset.size,
    }))
}

/// The expected hash of `name` in a SHA256SUMS file (`sha256sum` format:
/// hash, two spaces or a space and `*`, file name).
pub fn expected_hash(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start().trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

/// Download the installer into `dir` and verify it. On any mismatch the file
/// is removed and an error returned: nothing unverified is left behind.
pub fn download_verified(update: &Update, dir: &Path) -> Result<PathBuf, String> {
    let sums_url = update
        .sums_url
        .as_deref()
        .ok_or("в релизе нет файла SHA256SUMS — устанавливать без проверки нельзя")?;
    let sums = get(sums_url, TIMEOUT)?
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let expected = expected_hash(&sums, &update.asset_name)
        .ok_or("установщика нет в SHA256SUMS — устанавливать без проверки нельзя")?;
    // A plain file name only: the name comes from the release.
    if update.asset_name.contains(['/', '\\']) || update.asset_name.starts_with('.') {
        return Err("странное имя файла в релизе".into());
    }

    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(&update.asset_name);
    let partial = dir.join(format!("{}.part", update.asset_name));
    let result = (|| -> Result<String, String> {
        let mut response = get(&update.asset_url, DOWNLOAD_TIMEOUT)?;
        let mut reader = response
            .body_mut()
            .with_config()
            .limit(MAX_DOWNLOAD)
            .reader();
        let mut file = std::fs::File::create(&partial).map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            file.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
        }
        file.sync_all().map_err(|e| e.to_string())?;
        Ok(hex(&hasher.finalize()))
    })();
    let actual = match result {
        Ok(hash) => hash,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    };
    if actual != expected {
        let _ = std::fs::remove_file(&partial);
        log::warn!(
            "update {}: checksum mismatch; download deleted",
            update.version
        );
        return Err("контрольная сумма не совпала — файл удалён, установка отменена".into());
    }
    std::fs::rename(&partial, &path).map_err(|e| e.to_string())?;
    log::info!("update {}: downloaded and verified", update.version);
    Ok(path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where updates are downloaded before installing.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("com.marmos.markey")
        .join("updates")
}

/// The `.app` bundle this process runs from.
#[cfg(target_os = "macos")]
fn current_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf)
}

/// Install the verified file and start the new version. Returns once the
/// relaunch is scheduled; the caller then quits.
#[cfg(target_os = "macos")]
pub fn install(file: &Path) -> Result<(), String> {
    use std::process::Command;
    let target =
        current_bundle().ok_or("MarKey запущен не из .app — установите обновление вручную")?;
    let parent = target.parent().ok_or("нет папки приложения")?;
    let mount = std::env::temp_dir().join(format!("markey-update-{}", std::process::id()));
    std::fs::create_dir_all(&mount).map_err(|e| e.to_string())?;
    let attach = Command::new("/usr/bin/hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-noautoopen",
            "-mountpoint",
        ])
        .arg(&mount)
        .arg(file)
        .output()
        .map_err(|e| e.to_string())?;
    if !attach.status.success() {
        return Err("не удалось открыть образ обновления".into());
    }
    let detach = || {
        let _ = Command::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet"])
            .arg(&mount)
            .status();
        let _ = std::fs::remove_dir(&mount);
    };
    let result = (|| -> Result<(), String> {
        let source = std::fs::read_dir(&mount)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .ok_or("в образе нет приложения")?;
        // The new bundle must be intact before it replaces the old one.
        let verify = Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&source)
            .status()
            .map_err(|e| e.to_string())?;
        if !verify.success() {
            return Err("подпись нового приложения не сходится — установка отменена".into());
        }
        let staged = parent.join(".MarKey.update.app");
        let old = parent.join(".MarKey.previous.app");
        let _ = std::fs::remove_dir_all(&staged);
        let _ = std::fs::remove_dir_all(&old);
        let copied = Command::new("/usr/bin/ditto")
            .arg(&source)
            .arg(&staged)
            .status()
            .map_err(|e| e.to_string())?;
        if !copied.success() {
            let _ = std::fs::remove_dir_all(&staged);
            return Err("не удалось скопировать новую версию (нет прав на папку?)".into());
        }
        // Swap, keeping the old version until the new one is in place.
        std::fs::rename(&target, &old).map_err(|e| e.to_string())?;
        if let Err(e) = std::fs::rename(&staged, &target) {
            let _ = std::fs::rename(&old, &target);
            return Err(e.to_string());
        }
        let _ = std::fs::remove_dir_all(&old);
        Ok(())
    })();
    detach();
    result?;
    // Reopen once this process has quit.
    Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 1; /usr/bin/open -n \"$0\"")
        .arg(&target)
        .spawn()
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(file);
    Ok(())
}

/// Windows: run the verified installer; it replaces the running copy after
/// MarKey quits.
#[cfg(target_os = "windows")]
pub fn install(file: &Path) -> Result<(), String> {
    use std::process::Command;
    let is_msi = file
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("msi"));
    let spawned = if is_msi {
        Command::new("msiexec").arg("/i").arg(file).spawn()
    } else {
        Command::new(file).spawn()
    };
    spawned.map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn install(_file: &Path) -> Result<(), String> {
    Err("обновление ставится только на macOS и Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A tiny HTTP server for a fixture feed: path -> body.
    struct Fixture {
        base: String,
        routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    }

    impl Fixture {
        fn start() -> Fixture {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
            let served = routes.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut request = Vec::new();
                    let mut byte = [0u8; 1];
                    while !request.ends_with(b"\r\n\r\n") {
                        if stream.read(&mut byte).unwrap_or(0) == 0 {
                            break;
                        }
                        request.push(byte[0]);
                    }
                    let text = String::from_utf8_lossy(&request);
                    let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let body = served.lock().unwrap().get(&path).cloned();
                    let (status, body) = match body {
                        Some(b) => ("200 OK", b),
                        None => ("404 Not Found", b"not found".to_vec()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                }
            });
            Fixture { base, routes }
        }

        fn serve(&self, path: &str, body: impl Into<Vec<u8>>) {
            self.routes
                .lock()
                .unwrap()
                .insert(path.to_string(), body.into());
        }

        fn release(&self, tag: &str, body: &str, extra: &str) {
            let json = format!(
                r#"{{"tag_name":"{tag}","html_url":"https://github.com/Marmos1337/MarKey/releases/tag/{tag}",
                "draft":false,"prerelease":false,"body":{body},
                "assets":[
                  {{"name":"MarKey_0.3.0_aarch64.dmg","browser_download_url":"{b}/MarKey_0.3.0_aarch64.dmg","size":11}},
                  {{"name":"MarKey_0.3.0_x64.dmg","browser_download_url":"{b}/MarKey_0.3.0_x64.dmg","size":11}},
                  {{"name":"MarKey_0.3.0_x64-setup.exe","browser_download_url":"{b}/setup.exe","size":11}}
                  {extra}
                ]}}"#,
                body = serde_json::to_string(body).unwrap(),
                b = self.base,
            );
            self.serve("/latest", json);
        }
    }

    const NOTES: &str = "## Что нового\n\n- Расширенный словарь\n- Окно обновления\n\n## What's new\n\n- Extended dictionary\n\n## Installing\n\nOpen the dmg.";

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("markey-update-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sha(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert_eq!(parse_version("v0.10.2"), Some((0, 10, 2)));
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
        assert_eq!(parse_version("v1.0.0-beta"), None);
        assert_eq!(parse_version("1.0"), None);
    }

    #[test]
    fn notes_are_split_into_russian_and_english() {
        let (ru, en) = split_notes(NOTES);
        assert_eq!(ru, "- Расширенный словарь\n- Окно обновления");
        assert_eq!(en, "- Extended dictionary");
        // The release template's empty bullets are not notes.
        let (ru, en) =
            split_notes("## Что нового\n\n- \n\n## What's new\n\n- \n\n## Installing\n\nx");
        assert_eq!((ru.as_str(), en.as_str()), ("", ""));
        // No sections: the whole text.
        assert_eq!(split_notes("Fixes.").0, "Fixes.");
    }

    #[test]
    fn a_newer_release_is_offered_with_the_right_installer() {
        let f = Fixture::start();
        f.release(
            "v0.3.0",
            NOTES,
            &format!(
                r#",{{"name":"SHA256SUMS","browser_download_url":"{}/SHA256SUMS"}}"#,
                f.base
            ),
        );
        let feed = format!("{}/latest", f.base);
        let update = check_for(&feed, "0.2.0", "macos", "aarch64")
            .unwrap()
            .unwrap();
        assert_eq!(update.version, "0.3.0");
        assert_eq!(update.asset_name, "MarKey_0.3.0_aarch64.dmg");
        assert_eq!(update.notes_ru, "- Расширенный словарь\n- Окно обновления");
        assert!(update.sums_url.is_some());
        let intel = check_for(&feed, "0.2.0", "macos", "x86_64")
            .unwrap()
            .unwrap();
        assert_eq!(intel.asset_name, "MarKey_0.3.0_x64.dmg");
        let windows = check_for(&feed, "0.2.0", "windows", "x86_64")
            .unwrap()
            .unwrap();
        assert_eq!(windows.asset_name, "MarKey_0.3.0_x64-setup.exe");
        // Same or older: nothing.
        assert_eq!(check_for(&feed, "0.3.0", "macos", "aarch64").unwrap(), None);
        assert_eq!(check_for(&feed, "0.4.1", "macos", "aarch64").unwrap(), None);
    }

    #[test]
    fn drafts_and_prereleases_are_not_offered() {
        let f = Fixture::start();
        f.serve(
            "/latest",
            r#"{"tag_name":"v9.0.0","draft":true,"prerelease":false,"assets":[]}"#,
        );
        assert_eq!(
            check_for(&format!("{}/latest", f.base), "0.2.0", "macos", "aarch64").unwrap(),
            None
        );
        f.serve(
            "/latest",
            r#"{"tag_name":"v9.0.0","draft":false,"prerelease":true,"assets":[]}"#,
        );
        assert_eq!(
            check_for(&format!("{}/latest", f.base), "0.2.0", "macos", "aarch64").unwrap(),
            None
        );
    }

    fn update_for(f: &Fixture, with_sums: bool) -> Update {
        let extra = if with_sums {
            format!(
                r#",{{"name":"SHA256SUMS","browser_download_url":"{}/SHA256SUMS"}}"#,
                f.base
            )
        } else {
            String::new()
        };
        f.release("v0.3.0", NOTES, &extra);
        check_for(&format!("{}/latest", f.base), "0.2.0", "macos", "aarch64")
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_verified_download_is_kept() {
        let f = Fixture::start();
        let installer = b"dmg-bytes!!".to_vec();
        f.serve("/MarKey_0.3.0_aarch64.dmg", installer.clone());
        f.serve(
            "/SHA256SUMS",
            format!(
                "{}  MarKey_0.3.0_x64.dmg\n{}  MarKey_0.3.0_aarch64.dmg\n",
                sha(b"other"),
                sha(&installer)
            ),
        );
        let dir = temp("ok");
        let path = download_verified(&update_for(&f, true), &dir).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), installer);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_mismatching_download_is_deleted_and_refused() {
        let f = Fixture::start();
        f.serve("/MarKey_0.3.0_aarch64.dmg", b"tampered!!!".to_vec());
        f.serve(
            "/SHA256SUMS",
            format!("{}  MarKey_0.3.0_aarch64.dmg\n", sha(b"dmg-bytes!!")),
        );
        let dir = temp("bad");
        let err = download_verified(&update_for(&f, true), &dir).unwrap_err();
        assert!(err.contains("контрольная сумма"), "{err}");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert!(left.is_empty(), "nothing unverified may remain: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_sums_or_an_entry_nothing_is_downloaded() {
        let f = Fixture::start();
        f.serve("/MarKey_0.3.0_aarch64.dmg", b"dmg-bytes!!".to_vec());
        let dir = temp("nosums");
        assert!(download_verified(&update_for(&f, false), &dir).is_err());
        assert!(!dir.join("MarKey_0.3.0_aarch64.dmg").exists());
        f.serve(
            "/SHA256SUMS",
            format!("{}  Something-else.dmg\n", sha(b"dmg-bytes!!")),
        );
        assert!(download_verified(&update_for(&f, true), &dir).is_err());
        assert!(!dir.join("MarKey_0.3.0_aarch64.dmg").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_https_or_this_machine_is_fetched() {
        assert!(allowed("https://api.github.com/x"));
        assert!(allowed("http://127.0.0.1:8080/latest"));
        assert!(allowed("http://localhost/latest"));
        assert!(!allowed("http://example.com/latest"));
        assert!(!allowed("http://127.0.0.1.evil.com/latest"));
        assert!(!allowed("file:///etc/passwd"));
    }

    #[test]
    fn sums_lines_are_read_in_both_sha256sum_formats() {
        let h = "a".repeat(64);
        assert_eq!(
            expected_hash(&format!("{h}  MarKey.dmg"), "MarKey.dmg"),
            Some(h.clone())
        );
        assert_eq!(
            expected_hash(&format!("{h} *MarKey.dmg"), "MarKey.dmg"),
            Some(h.clone())
        );
        assert_eq!(
            expected_hash(&format!("{h}  MarKey.dmg"), "Other.dmg"),
            None
        );
        assert_eq!(expected_hash("short  MarKey.dmg", "MarKey.dmg"), None);
    }
}

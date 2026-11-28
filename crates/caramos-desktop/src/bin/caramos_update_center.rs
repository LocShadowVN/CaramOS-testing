#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing,
    unreachable_patterns,
    unused_must_use
)]

use caramos_desktop::{
    install_caramos_css, invoke_self_pkexec, open_external_url, CaramDesktopError,
};
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use libadwaita::prelude::AdwApplicationWindowExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::Duration;
use tempfile::NamedTempFile;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const APP_ID: &str = "org.caramos.UpdateCenter";
const GITHUB_RELEASES_API: &str =
    "https://api.github.com/repos/LocShadowVN/CaramOS-testing/releases?per_page=15";
const VERSION_FILE: &str = "/etc/caramos-version";
const CHANNEL_FILE: &str = "/etc/caramos-channel";

type RefreshFnSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateChannel {
    Stable,
    Testing,
}

impl UpdateChannel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Testing => "testing",
        }
    }

    fn display_label(self) -> &'static str {
        match self {
            Self::Stable => "Ổn định (Stable)",
            Self::Testing => "Thử nghiệm (Testing)",
        }
    }

    fn flipped(self) -> Self {
        match self {
            Self::Stable => Self::Testing,
            Self::Testing => Self::Stable,
        }
    }
}

/// Thứ tự phân cấp SemVer: Alpha < Beta < Testing < Rc < Stable
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PreRelease {
    Alpha(u32),
    Beta(u32),
    Testing(u32),
    Rc(u32),
    Stable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CaramVersion {
    major: u32,
    minor: u32,
    patch: u32,
    pre: PreRelease,
}

#[derive(Debug, Clone)]
struct PendingUpdate {
    source_label: &'static str,
    name: String,
    version_note: String,
}

#[derive(Debug, Clone)]
struct ReleaseAssetInfo {
    name: String,
    download_url: String,
    size_bytes: u64,
    expected_sha256: Option<String>,
}

#[derive(Debug, Clone)]
struct CaramOsReleaseUpdate {
    current_version: String,
    new_version: String,
    is_prerelease: bool,
    title: String,
    notes: String,
    html_url: String,
    ota_bundle: Option<ReleaseAssetInfo>,
    iso_asset: Option<ReleaseAssetInfo>,
}

#[derive(Debug, Clone)]
struct UpdateScan {
    channel: UpdateChannel,
    caramos_release: Option<CaramOsReleaseUpdate>,
    apt_items: Vec<PendingUpdate>,
    flatpak_items: Vec<PendingUpdate>,
}

impl UpdateScan {
    fn total(&self) -> usize {
        usize::from(self.caramos_release.is_some())
            + self.apt_items.len()
            + self.flatpak_items.len()
    }
}

#[derive(Debug)]
enum UpgradeEvent {
    Step {
        headline: String,
        log: String,
    },
    Done {
        ok: bool,
        detail: String,
        reboot_required: bool,
    },
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

fn read_installed_version() -> String {
    fs::read_to_string(VERSION_FILE)
        .ok()
        .map(|s| s.trim().trim_start_matches(['v', 'V']).to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

fn read_current_channel() -> UpdateChannel {
    let raw = fs::read_to_string(CHANNEL_FILE).unwrap_or_default();
    if raw.trim().eq_ignore_ascii_case("testing") {
        UpdateChannel::Testing
    } else if raw.trim().eq_ignore_ascii_case("stable") {
        UpdateChannel::Stable
    } else if read_installed_version().contains('-') {
        UpdateChannel::Testing
    } else {
        UpdateChannel::Stable
    }
}

fn parse_trailing_num(rest: &str) -> u32 {
    let digits: String = rest
        .trim_start_matches(['.', '-', '_'])
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse::<u32>().unwrap_or(0)
}

fn parse_caram_version(raw: &str) -> Option<CaramVersion> {
    let cleaned = raw.trim().trim_start_matches(['v', 'V']);
    let without_build = cleaned.split('+').next().unwrap_or("");
    let (core, pre_raw) = match without_build.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (without_build, None),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    let patch = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);

    let pre = match pre_raw {
        None => PreRelease::Stable,
        Some(tag) => {
            let lower = tag.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("alpha") {
                PreRelease::Alpha(parse_trailing_num(rest))
            } else if let Some(rest) = lower.strip_prefix("beta") {
                PreRelease::Beta(parse_trailing_num(rest))
            } else if let Some(rest) = lower.strip_prefix("testing") {
                PreRelease::Testing(parse_trailing_num(rest))
            } else if let Some(rest) = lower.strip_prefix("test") {
                PreRelease::Testing(parse_trailing_num(rest))
            } else if let Some(rest) = lower.strip_prefix("rc") {
                PreRelease::Rc(parse_trailing_num(rest))
            } else {
                PreRelease::Testing(parse_trailing_num(&lower))
            }
        }
    };

    Some(CaramVersion {
        major,
        minor,
        patch,
        pre,
    })
}

fn is_newer_version(current: &str, candidate: &str) -> bool {
    match (parse_caram_version(current), parse_caram_version(candidate)) {
        (Some(cur), Some(cand)) => cand > cur,
        _ => false,
    }
}

fn fetch_sha256_text(agent: &ureq::Agent, url: &str) -> Option<String> {
    let text = agent
        .get(url)
        .set("User-Agent", "caramos-update-center/2.0")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let token = text.split_whitespace().next()?;
    if token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(token.to_ascii_lowercase())
    } else {
        None
    }
}

fn extract_asset_sha(agent: &ureq::Agent, assets: &[GhAsset], asset: &GhAsset) -> Option<String> {
    if let Some(hex) = asset
        .digest
        .as_deref()
        .and_then(|d| d.trim().strip_prefix("sha256:"))
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Some(hex.to_ascii_lowercase());
    }
    let sha_name = format!("{}.sha256", asset.name);
    let sha_asset = assets.iter().find(|a| a.name == sha_name)?;
    fetch_sha256_text(agent, &sha_asset.browser_download_url)
}

fn check_github_release_update(channel: UpdateChannel) -> Option<CaramOsReleaseUpdate> {
    let current_str = read_installed_version();
    let current_ver = parse_caram_version(&current_str)?;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(12))
        .timeout_read(Duration::from_secs(20))
        .build();
    let resp = agent
        .get(GITHUB_RELEASES_API)
        .set("User-Agent", "caramos-update-center/2.0")
        .set("Accept", "application/vnd.github+json")
        .call()
        .ok()?;
    let releases: Vec<GhRelease> = resp.into_json().ok()?;

    let mut best: Option<(CaramVersion, GhRelease)> = None;
    for rel in releases {
        if rel.draft {
            continue;
        }
        let Some(ver) = parse_caram_version(&rel.tag_name) else {
            continue;
        };
        let rel_is_pre = rel.prerelease || ver.pre != PreRelease::Stable;
        if channel == UpdateChannel::Stable && rel_is_pre {
            continue;
        }
        if ver <= current_ver {
            continue;
        }
        let is_better = best.as_ref().map(|(b, _)| ver > *b).unwrap_or(true);
        if is_better {
            best = Some((ver, rel));
        }
    }

    let (best_ver, rel) = best?;
    let new_ver = rel
        .tag_name
        .trim()
        .trim_start_matches(['v', 'V'])
        .to_string();
    let is_prerelease = rel.prerelease || best_ver.pre != PreRelease::Stable;

    let ota_bundle = rel
        .assets
        .iter()
        .find(|a| a.name.starts_with("caramos-ota-bundle") && a.name.ends_with(".tar.gz"))
        .map(|a| ReleaseAssetInfo {
            name: a.name.clone(),
            download_url: a.browser_download_url.clone(),
            size_bytes: a.size,
            expected_sha256: extract_asset_sha(&agent, &rel.assets, a),
        });

    let iso_asset = rel
        .assets
        .iter()
        .find(|a| a.name.to_ascii_lowercase().ends_with(".iso"))
        .map(|a| ReleaseAssetInfo {
            name: a.name.clone(),
            download_url: a.browser_download_url.clone(),
            size_bytes: a.size,
            expected_sha256: extract_asset_sha(&agent, &rel.assets, a),
        });

    Some(CaramOsReleaseUpdate {
        current_version: current_str,
        new_version: new_ver,
        is_prerelease,
        title: if rel.name.trim().is_empty() {
            rel.tag_name
        } else {
            rel.name
        },
        notes: rel.body.trim().to_string(),
        html_url: rel.html_url,
        ota_bundle,
        iso_asset,
    })
}

fn scan_pending_updates() -> UpdateScan {
    let channel = read_current_channel();
    let caramos_release = check_github_release_update(channel);

    let mut apt_items = Vec::new();
    if let Ok(out) = Command::new("apt")
        .args(["list", "--upgradable"])
        .env("LC_ALL", "C")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.starts_with("Listing") || !line.contains('/') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let pkg = parts
                .next()
                .and_then(|p| p.split('/').next())
                .unwrap_or("")
                .trim();
            let new_ver = parts.next().unwrap_or("").trim();
            if !pkg.is_empty() {
                apt_items.push(PendingUpdate {
                    source_label: "Hệ thống Debian (APT)",
                    name: pkg.to_string(),
                    version_note: format!("Phiên bản mới: {}", new_ver),
                });
            }
        }
    }

    let mut flatpak_items = Vec::new();
    if let Ok(out) = Command::new("flatpak")
        .args([
            "remote-ls",
            "--updates",
            "--columns=application,name,version",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let cols: Vec<&str> = line.split('\t').collect();
            let app_id = cols.first().copied().unwrap_or("").trim();
            if app_id.is_empty() {
                continue;
            }
            let title = cols
                .get(1)
                .copied()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(app_id);
            let ver = cols.get(2).copied().unwrap_or("").trim();
            flatpak_items.push(PendingUpdate {
                source_label: "Ứng dụng Flathub",
                name: title.to_string(),
                version_note: if ver.is_empty() {
                    app_id.to_string()
                } else {
                    format!("{} ({})", app_id, ver)
                },
            });
        }
    }

    UpdateScan {
        channel,
        caramos_release,
        apt_items,
        flatpak_items,
    }
}

fn validate_ota_entry_path(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains("..") || trimmed.contains('\0') {
        return false;
    }
    let rel = trimmed.strip_prefix("./").unwrap_or(trimmed);
    if rel.starts_with('/') {
        return false;
    }
    if rel.is_empty() || rel == "." || rel.ends_with('/') {
        return true;
    }
    const ALLOWED_PREFIXES: &[&str] = &[
        "usr/local/bin/caramos-",
        "usr/share/backgrounds/caramos/",
        "usr/share/backgrounds/caramos-login.png",
        "usr/share/pixmaps/caramos-",
        "usr/share/applications/caramos-",
        "usr/share/cinnamon/applets/caramos-",
        "usr/share/themes/CaramOS-",
        "usr/share/caramos-app-center/",
        "usr/share/plymouth/themes/caramos/",
        "etc/dconf/db/local.d/",
        "etc/xdg/autostart/caramos-",
        "etc/calamares/branding/caramos/",
        "etc/lightdm/",
        "boot/grub/themes/caramos/",
        "etc/caramos-version",
    ];
    ALLOWED_PREFIXES
        .iter()
        .any(|prefix| rel.starts_with(prefix))
}

fn atomic_write_root_line(dest: &Path, line: &str) -> Result<(), CaramDesktopError> {
    let Some(parent) = dest.parent() else {
        return Ok(());
    };
    let mut tmp = NamedTempFile::new_in(parent).map_err(|e| CaramDesktopError::Io {
        context: format!("Tạo tệp tạm cho '{}'", dest.display()),
        source: e,
    })?;
    writeln!(tmp, "{}", line).map_err(|e| CaramDesktopError::Io {
        context: format!("Ghi '{}'", dest.display()),
        source: e,
    })?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| CaramDesktopError::Io {
            context: format!("fsync '{}'", dest.display()),
            source: e,
        })?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o644));
    }
    tmp.persist(dest).map_err(|e| CaramDesktopError::Io {
        context: format!("Lưu '{}'", dest.display()),
        source: e.error,
    })?;
    Ok(())
}

fn run_privileged_set_channel(channel: &str) -> Result<(), CaramDesktopError> {
    if !matches!(channel, "stable" | "testing") {
        return Err(CaramDesktopError::InvalidTarget {
            arg: channel.to_string(),
            reason: "Kênh cập nhật chỉ chấp nhận 'stable' hoặc 'testing'",
        });
    }
    atomic_write_root_line(Path::new(CHANNEL_FILE), channel)
}

fn run_privileged_apply_ota(bundle_path: &str, new_version: &str) -> Result<(), CaramDesktopError> {
    let valid_bundle = bundle_path.ends_with(".tar.gz")
        && (bundle_path.starts_with("/tmp/caramos-") || bundle_path.starts_with("/run/user/"))
        && !bundle_path.contains("..")
        && Path::new(bundle_path).is_file();
    if !valid_bundle {
        return Err(CaramDesktopError::InvalidTarget {
            arg: bundle_path.to_string(),
            reason: "Đường dẫn gói OTA không nằm trong vùng tải tạm hợp lệ",
        });
    }
    let valid_ver = !new_version.is_empty()
        && new_version.len() <= 40
        && new_version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !valid_ver {
        return Err(CaramDesktopError::InvalidTarget {
            arg: new_version.to_string(),
            reason: "Chuỗi phiên bản CaramOS không hợp lệ",
        });
    }

    let list_out = Command::new("tar")
        .args(["-tzf", bundle_path])
        .output()
        .map_err(|e| CaramDesktopError::SpawnFailed {
            binary: "tar".to_string(),
            source: e,
        })?;
    if !list_out.status.success() {
        return Err(CaramDesktopError::CommandFailed {
            action: "tar -tzf".to_string(),
            detail: String::from_utf8_lossy(&list_out.stderr).trim().to_string(),
        });
    }
    for entry in String::from_utf8_lossy(&list_out.stdout).lines() {
        if !validate_ota_entry_path(entry) {
            return Err(CaramDesktopError::InvalidTarget {
                arg: entry.to_string(),
                reason: "Gói OTA chứa đường dẫn ngoài vùng cho phép của CaramOS",
            });
        }
    }

    let extract_out = Command::new("tar")
        .args(["--no-same-owner", "-xzf", bundle_path, "-C", "/"])
        .output()
        .map_err(|e| CaramDesktopError::SpawnFailed {
            binary: "tar".to_string(),
            source: e,
        })?;
    if !extract_out.status.success() {
        return Err(CaramDesktopError::CommandFailed {
            action: "tar -xzf".to_string(),
            detail: String::from_utf8_lossy(&extract_out.stderr)
                .trim()
                .to_string(),
        });
    }

    for (bin, args) in [
        ("/usr/bin/dconf", &["update"][..]),
        (
            "/usr/bin/glib-compile-schemas",
            &["/usr/share/glib-2.0/schemas"][..],
        ),
        (
            "/usr/bin/update-desktop-database",
            &["/usr/share/applications"][..],
        ),
    ] {
        if Path::new(bin).is_file() {
            let _ = Command::new(bin)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    atomic_write_root_line(Path::new(VERSION_FILE), new_version)
}

fn run_privileged_upgrade() -> Result<(), CaramDesktopError> {
    let upd = Command::new("apt-get")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .args(["-o", "DPkg::Lock::Timeout=120", "update"])
        .output()
        .map_err(|e| CaramDesktopError::SpawnFailed {
            binary: "apt-get".to_string(),
            source: e,
        })?;
    if !upd.status.success() {
        eprintln!(
            "[WARN] apt-get update: {}",
            String::from_utf8_lossy(&upd.stderr).trim()
        );
    }

    let upg = Command::new("apt-get")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .args([
            "-o",
            "DPkg::Lock::Timeout=120",
            "-o",
            "Dpkg::Options::=--force-confdef",
            "-o",
            "Dpkg::Options::=--force-confold",
            "dist-upgrade",
            "-y",
        ])
        .output()
        .map_err(|e| CaramDesktopError::SpawnFailed {
            binary: "apt-get".to_string(),
            source: e,
        })?;
    if !upg.status.success() {
        return Err(CaramDesktopError::CommandFailed {
            action: "apt-get dist-upgrade".to_string(),
            detail: String::from_utf8_lossy(&upg.stderr).trim().to_string(),
        });
    }

    let _ = Command::new("apt-get")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .args(["-o", "DPkg::Lock::Timeout=120", "autoremove", "-y"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Ok(())
}

fn stream_download_verified(
    url: &str,
    dest_dir: &Path,
    file_name: &str,
    expected_sha256: Option<&str>,
) -> Result<PathBuf, CaramDesktopError> {
    fs::create_dir_all(dest_dir).map_err(|e| CaramDesktopError::Io {
        context: format!("Tạo thư mục '{}'", dest_dir.display()),
        source: e,
    })?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(900))
        .build();
    let resp = agent
        .get(url)
        .set("User-Agent", "caramos-update-center/2.0")
        .call()
        .map_err(|e| CaramDesktopError::Network {
            url: url.to_string(),
            reason: e.to_string(),
        })?;

    let mut reader = resp.into_reader();
    let mut tmp = NamedTempFile::new_in(dest_dir).map_err(|e| CaramDesktopError::Io {
        context: "Tạo tệp đệm tải xuống".to_string(),
        source: e,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65_536];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| CaramDesktopError::Network {
                url: url.to_string(),
                reason: e.to_string(),
            })?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            hasher.update(chunk);
            tmp.write_all(chunk).map_err(|e| CaramDesktopError::Io {
                context: "Ghi dữ liệu tải xuống".to_string(),
                source: e,
            })?;
        }
    }

    let actual_sha = hex::encode(hasher.finalize());
    if let Some(expected) = expected_sha256 {
        if actual_sha != expected.to_ascii_lowercase() {
            return Err(CaramDesktopError::ChecksumMismatch {
                file: file_name.to_string(),
            });
        }
    }

    tmp.as_file()
        .sync_all()
        .map_err(|e| CaramDesktopError::Io {
            context: "fsync tệp tải về".to_string(),
            source: e,
        })?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o644));
    }
    let dst = dest_dir.join(file_name);
    tmp.persist(&dst).map_err(|e| CaramDesktopError::Io {
        context: format!("Lưu '{}'", dst.display()),
        source: e.error,
    })?;
    Ok(dst)
}

fn user_downloads_dir() -> PathBuf {
    if let Ok(out) = Command::new("xdg-user-dir").arg("DOWNLOAD").output() {
        let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        if p.is_dir() {
            return p;
        }
    }
    env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else {
        format!("{} KB", (bytes / 1024).max(1))
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("--pkexec-upgrade") {
        return match run_privileged_upgrade() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::FAILURE
            }
        };
    }
    if args.get(1).map(String::as_str) == Some("--pkexec-apply-ota") {
        let bundle = args.get(2).map(String::as_str).unwrap_or("");
        let ver = args.get(3).map(String::as_str).unwrap_or("");
        return match run_privileged_apply_ota(bundle, ver) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::FAILURE
            }
        };
    }
    if args.get(1).map(String::as_str) == Some("--pkexec-set-channel") {
        let ch = args.get(2).map(String::as_str).unwrap_or("");
        return match run_privileged_set_channel(ch) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::FAILURE
            }
        };
    }

    let check_only = args.iter().any(|a| a == "--check");
    let pre_scan = if check_only {
        let scan = scan_pending_updates();
        if scan.total() == 0 {
            return ExitCode::SUCCESS;
        }
        Some(scan)
    } else {
        None
    };

    let pre_slot = Rc::new(RefCell::new(pre_scan));
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |application| {
        install_caramos_css();
        build_window(application, pre_slot.borrow_mut().take());
    });

    if app.run_with_args(&["caramos-update-center"]) == glib::ExitCode::SUCCESS {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn build_window(app: &adw::Application, initial_scan: Option<UpdateScan>) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Trung tâm Cập nhật")
        .default_width(680)
        .default_height(620)
        .build();

    let toast_overlay = adw::ToastOverlay::new();
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let init_ch = read_current_channel();
    let header = adw::HeaderBar::new();
    let header_title = adw::WindowTitle::new(
        "Trung tâm Cập nhật",
        &format!(
            "CaramOS {} • Kênh: {}",
            read_installed_version(),
            init_ch.display_label()
        ),
    );
    header.set_title_widget(Some(&header_title));

    let channel_btn = gtk4::Button::with_label(match init_ch {
        UpdateChannel::Stable => "Kênh: Stable",
        UpdateChannel::Testing => "Kênh: Testing",
    });
    channel_btn.set_tooltip_text(Some(
        "Chuyển giữa kênh Ổn định (Stable) và Thử nghiệm (Testing)",
    ));
    let refresh_btn = gtk4::Button::from_icon_name("view-refresh-symbolic");
    refresh_btn.set_tooltip_text(Some("Kiểm tra bản phát hành & cập nhật mới"));
    header.pack_start(&channel_btn);
    header.pack_end(&refresh_btn);
    root.append(&header);

    let stack = gtk4::Stack::new();
    stack.set_vexpand(true);
    stack.set_transition_type(gtk4::StackTransitionType::Crossfade);

    // Trang 1: Đang kiểm tra
    let check_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    check_box.set_valign(gtk4::Align::Center);
    check_box.set_halign(gtk4::Align::Center);
    let spin = gtk4::Spinner::new();
    spin.set_size_request(36, 36);
    spin.start();
    let check_lbl = gtk4::Label::new(Some(
        "Đang kiểm tra bản phát hành CaramOS, Debian và Flathub...",
    ));
    check_lbl.add_css_class("dim-label");
    check_box.append(&spin);
    check_box.append(&check_lbl);
    stack.add_named(&check_box, Some("checking"));

    // Trang 2: Đã mới nhất
    let up_to_date = adw::StatusPage::builder()
        .icon_name("emblem-ok-symbolic")
        .title("Hệ thống đã được cập nhật mới nhất")
        .description("CaramOS, các gói hệ thống Debian 13 và ứng dụng Flathub đều ở phiên bản mới nhất trên kênh hiện tại.")
        .build();
    let recheck_btn = gtk4::Button::with_label("Kiểm tra lại");
    recheck_btn.add_css_class("pill");
    recheck_btn.set_halign(gtk4::Align::Center);
    up_to_date.set_child(Some(&recheck_btn));
    stack.add_named(&up_to_date, Some("up-to-date"));

    // Trang 3: Có bản cập nhật
    let list_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .build();
    let clamp = adw::Clamp::builder()
        .maximum_size(640)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(24)
        .build();
    let updates_container = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    clamp.set_child(Some(&updates_container));
    scroll.set_child(Some(&clamp));
    list_vbox.append(&scroll);

    let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    footer.add_css_class("wizard-bar");
    let footer_lbl = gtk4::Label::new(None);
    footer_lbl.set_hexpand(true);
    footer_lbl.set_xalign(0.0);
    footer_lbl.add_css_class("secondary");
    let update_now_btn = gtk4::Button::with_label("Cập nhật ngay");
    update_now_btn.add_css_class("suggested-action");
    footer.append(&footer_lbl);
    footer.append(&update_now_btn);
    list_vbox.append(&footer);
    stack.add_named(&list_vbox, Some("updates"));

    // Trang 4: Đang cập nhật / tải ISO
    let prog_clamp = adw::Clamp::builder()
        .maximum_size(600)
        .margin_start(16)
        .margin_end(16)
        .margin_top(28)
        .margin_bottom(24)
        .build();
    let prog_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    let prog_title = gtk4::Label::new(Some("Đang cập nhật CaramOS"));
    prog_title.set_xalign(0.0);
    prog_title.add_css_class("page-title");
    let prog_status = gtk4::Label::new(Some("Đang chuẩn bị..."));
    prog_status.set_xalign(0.0);
    prog_status.set_wrap(true);
    let prog_bar = gtk4::ProgressBar::new();
    let log_buf = gtk4::TextBuffer::new(None);
    let log_tv = gtk4::TextView::with_buffer(&log_buf);
    log_tv.set_editable(false);
    log_tv.set_monospace(true);
    log_tv.set_wrap_mode(gtk4::WrapMode::WordChar);
    let log_sc = gtk4::ScrolledWindow::builder()
        .min_content_height(180)
        .child(&log_tv)
        .build();
    let log_exp = gtk4::Expander::new(Some("Xem chi tiết nhật ký"));
    log_exp.set_child(Some(&log_sc));
    prog_vbox.append(&prog_title);
    prog_vbox.append(&prog_status);
    prog_vbox.append(&prog_bar);
    prog_vbox.append(&log_exp);
    prog_clamp.set_child(Some(&prog_vbox));
    stack.add_named(&prog_clamp, Some("updating"));

    // Trang 5: Hoàn tất
    let done_page = adw::StatusPage::builder()
        .icon_name("emblem-ok-symbolic")
        .title("Đã hoàn tất")
        .build();
    let done_close_btn = gtk4::Button::with_label("Đóng");
    done_close_btn.add_css_class("suggested-action");
    done_close_btn.set_halign(gtk4::Align::Center);
    done_page.set_child(Some(&done_close_btn));
    stack.add_named(&done_page, Some("done"));

    root.append(&stack);
    toast_overlay.set_child(Some(&root));
    window.set_content(Some(&toast_overlay));

    let is_busy = Rc::new(Cell::new(false));
    let hold_guard: Rc<RefCell<Option<gio::ApplicationHoldGuard>>> = Rc::new(RefCell::new(None));
    let current_scan: Rc<RefCell<Option<UpdateScan>>> = Rc::new(RefCell::new(None));
    let refresh_slot: RefreshFnSlot = Rc::new(RefCell::new(None));

    let start_background_task = Rc::new({
        let app_u = app.clone();
        let win_u = window.clone();
        let stack_u = stack.clone();
        let refresh_u = refresh_btn.clone();
        let ch_btn_u = channel_btn.clone();
        let title_u = prog_title.clone();
        let status_u = prog_status.clone();
        let bar_u = prog_bar.clone();
        let log_u = log_buf.clone();
        let done_u = done_page.clone();
        let busy_u = Rc::clone(&is_busy);
        let hold_u = Rc::clone(&hold_guard);
        move |heading: &str, job: Box<dyn FnOnce(async_channel::Sender<UpgradeEvent>) + Send>| {
            if busy_u.get() {
                return;
            }
            busy_u.set(true);
            *hold_u.borrow_mut() = Some(app_u.hold());
            refresh_u.set_sensitive(false);
            ch_btn_u.set_sensitive(false);
            title_u.set_text(heading);
            log_u.set_text("");
            status_u.set_text("Đang bắt đầu...");
            stack_u.set_visible_child_name("updating");

            let busy_pulse = Rc::clone(&busy_u);
            let bar_pulse = bar_u.clone();
            glib::timeout_add_local(Duration::from_millis(120), move || {
                if busy_pulse.get() {
                    bar_pulse.pulse();
                    glib::ControlFlow::Continue
                } else {
                    glib::ControlFlow::Break
                }
            });

            let (tx, rx) = async_channel::unbounded::<UpgradeEvent>();
            thread::spawn(move || job(tx));

            let app_d = app_u.clone();
            let win_d = win_u.clone();
            let stack_d = stack_u.clone();
            let refresh_d = refresh_u.clone();
            let ch_btn_d = ch_btn_u.clone();
            let status_d = status_u.clone();
            let log_d = log_u.clone();
            let done_d = done_u.clone();
            let busy_d = Rc::clone(&busy_u);
            let hold_d = Rc::clone(&hold_u);
            glib::spawn_future_local(async move {
                while let Ok(ev) = rx.recv().await {
                    match ev {
                        UpgradeEvent::Step { headline, log } => {
                            status_d.set_text(&headline);
                            let mut end = log_d.end_iter();
                            log_d.insert(&mut end, &format!("{}\n", log));
                        }
                        UpgradeEvent::Done {
                            ok,
                            detail,
                            reboot_required,
                        } => {
                            busy_d.set(false);
                            *hold_d.borrow_mut() = None;
                            refresh_d.set_sensitive(true);
                            ch_btn_d.set_sensitive(true);
                            let title = if ok && reboot_required {
                                "Đã cập nhật xong (Nên khởi động lại)"
                            } else if ok {
                                "Hoàn tất thành công"
                            } else {
                                "Cập nhật gặp sự cố"
                            };
                            done_d.set_icon_name(Some(if ok {
                                "emblem-ok-symbolic"
                            } else {
                                "dialog-warning-symbolic"
                            }));
                            done_d.set_title(title);
                            done_d.set_description(Some(&detail));
                            stack_d.set_visible_child_name("done");

                            if !win_d.is_visible() {
                                let notif = gio::Notification::new(title);
                                notif.set_body(Some(&detail));
                                app_d.send_notification(Some("caramos-update-done"), &notif);
                            }
                        }
                    }
                }
            });
        }
    });

    let render_scan = Rc::new({
        let updates_container = updates_container.clone();
        let footer_lbl = footer_lbl.clone();
        let stack = stack.clone();
        let header_title = header_title.clone();
        let channel_btn = channel_btn.clone();
        let launcher = Rc::clone(&start_background_task);
        move |scan: &UpdateScan| {
            header_title.set_subtitle(&format!(
                "CaramOS {} • Kênh: {}",
                read_installed_version(),
                scan.channel.display_label()
            ));
            channel_btn.set_label(match scan.channel {
                UpdateChannel::Stable => "Kênh: Stable",
                UpdateChannel::Testing => "Kênh: Testing",
            });

            while let Some(c) = updates_container.first_child() {
                updates_container.remove(&c);
            }
            if scan.total() == 0 {
                stack.set_visible_child_name("up-to-date");
                return;
            }

            if let Some(ref rel) = scan.caramos_release {
                let card = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
                card.add_css_class("card");
                card.add_css_class("summary-card");

                let badge = if rel.is_prerelease {
                    "Bản thử nghiệm (Testing)"
                } else {
                    "Bản chính thức (Stable)"
                };
                let t = gtk4::Label::new(Some(&format!(
                    "{}: v{} (hiện tại: v{})",
                    badge, rel.new_version, rel.current_version
                )));
                t.set_xalign(0.0);
                t.add_css_class("row-title");
                card.append(&t);

                let sub = gtk4::Label::new(Some(&rel.title));
                sub.set_xalign(0.0);
                sub.set_wrap(true);
                card.append(&sub);

                if !rel.notes.is_empty() {
                    let notes = gtk4::Label::new(Some(&rel.notes));
                    notes.set_xalign(0.0);
                    notes.set_wrap(true);
                    notes.add_css_class("dim-label");
                    notes.add_css_class("secondary");
                    card.append(&notes);
                }

                let btn_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
                if let Some(ref iso) = rel.iso_asset {
                    let iso_btn = gtk4::Button::with_label(&format!(
                        "Tải file ISO mới ({})",
                        format_bytes(iso.size_bytes)
                    ));
                    let iso_clone = iso.clone();
                    let launch_iso = Rc::clone(&launcher);
                    iso_btn.connect_clicked(move |_| {
                        let asset = iso_clone.clone();
                        launch_iso(
                            "Đang tải ảnh ISO CaramOS mới",
                            Box::new(move |tx| {
                                let dl_dir = user_downloads_dir();
                                let _ = tx.send_blocking(UpgradeEvent::Step {
                                    headline: format!(
                                        "Đang tải {} về {}...",
                                        asset.name,
                                        dl_dir.display()
                                    ),
                                    log: format!("GET {}", asset.download_url),
                                });
                                match stream_download_verified(
                                    &asset.download_url,
                                    &dl_dir,
                                    &asset.name,
                                    asset.expected_sha256.as_deref(),
                                ) {
                                    Ok(saved) => {
                                        let _ = tx.send_blocking(UpgradeEvent::Done {
                                            ok: true,
                                            detail: format!(
                                                "Đã tải xong và xác thực SHA-256 ISO tại:\n{}",
                                                saved.display()
                                            ),
                                            reboot_required: false,
                                        });
                                    }
                                    Err(e) => {
                                        let _ = tx.send_blocking(UpgradeEvent::Done {
                                            ok: false,
                                            detail: e.to_string(),
                                            reboot_required: false,
                                        });
                                    }
                                }
                            }),
                        );
                    });
                    btn_row.append(&iso_btn);
                }
                if !rel.html_url.is_empty() {
                    let gh_btn = gtk4::Button::with_label("Xem trên GitHub");
                    let url = rel.html_url.clone();
                    gh_btn.connect_clicked(move |_| open_external_url(&url));
                    btn_row.append(&gh_btn);
                }
                card.append(&btn_row);
                updates_container.append(&card);
            }

            footer_lbl.set_text(&format!(
                "Có {} mục cập nhật ({} CaramOS, {} gói Debian, {} Flathub)",
                scan.total(),
                usize::from(scan.caramos_release.is_some()),
                scan.apt_items.len(),
                scan.flatpak_items.len()
            ));

            for (heading_text, items) in [
                ("Hệ thống Debian (APT)", &scan.apt_items),
                ("Ứng dụng Flathub", &scan.flatpak_items),
            ] {
                if items.is_empty() {
                    continue;
                }
                let h = gtk4::Label::new(Some(heading_text));
                h.set_xalign(0.0);
                h.add_css_class("section-heading");
                updates_container.append(&h);

                let list = gtk4::ListBox::new();
                list.set_selection_mode(gtk4::SelectionMode::None);
                list.add_css_class("boxed-list");
                for it in items {
                    let bx = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
                    let t = gtk4::Label::new(Some(&it.name));
                    t.set_xalign(0.0);
                    t.add_css_class("row-title");
                    let s = gtk4::Label::new(Some(&format!(
                        "{} — {}",
                        it.source_label, it.version_note
                    )));
                    s.set_xalign(0.0);
                    s.set_wrap(true);
                    s.add_css_class("dim-label");
                    s.add_css_class("secondary");
                    bx.append(&t);
                    bx.append(&s);
                    let row = gtk4::ListBoxRow::new();
                    row.add_css_class("app-row");
                    row.set_child(Some(&bx));
                    list.append(&row);
                }
                updates_container.append(&list);
            }
            stack.set_visible_child_name("updates");
        }
    });

    let trigger_refresh: Rc<dyn Fn()> = Rc::new({
        let stack = stack.clone();
        let refresh_btn = refresh_btn.clone();
        let channel_btn = channel_btn.clone();
        let current_scan = Rc::clone(&current_scan);
        let render_scan = Rc::clone(&render_scan);
        let is_busy = Rc::clone(&is_busy);
        move || {
            if is_busy.get() {
                return;
            }
            stack.set_visible_child_name("checking");
            refresh_btn.set_sensitive(false);
            channel_btn.set_sensitive(false);
            let (tx, rx) = async_channel::bounded::<UpdateScan>(1);
            thread::spawn(move || {
                let _ = tx.send_blocking(scan_pending_updates());
            });
            let btn_d = refresh_btn.clone();
            let ch_d = channel_btn.clone();
            let scan_d = Rc::clone(&current_scan);
            let render_d = Rc::clone(&render_scan);
            glib::spawn_future_local(async move {
                if let Ok(rep) = rx.recv().await {
                    render_d(&rep);
                    *scan_d.borrow_mut() = Some(rep);
                    btn_d.set_sensitive(true);
                    ch_d.set_sensitive(true);
                }
            });
        }
    });
    *refresh_slot.borrow_mut() = Some(Rc::clone(&trigger_refresh));

    {
        let r = Rc::clone(&trigger_refresh);
        refresh_btn.connect_clicked(move |_| r());
    }
    {
        let r = Rc::clone(&trigger_refresh);
        recheck_btn.connect_clicked(move |_| r());
    }
    {
        let busy_ch = Rc::clone(&is_busy);
        let toast_ch = toast_overlay.clone();
        let slot_ch = Rc::clone(&refresh_slot);
        channel_btn.connect_clicked(move |_| {
            if busy_ch.get() {
                return;
            }
            let next_ch = read_current_channel().flipped();
            let (tx, rx) = async_channel::bounded::<Result<UpdateChannel, String>>(1);
            thread::spawn(move || {
                let res =
                    invoke_self_pkexec("--pkexec-set-channel", &[next_ch.as_str().to_string()])
                        .map(|()| next_ch)
                        .map_err(|e| e.to_string());
                let _ = tx.send_blocking(res);
            });
            let toast_d = toast_ch.clone();
            let slot_d = Rc::clone(&slot_ch);
            glib::spawn_future_local(async move {
                if let Ok(res) = rx.recv().await {
                    match res {
                        Ok(ch) => {
                            toast_d.add_toast(adw::Toast::new(&format!(
                                "Đã chuyển sang kênh {}",
                                ch.display_label()
                            )));
                            if let Some(ref f) = *slot_d.borrow() {
                                f();
                            }
                        }
                        Err(e) => toast_d.add_toast(adw::Toast::new(&e)),
                    }
                }
            });
        });
    }
    {
        let w = window.clone();
        done_close_btn.connect_clicked(move |_| w.close());
    }
    {
        let busy_c = Rc::clone(&is_busy);
        let toast_c = toast_overlay.clone();
        window.connect_close_request(move |win| {
            if busy_c.get() {
                win.set_visible(false);
                toast_c.add_toast(adw::Toast::new("Đang tiếp tục tác vụ ở chế độ nền"));
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    {
        let scan_u = Rc::clone(&current_scan);
        let launch_u = Rc::clone(&start_background_task);
        update_now_btn.connect_clicked(move |_| {
            let snap = scan_u.borrow().clone();
            launch_u(
                "Đang cập nhật CaramOS",
                Box::new(move |tx| {
                    let mut errors: Vec<String> = Vec::new();
                    let mut ota_applied = false;

                    if let Some(ref s) = snap {
                        if let Some(ref rel) = s.caramos_release {
                            if let Some(ref bundle) = rel.ota_bundle {
                                let _ = tx.send_blocking(UpgradeEvent::Step {
                                    headline: format!("Đang tải gói nâng cấp CaramOS v{}...", rel.new_version),
                                    log: format!("GET {}", bundle.download_url),
                                });
                                let tmp_dir = PathBuf::from("/tmp/caramos-ota-stage");
                                let fname = format!("caramos-ota-{}.tar.gz", rel.new_version);
                                match stream_download_verified(
                                    &bundle.download_url,
                                    &tmp_dir,
                                    &fname,
                                    bundle.expected_sha256.as_deref(),
                                ) {
                                    Ok(path) => {
                                        let _ = tx.send_blocking(UpgradeEvent::Step {
                                            headline: format!("Đang áp dụng bản cập nhật CaramOS v{}...", rel.new_version),
                                            log: format!("pkexec --pkexec-apply-ota {}", path.display()),
                                        });
                                        let args = vec![path.display().to_string(), rel.new_version.clone()];
                                        match invoke_self_pkexec("--pkexec-apply-ota", &args) {
                                            Ok(()) => ota_applied = true,
                                            Err(e) => errors.push(e.to_string()),
                                        }
                                        let _ = fs::remove_file(path);
                                    }
                                    Err(e) => errors.push(e.to_string()),
                                }
                            }
                        }

                        if !s.apt_items.is_empty() {
                            let _ = tx.send_blocking(UpgradeEvent::Step {
                                headline: "Đang nâng cấp các gói hệ thống Debian (APT)...".to_string(),
                                log: "pkexec caramos-update-center --pkexec-upgrade".to_string(),
                            });
                            if let Err(e) = invoke_self_pkexec("--pkexec-upgrade", &[]) {
                                errors.push(e.to_string());
                            }
                        }

                        if !s.flatpak_items.is_empty() {
                            let _ = tx.send_blocking(UpgradeEvent::Step {
                                headline: "Đang cập nhật ứng dụng Flathub...".to_string(),
                                log: "flatpak update -y --noninteractive --user".to_string(),
                            });
                            if let Ok(out) = Command::new("flatpak")
                                .args(["update", "-y", "--noninteractive", "--user"])
                                .output()
                            {
                                if !out.status.success() {
                                    errors.push(String::from_utf8_lossy(&out.stderr).trim().to_string());
                                }
                            }
                        }
                    }

                    let reboot_required = ota_applied || Path::new("/var/run/reboot-required").exists();
                    let _ = tx.send_blocking(UpgradeEvent::Done {
                        ok: errors.is_empty(),
                        detail: if errors.is_empty() {
                            if reboot_required {
                                "Cập nhật thành công. Nên đăng xuất hoặc khởi động lại máy để áp dụng đầy đủ phiên bản mới."
                                    .to_string()
                            } else {
                                "Toàn bộ hệ thống và ứng dụng đã ở phiên bản mới nhất.".to_string()
                            }
                        } else {
                            errors.join("\n")
                        },
                        reboot_required,
                    });
                }),
            );
        });
    }

    if let Some(rep) = initial_scan {
        render_scan(&rep);
        *current_scan.borrow_mut() = Some(rep);
    } else {
        trigger_refresh();
    }
    window.present();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn semver_orders_testing_rc_and_stable_strictly() {
        assert!(is_newer_version("2.0.0", "v2.0.1-testing.1"));
        assert!(is_newer_version("2.0.1-testing.1", "v2.0.1-testing.2"));
        assert!(is_newer_version("2.0.1-testing.2", "v2.0.1-rc.1"));
        assert!(is_newer_version("2.0.1-rc.1", "v2.0.1"));
        assert!(!is_newer_version("2.0.1", "v2.0.1-testing.9"));
        assert!(!is_newer_version("2.0.1", "v2.0.1"));
    }

    #[test]
    fn ota_path_validator_blocks_traversal_and_system_binaries() {
        assert!(validate_ota_entry_path(
            "./usr/local/bin/caramos-app-center"
        ));
        assert!(validate_ota_entry_path(
            "usr/share/cinnamon/applets/caramos-control-center@caramos/applet.js"
        ));
        assert!(validate_ota_entry_path("./usr/local/bin/"));
        assert!(!validate_ota_entry_path("../etc/shadow"));
        assert!(!validate_ota_entry_path("/etc/passwd"));
        assert!(!validate_ota_entry_path("usr/bin/sudo"));
    }
}

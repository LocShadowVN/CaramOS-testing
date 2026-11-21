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

//! Backend Rust thuần & bộ nhận diện CSS CaramOS (Sage Mist / Indigo Night + Be Vietnam Pro)
//! cho `caramos-app-center` và `caramos-driver-check`.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::NamedTempFile;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

// ============================================================================
// 1. PHONG CÁCH GIAO DIỆN CARAMOS (BE VIETNAM PRO + THẺ BO GÓC + PILL BADGE)
// ============================================================================

pub const CARAMOS_CSS: &str = r#"
window, .background {
    font-family: "Be Vietnam Pro", "Noto Sans", sans-serif;
}
.caramos-hero {
    border-radius: 14px;
    padding: 16px 20px;
    background-color: alpha(@accent_bg_color, 0.10);
    border: 1px solid alpha(@accent_bg_color, 0.28);
}
.caramos-hero-ok {
    border-radius: 14px;
    padding: 16px 20px;
    background-color: alpha(@success_bg_color, 0.12);
    border: 1px solid alpha(@success_bg_color, 0.35);
}
.caramos-hero-warn {
    border-radius: 14px;
    padding: 16px 20px;
    background-color: alpha(@warning_bg_color, 0.12);
    border: 1px solid alpha(@warning_bg_color, 0.38);
}
.caramos-pill-rec {
    border-radius: 999px;
    padding: 2px 10px;
    font-size: 11px;
    font-weight: 700;
    background-color: alpha(@accent_bg_color, 0.16);
    color: @accent_color;
}
.caramos-pill-ok {
    border-radius: 999px;
    padding: 2px 10px;
    font-size: 11px;
    font-weight: 700;
    background-color: alpha(@success_bg_color, 0.16);
    color: @success_color;
}
.caramos-pill-warn {
    border-radius: 999px;
    padding: 2px 10px;
    font-size: 11px;
    font-weight: 700;
    background-color: alpha(@warning_bg_color, 0.18);
    color: @warning_color;
}
.caramos-pill-err {
    border-radius: 999px;
    padding: 2px 10px;
    font-size: 11px;
    font-weight: 700;
    background-color: alpha(@error_bg_color, 0.18);
    color: @error_color;
}
"#;

pub fn install_caramos_css() {
    if let Some(display) = gtk4::gdk::Display::default() {
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(CARAMOS_CSS);
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

// ============================================================================
// 2. DANH MỤC ỨNG DỤNG CARAMOS APP CENTER (4 NHÓM CHUẨN CARAMOS GỐC)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppCategory {
    ChatAndMeetings,
    Office,
    Browsers,
    EverydayTools,
}

impl AppCategory {
    pub const ALL: [Self; 4] = [
        Self::ChatAndMeetings,
        Self::Office,
        Self::Browsers,
        Self::EverydayTools,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::ChatAndMeetings => "Liên lạc và Họp trực tuyến",
            Self::Office => "Bộ văn phòng",
            Self::Browsers => "Trình duyệt web",
            Self::EverydayTools => "Tiện ích hằng ngày",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Self::ChatAndMeetings => "Nhắn tin, gọi thoại, gọi video và làm việc nhóm",
            Self::Office => "Soạn thảo văn bản Word, bảng tính Excel và trình chiếu PowerPoint",
            Self::Browsers => "Duyệt web nhanh, đồng bộ dữ liệu và bảo vệ quyền riêng tư",
            Self::EverydayTools => "Phông chữ văn phòng, xem phim, quay màn hình, đồ họa và email",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSource {
    Apt {
        packages: &'static [&'static str],
        check_pkg: &'static str,
    },
    Flatpak {
        app_id: &'static str,
    },
    GithubDeb {
        repo: &'static str,
        check_pkg: &'static str,
        remove_pkg: &'static str,
    },
}

impl InstallSource {
    pub fn source_label(self) -> &'static str {
        match self {
            Self::Apt { .. } => "Debian APT",
            Self::Flatpak { .. } => "Flathub",
            Self::GithubDeb { .. } => "GitHub .deb",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AppEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub category: AppCategory,
    pub summary: &'static str,
    pub pros_cons: &'static str,
    pub size_mb: u32,
    pub recommended: bool,
    pub source: InstallSource,
}

pub const CARAMOS_APP_CATALOG: &[AppEntry] = &[
    // --- Liên lạc và Họp trực tuyến ---
    AppEntry {
        id: "zalo",
        name: "Zalo Desktop",
        category: AppCategory::ChatAndMeetings,
        summary: "Gọi thoại, gọi video và nhắn tin tiếng Việt mượt mà (bản native VNLF)",
        pros_cons: "Ưu: Không cần Wine, gõ Fcitx5 Lotus chuẩn • Nhược: Bản cộng đồng VNLF",
        size_mb: 115,
        recommended: true,
        source: InstallSource::GithubDeb {
            repo: "VN-Linux-Family/Zalo-for-linux",
            check_pkg: "zalo-desktop",
            remove_pkg: "zalo-desktop",
        },
    },
    AppEntry {
        id: "telegram",
        name: "Telegram Desktop",
        category: AppCategory::ChatAndMeetings,
        summary: "Nhắn tin tốc độ cao, gửi tệp lớn lên đến 2 GB",
        pros_cons: "Ưu: Nhẹ, đồng bộ tức thì • Nhược: Cài qua Flathub runtime",
        size_mb: 120,
        recommended: false,
        source: InstallSource::Flatpak {
            app_id: "org.telegram.desktop",
        },
    },
    AppEntry {
        id: "zoom",
        name: "Zoom Workplace",
        category: AppCategory::ChatAndMeetings,
        summary: "Họp trực tuyến, học online và chia sẻ màn hình phổ biến",
        pros_cons: "Ưu: Tương thích rộng trong công việc • Nhược: Nguồn đóng",
        size_mb: 190,
        recommended: false,
        source: InstallSource::Flatpak {
            app_id: "us.zoom.Zoom",
        },
    },
    AppEntry {
        id: "teams",
        name: "Teams for Linux",
        category: AppCategory::ChatAndMeetings,
        summary: "Ứng dụng họp và làm việc nhóm Microsoft Teams cho Linux",
        pros_cons: "Ưu: Thông báo desktop ổn định • Nhược: Tốn RAM mức trung bình",
        size_mb: 130,
        recommended: false,
        source: InstallSource::Flatpak {
            app_id: "com.github.IsmaelMartinez.teams_for_linux",
        },
    },
    AppEntry {
        id: "discord",
        name: "Discord",
        category: AppCategory::ChatAndMeetings,
        summary: "Trò chuyện thoại, cộng đồng và chia sẻ màn hình",
        pros_cons: "Ưu: Tự cập nhật gọn trên Flathub • Nhược: Cần mạng ổn định",
        size_mb: 150,
        recommended: false,
        source: InstallSource::Flatpak {
            app_id: "com.discordapp.Discord",
        },
    },
    // --- Bộ văn phòng ---
    AppEntry {
        id: "onlyoffice",
        name: "ONLYOFFICE Desktop Editors",
        category: AppCategory::Office,
        summary: "Word, Excel, PowerPoint giao diện hiện đại giống Microsoft Office",
        pros_cons: "Ưu: Ít lệch định dạng .docx/.xlsx nhất • Nhược: Dung lượng tải lớn",
        size_mb: 480,
        recommended: true,
        source: InstallSource::Flatpak {
            app_id: "org.onlyoffice.desktopeditors",
        },
    },
    AppEntry {
        id: "libreoffice",
        name: "LibreOffice (Tiếng Việt)",
        category: AppCategory::Office,
        summary: "Bộ văn phòng mã nguồn mở đầy đủ giao diện tiếng Việt",
        pros_cons: "Ưu: Nhẹ, cài trực tiếp từ kho Debian • Nhược: Giao diện truyền thống",
        size_mb: 320,
        recommended: false,
        source: InstallSource::Apt {
            packages: &[
                "libreoffice-writer",
                "libreoffice-calc",
                "libreoffice-impress",
                "libreoffice-gtk3",
                "libreoffice-l10n-vi",
            ],
            check_pkg: "libreoffice-writer",
        },
    },
    AppEntry {
        id: "wps-office",
        name: "WPS Office",
        category: AppCategory::Office,
        summary: "Bộ văn phòng giao diện Ribbon quen thuộc từ Windows",
        pros_cons: "Ưu: Mở tài liệu nhanh • Nhược: Phần mềm nguồn đóng",
        size_mb: 450,
        recommended: false,
        source: InstallSource::Flatpak {
            app_id: "com.wps.Office",
        },
    },
    // --- Trình duyệt web ---
    AppEntry {
        id: "firefox",
        name: "Firefox ESR (Tiếng Việt)",
        category: AppCategory::Browsers,
        summary: "Trình duyệt ổn định, bảo vệ riêng tư, cài sẵn cùng CaramOS",
        pros_cons: "Ưu: Nhẹ, cập nhật bảo mật Debian • Nhược: Một số web tối ưu theo Chromium",
        size_mb: 110,
        recommended: true,
        source: InstallSource::Apt {
            packages: &["firefox-esr", "firefox-esr-l10n-vi"],
            check_pkg: "firefox-esr",
        },
    },
    AppEntry {
        id: "chrome",
        name: "Google Chrome",
        category: AppCategory::Browsers,
        summary: "Trình duyệt phổ biến nhất, đồng bộ tài khoản Google mượt mà",
        pros_cons: "Ưu: Tương thích tối đa mọi trang web • Nhược: Tốn RAM hơn khi mở nhiều tab",
        size_mb: 170,
        recommended: false,
        source: InstallSource::Flatpak {
            app_id: "com.google.Chrome",
        },
    },
    AppEntry {
        id: "brave",
        name: "Brave Browser",
        category: AppCategory::Browsers,
        summary: "Nhân Chromium tích hợp sẵn chặn quảng cáo và trình theo dõi",
        pros_cons: "Ưu: Tải trang nhanh, gọn • Nhược: Không đồng bộ tài khoản Google",
        size_mb: 180,
        recommended: false,
        source: InstallSource::Flatpak {
            app_id: "com.brave.Browser",
        },
    },
    AppEntry {
        id: "edge",
        name: "Microsoft Edge",
        category: AppCategory::Browsers,
        summary: "Đồng bộ tốt với hệ sinh thái Microsoft 365 và tài khoản công việc",
        pros_cons: "Ưu: Quen thuộc từ Windows • Nhược: Nguồn đóng",
        size_mb: 190,
        recommended: false,
        source: InstallSource::Flatpak {
            app_id: "com.microsoft.Edge",
        },
    },
    // --- Tiện ích hằng ngày ---
    AppEntry {
        id: "ms-compat-fonts",
        name: "Font văn phòng chuẩn Microsoft (Liberation / Carlito)",
        category: AppCategory::EverydayTools,
        summary: "Thay thế khớp kích thước Times New Roman, Arial và Calibri",
        pros_cons: "Ưu: Mở tài liệu Word không nhảy trang • Nhược: Không kèm Wingdings",
        size_mb: 8,
        recommended: true,
        source: InstallSource::Apt {
            packages: &["fonts-liberation", "fonts-crosextra-carlito"],
            check_pkg: "fonts-crosextra-carlito",
        },
    },
    AppEntry {
        id: "vlc",
        name: "VLC Media Player",
        category: AppCategory::EverydayTools,
        summary: "Phát mọi định dạng video và âm thanh không cần cài thêm codec",
        pros_cons: "Ưu: Rất nhẹ, hỗ trợ phụ đề tiếng Việt tốt • Nhược: Giao diện đơn giản",
        size_mb: 45,
        recommended: true,
        source: InstallSource::Apt {
            packages: &["vlc"],
            check_pkg: "vlc",
        },
    },
    AppEntry {
        id: "obs-studio",
        name: "OBS Studio",
        category: AppCategory::EverydayTools,
        summary: "Quay màn hình bài giảng, hướng dẫn và phát trực tiếp chất lượng cao",
        pros_cons: "Ưu: Hỗ trợ PipeWire mượt mà • Nhược: Cần GPU hỗ trợ OpenGL",
        size_mb: 140,
        recommended: false,
        source: InstallSource::Apt {
            packages: &["obs-studio"],
            check_pkg: "obs-studio",
        },
    },
    AppEntry {
        id: "rustdesk",
        name: "RustDesk",
        category: AppCategory::EverydayTools,
        summary: "Điều khiển máy tính từ xa mã nguồn mở thay TeamViewer/UltraViewer",
        pros_cons: "Ưu: Viết bằng Rust, không giới hạn phiên • Nhược: Cài dạng Flatpak",
        size_mb: 45,
        recommended: false,
        source: InstallSource::Flatpak {
            app_id: "com.rustdesk.RustDesk",
        },
    },
    AppEntry {
        id: "gimp",
        name: "GIMP",
        category: AppCategory::EverydayTools,
        summary: "Chỉnh sửa ảnh chuyên sâu miễn phí thay thế Photoshop",
        pros_cons: "Ưu: Đầy đủ layer, bộ lọc, xuất PSD • Nhược: Cần làm quen bố cục",
        size_mb: 260,
        recommended: false,
        source: InstallSource::Flatpak {
            app_id: "org.gimp.GIMP",
        },
    },
    AppEntry {
        id: "thunderbird",
        name: "Thunderbird (Tiếng Việt)",
        category: AppCategory::EverydayTools,
        summary: "Quản lý nhiều hộp thư Email, Lịch và Danh bạ cục bộ",
        pros_cons: "Ưu: Hỗ trợ Gmail/Outlook/IMAP ổn định • Nhược: Lưu thư offline chiếm đĩa",
        size_mb: 110,
        recommended: false,
        source: InstallSource::Apt {
            packages: &["thunderbird", "thunderbird-l10n-vi"],
            check_pkg: "thunderbird",
        },
    },
];

// ============================================================================
// 3. KIỂM ĐỊNH GÓI, TẢI GITHUB `.deb` & CHẠY ĐẶC QUYỀN AN TOÀN (ZERO `sh -c`)
// ============================================================================

pub fn is_deb_pkg_installed(pkg: &str) -> bool {
    Command::new("dpkg-query")
        .args(["-W", "-f=${Status}", pkg])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).contains("install ok installed")
        })
        .unwrap_or(false)
}

pub fn is_flatpak_installed(app_id: &str) -> bool {
    Command::new("flatpak")
        .args(["info", app_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn is_app_installed(app: &AppEntry) -> bool {
    match app.source {
        InstallSource::Apt { check_pkg, .. } | InstallSource::GithubDeb { check_pkg, .. } => {
            is_deb_pkg_installed(check_pkg)
        }
        InstallSource::Flatpak { app_id } => is_flatpak_installed(app_id),
    }
}

pub fn validate_apt_target(arg: &str) -> Result<(), String> {
    if arg.is_empty() || arg.len() > 160 || arg.contains("..") || arg.contains('\0') {
        return Err(format!("Tham số gói '{}' không hợp lệ", arg));
    }
    if arg.starts_with("/tmp/caramos-") && arg.ends_with(".deb") {
        if Path::new(arg).is_file() {
            return Ok(());
        }
        return Err(format!("Không tìm thấy tệp gói cục bộ '{}'", arg));
    }
    let valid_pkg = arg
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '+'))
        && arg
            .chars()
            .next()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false);
    if valid_pkg {
        Ok(())
    } else {
        Err(format!("Tên gói APT '{}' bị từ chối vì lý do bảo mật", arg))
    }
}

pub fn run_privileged_apt_install(targets: &[String]) -> Result<(), String> {
    if targets.is_empty() {
        return Ok(());
    }
    for t in targets {
        validate_apt_target(t)?;
    }

    let update_status = Command::new("apt-get")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .arg("update")
        .status()
        .map_err(|e| format!("Không chạy được apt-get update: {}", e))?;
    if !update_status.success() {
        return Err("Cập nhật danh sách kho APT thất bại (kiểm tra kết nối Internet)".to_string());
    }

    let mut cmd = Command::new("apt-get");
    cmd.env("DEBIAN_FRONTEND", "noninteractive")
        .args(["install", "-y", "--no-install-recommends"]);
    for t in targets {
        cmd.arg(t);
    }
    let status = cmd
        .status()
        .map_err(|e| format!("Không chạy được apt-get install: {}", e))?;
    if !status.success() {
        return Err("Cài đặt gói APT/.deb thất bại".to_string());
    }
    Ok(())
}

pub fn run_privileged_apt_remove(packages: &[String]) -> Result<(), String> {
    if packages.is_empty() {
        return Ok(());
    }
    for p in packages {
        if p.starts_with('/') {
            return Err("Không chấp nhận đường dẫn khi gỡ gói APT".to_string());
        }
        validate_apt_target(p)?;
    }

    let mut cmd = Command::new("apt-get");
    cmd.env("DEBIAN_FRONTEND", "noninteractive")
        .args(["remove", "-y"]);
    for p in packages {
        cmd.arg(p);
    }
    let status = cmd
        .status()
        .map_err(|e| format!("Không chạy được apt-get remove: {}", e))?;
    if !status.success() {
        return Err("Gỡ gói APT thất bại".to_string());
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

pub fn select_amd64_deb_url(assets_raw: &[(&str, &str)]) -> Option<String> {
    assets_raw
        .iter()
        .find(|(name, _)| {
            let lower = name.to_ascii_lowercase();
            lower.ends_with(".deb")
                && !lower.contains("dbgsym")
                && (lower.contains("amd64") || lower.contains("x86_64"))
        })
        .map(|(_, url)| (*url).to_string())
}

pub fn download_github_deb_to_tmp(repo: &str, app_id: &str) -> Result<PathBuf, String> {
    let api_url = format!("https://api.github.com/repos/{}/releases/latest", repo);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(300))
        .build();

    let release: GithubRelease = agent
        .get(&api_url)
        .set("User-Agent", "caramos-app-center/2.0")
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("Không kết nối được GitHub ({}): {}", repo, e))?
        .into_json()
        .map_err(|e| format!("Dữ liệu GitHub Releases không hợp lệ: {}", e))?;

    let asset = release
        .assets
        .iter()
        .find(|a| {
            let lower = a.name.to_ascii_lowercase();
            lower.ends_with(".deb")
                && !lower.contains("dbgsym")
                && (lower.contains("amd64") || lower.contains("x86_64"))
        })
        .ok_or_else(|| {
            format!(
                "Không tìm thấy tệp .deb amd64 trong bản phát hành {} của {}",
                release.tag_name, repo
            )
        })?;

    let resp = agent
        .get(&asset.browser_download_url)
        .set("User-Agent", "caramos-app-center/2.0")
        .call()
        .map_err(|e| format!("Tải {} thất bại: {}", asset.name, e))?;

    let mut reader = resp.into_reader();
    let dst_path = PathBuf::from(format!("/tmp/caramos-{}.deb", app_id));
    let mut tmp = NamedTempFile::new_in("/tmp")
        .map_err(|e| format!("Không tạo được tệp tạm trong /tmp: {}", e))?;

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65_536];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("Lỗi đọc luồng tải {}: {}", asset.name, e))?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            hasher.update(chunk);
            tmp.write_all(chunk)
                .map_err(|e| format!("Lỗi ghi tệp tạm {}: {}", asset.name, e))?;
        }
    }

    let actual_sha = hex::encode(hasher.finalize());
    if let Some(ref digest_str) = asset.digest {
        if let Some(expected) = digest_str.trim().strip_prefix("sha256:") {
            if actual_sha != expected.to_ascii_lowercase() {
                return Err(format!(
                    "Sai lệch chữ ký SHA-256 cho {} (tệp tải có thể bị hỏng)",
                    asset.name
                ));
            }
        }
    }

    tmp.as_file()
        .sync_all()
        .map_err(|e| format!("Lỗi đồng bộ đĩa: {}", e))?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o644));
    }

    tmp.persist(&dst_path)
        .map_err(|e| format!("Không lưu được {}: {}", dst_path.display(), e.error))?;
    Ok(dst_path)
}

pub fn ensure_flathub_remote() {
    let _ = Command::new("flatpak")
        .args([
            "remote-add",
            "--if-not-exists",
            "--user",
            "flathub",
            "https://dl.flathub.org/repo/flathub.flatpakrepo",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn install_flatpak_app(app_id: &str) -> Result<(), String> {
    ensure_flathub_remote();
    let status = Command::new("flatpak")
        .args(["install", "-y", "--noninteractive", "--user", "flathub", app_id])
        .status()
        .map_err(|e| format!("Không chạy được flatpak cho {}: {}", app_id, e))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "Cài Flatpak '{}' thất bại (kiểm tra kết nối Flathub)",
            app_id
        ))
    }
}

pub fn remove_flatpak_app(app_id: &str) -> Result<(), String> {
    let user_ok = Command::new("flatpak")
        .args(["uninstall", "-y", "--noninteractive", "--user", app_id])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if user_ok {
        return Ok(());
    }
    let sys_ok = Command::new("flatpak")
        .args(["uninstall", "-y", "--noninteractive", app_id])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if sys_ok {
        Ok(())
    } else {
        Err(format!("Không thể gỡ Flatpak '{}'", app_id))
    }
}

pub fn invoke_self_pkexec(flag: &str, args: &[String]) -> Result<(), String> {
    let exe =
        env::current_exe().map_err(|e| format!("Không xác định được binary hiện tại: {}", e))?;
    let mut cmd = Command::new("pkexec");
    cmd.arg(exe).arg(flag);
    for a in args {
        cmd.arg(a);
    }
    let status = cmd
        .status()
        .map_err(|e| format!("Không khởi chạy được pkexec (cần gói pkexec): {}", e))?;
    if status.success() {
        Ok(())
    } else {
        Err("Thao tác quản trị bị hủy hoặc thất bại".to_string())
    }
}

// ============================================================================
// 4. BỘ QUÉT PHẦN CỨNG & CHẨN ĐOÁN DRIVER CARAMOS (`caramos-driver-check`)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareCategory {
    Wifi,
    Graphics,
    Audio,
    Printer,
}

impl HardwareCategory {
    pub const ALL: [Self; 4] = [Self::Wifi, Self::Graphics, Self::Audio, Self::Printer];

    pub fn title(self) -> &'static str {
        match self {
            Self::Wifi => "Wi-Fi và Mạng không dây",
            Self::Graphics => "Đồ họa (GPU)",
            Self::Audio => "Âm thanh (PipeWire / ALSA)",
            Self::Printer => "Máy in (CUPS)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverStatus {
    Working,
    MissingDriver,
    MissingFirmware,
    Blocked,
    NeedsReboot,
}

impl DriverStatus {
    pub fn badge_text(self) -> &'static str {
        match self {
            Self::Working => "Hoạt động tốt",
            Self::MissingDriver => "Chưa có driver",
            Self::MissingFirmware => "Thiếu firmware",
            Self::Blocked => "Bị chặn (Blacklist / Secure Boot)",
            Self::NeedsReboot => "Chờ khởi động lại",
        }
    }

    pub fn is_issue(self) -> bool {
        !matches!(self, Self::Working)
    }
}

#[derive(Debug, Clone)]
pub struct HardwareFinding {
    pub category: HardwareCategory,
    pub device_name: String,
    pub driver_in_use: Option<String>,
    pub status: DriverStatus,
    pub explanation: String,
    pub recommended_packages: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DriverScanReport {
    pub secure_boot_enabled: bool,
    pub findings: Vec<HardwareFinding>,
    pub firmware_errors: Vec<String>,
}

impl DriverScanReport {
    pub fn issue_count(&self) -> usize {
        self.findings.iter().filter(|f| f.status.is_issue()).count()
    }

    pub fn all_recommended_packages(&self) -> Vec<String> {
        let mut set = BTreeSet::new();
        for f in &self.findings {
            if f.status.is_issue() {
                for p in &f.recommended_packages {
                    set.insert(p.clone());
                }
            }
        }
        set.into_iter().collect()
    }

    pub fn fingerprint(&self) -> String {
        let mut lines: Vec<String> = self
            .findings
            .iter()
            .map(|f| format!("{:?}|{}|{:?}", f.category, f.device_name, f.status))
            .collect();
        lines.sort();
        let joined = lines.join("\n");
        let digest = Sha256::digest(joined.as_bytes());
        hex::encode(digest)
    }

    pub fn format_text_report(&self) -> String {
        let mut out = String::from("=== CARAMOS 2.0 HARDWARE & DRIVER REPORT ===\n");
        out.push_str(&format!(
            "Secure Boot: {}\nTổng vấn đề phát hiện: {}\n\n",
            if self.secure_boot_enabled {
                "BẬT"
            } else {
                "TẮT / Không xác định"
            },
            self.issue_count()
        ));
        for f in &self.findings {
            out.push_str(&format!(
                "[{:?}] {} | Trạng thái: {} | Driver: {}\n  Giải thích: {}\n",
                f.category,
                f.device_name,
                f.status.badge_text(),
                f.driver_in_use.as_deref().unwrap_or("Chưa nạp"),
                f.explanation
            ));
            if !f.recommended_packages.is_empty() {
                out.push_str(&format!(
                    "  Gói đề xuất: {}\n",
                    f.recommended_packages.join(", ")
                ));
            }
        }
        if !self.firmware_errors.is_empty() {
            out.push_str("\n--- Cảnh báo Firmware từ Kernel ---\n");
            for err in &self.firmware_errors {
                out.push_str(err);
                out.push('\n');
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct PciDeviceInfo {
    pub slot: String,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u32,
    pub driver: Option<String>,
    pub human_name: String,
}

fn parse_hex_u32(text: &str) -> Option<u32> {
    let trimmed = text.trim();
    let hex_str = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    u32::from_str_radix(hex_str, 16).ok()
}

pub fn detect_secure_boot_enabled() -> bool {
    let dir = Path::new("/sys/firmware/efi/efivars");
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("SecureBoot-") {
            if let Ok(bytes) = fs::read(entry.path()) {
                return bytes.last().copied() == Some(1);
            }
        }
    }
    false
}

fn collect_blacklisted_modules() -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    let Ok(entries) = fs::read_dir("/etc/modprobe.d") else {
        return set;
    };
    for entry in entries.flatten() {
        if let Ok(text) = fs::read_to_string(entry.path()) {
            for line in text.lines() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix("blacklist ") {
                    if let Some(module) = rest.split_whitespace().next() {
                        set.insert(module.to_string());
                    }
                }
            }
        }
    }
    set
}

fn collect_kernel_firmware_errors() -> Vec<String> {
    let output = Command::new("journalctl")
        .args(["-k", "-b", "--no-pager", "-q"])
        .output()
        .or_else(|_| Command::new("dmesg").output());

    let Ok(out) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut errors = Vec::new();
    for line in text.lines() {
        if line.contains("Direct firmware load for") && line.contains("failed") {
            errors.push(line.trim().to_string());
            if errors.len() >= 10 {
                break;
            }
        }
    }
    errors
}

fn lspci_name_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(out) = Command::new("lspci").args(["-D", "-mm"]).output() else {
        return map;
    };
    if !out.status.success() {
        return map;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some((slot, rest)) = line.split_once(' ') {
            let quoted: Vec<&str> = rest.split('"').collect();
            let vendor = quoted.get(3).copied().unwrap_or("");
            let device = quoted.get(5).copied().unwrap_or("");
            let full = format!("{} {}", vendor, device).trim().to_string();
            if !full.is_empty() {
                map.insert(slot.to_string(), full);
            }
        }
    }
    map
}

fn read_sysfs_pci_devices() -> Vec<PciDeviceInfo> {
    let names = lspci_name_map();
    let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") else {
        return Vec::new();
    };

    let mut list = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let slot = entry.file_name().to_string_lossy().to_string();
        let Some(vendor) = fs::read_to_string(path.join("vendor"))
            .ok()
            .and_then(|s| parse_hex_u32(&s))
            .and_then(|v| u16::try_from(v).ok())
        else {
            continue;
        };
        let Some(device) = fs::read_to_string(path.join("device"))
            .ok()
            .and_then(|s| parse_hex_u32(&s))
            .and_then(|v| u16::try_from(v).ok())
        else {
            continue;
        };
        let Some(class_code) = fs::read_to_string(path.join("class"))
            .ok()
            .and_then(|s| parse_hex_u32(&s))
        else {
            continue;
        };

        let driver = fs::read_link(path.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));

        let human_name = names
            .get(&slot)
            .cloned()
            .unwrap_or_else(|| format!("Thiết bị PCI {:04x}:{:04x} ({})", vendor, device, slot));

        list.push(PciDeviceInfo {
            slot,
            vendor_id: vendor,
            device_id: device,
            class_code,
            driver,
            human_name,
        });
    }
    list.sort_by(|a, b| a.slot.cmp(&b.slot));
    list
}

pub fn classify_pci_device(
    dev: &PciDeviceInfo,
    secure_boot: bool,
    blacklisted: &BTreeSet<String>,
    firmware_errors: &[String],
    pkg_installed_fn: &dyn Fn(&str) -> bool,
) -> Option<HardwareFinding> {
    let class_hi = dev.class_code >> 8;

    let category = match class_hi {
        0x0280 => HardwareCategory::Wifi,
        0x0300 | 0x0302 => HardwareCategory::Graphics,
        0x0403 | 0x0401 => HardwareCategory::Audio,
        _ => return None,
    };

    let (recommended_pkgs, expected_module): (Vec<&'static str>, Option<&'static str>) =
        match (category, dev.vendor_id) {
            (HardwareCategory::Wifi, 0x8086) => (vec!["firmware-iwlwifi"], Some("iwlwifi")),
            (HardwareCategory::Wifi, 0x10ec) => (vec!["firmware-realtek"], Some("rtw89_pci")),
            (HardwareCategory::Wifi, 0x14e4) => (vec!["broadcom-sta-dkms"], Some("wl")),
            (HardwareCategory::Wifi, 0x14c3 | 0x168c | 0x17cb) => {
                (vec!["firmware-misc-nonfree"], None)
            }
            (HardwareCategory::Graphics, 0x10de) => (vec!["nvidia-driver"], Some("nvidia")),
            (HardwareCategory::Graphics, 0x1002) => (vec!["firmware-amd-graphics"], Some("amdgpu")),
            (HardwareCategory::Graphics, 0x8086) => (vec!["firmware-intel-graphics"], Some("i915")),
            (HardwareCategory::Audio, _) => (vec!["firmware-sof-signed", "pipewire-audio"], None),
            _ => (Vec::new(), None),
        };

    let fw_missing_in_dmesg = firmware_errors.iter().any(|line| {
        let lower = line.to_ascii_lowercase();
        match (category, dev.vendor_id) {
            (HardwareCategory::Wifi, 0x8086) => lower.contains("iwlwifi"),
            (HardwareCategory::Wifi, 0x10ec) => lower.contains("rtw") || lower.contains("rtl"),
            (HardwareCategory::Graphics, 0x1002) => {
                lower.contains("amdgpu") || lower.contains("radeon")
            }
            (HardwareCategory::Graphics, 0x8086) => lower.contains("i915") || lower.contains("xe"),
            (HardwareCategory::Audio, _) => lower.contains("sof") || lower.contains("dsp"),
            _ => false,
        }
    });

    let missing_pkgs: Vec<String> = recommended_pkgs
        .iter()
        .filter(|pkg| !pkg_installed_fn(pkg))
        .map(|s| (*s).to_string())
        .collect();

    let (status, explanation) = if let Some(ref drv) = dev.driver {
        if fw_missing_in_dmesg && !missing_pkgs.is_empty() {
            (
                DriverStatus::MissingFirmware,
                format!(
                    "Đã nạp driver '{}' nhưng kernel báo thiếu tệp firmware. Nên cài gói đề xuất.",
                    drv
                ),
            )
        } else if category == HardwareCategory::Graphics
            && dev.vendor_id == 0x10de
            && drv == "nouveau"
        {
            if pkg_installed_fn("nvidia-driver") {
                (
                    DriverStatus::NeedsReboot,
                    "Driver NVIDIA chính hãng đã được cài, hãy khởi động lại máy để kích hoạt."
                        .to_string(),
                )
            } else if secure_boot {
                (
                    DriverStatus::Blocked,
                    "Đang chạy driver nguồn mở 'nouveau'. Có thể cài 'nvidia-driver' (lưu ý Secure Boot đang bật)."
                        .to_string(),
                )
            } else {
                (
                    DriverStatus::MissingDriver,
                    "Card đồ họa NVIDIA đang dùng driver cơ bản 'nouveau'. Cài 'nvidia-driver' để đạt hiệu năng tối ưu."
                        .to_string(),
                )
            }
        } else {
            (
                DriverStatus::Working,
                format!("Đang hoạt động ổn định với driver kernel '{}'.", drv),
            )
        }
    } else if expected_module
        .map(|m| blacklisted.contains(m))
        .unwrap_or(false)
    {
        (
            DriverStatus::Blocked,
            "Module driver đang bị chặn trong /etc/modprobe.d/ (blacklist).".to_string(),
        )
    } else if secure_boot && matches!(dev.vendor_id, 0x10de | 0x14e4) {
        (
            DriverStatus::Blocked,
            "Thiết bị cần driver DKMS nhưng Secure Boot đang bật có thể chặn nạp module chưa ký."
                .to_string(),
        )
    } else if !recommended_pkgs.is_empty() && missing_pkgs.is_empty() {
        (
            DriverStatus::NeedsReboot,
            "Gói driver/firmware đã có trên máy nhưng thiết bị chưa nạp module; hãy khởi động lại."
                .to_string(),
        )
    } else {
        (
            DriverStatus::MissingDriver,
            "Thiết bị chưa có driver kernel đang hoạt động; cần cài bổ sung gói đề xuất."
                .to_string(),
        )
    };

    Some(HardwareFinding {
        category,
        device_name: dev.human_name.clone(),
        driver_in_use: dev.driver.clone(),
        status,
        explanation,
        recommended_packages: if status.is_issue() {
            missing_pkgs
        } else {
            Vec::new()
        },
    })
}

pub fn scan_system_drivers() -> DriverScanReport {
    let secure_boot = detect_secure_boot_enabled();
    let blacklisted = collect_blacklisted_modules();
    let firmware_errors = collect_kernel_firmware_errors();
    let pci_devices = read_sysfs_pci_devices();

    let mut findings = Vec::new();
    for dev in &pci_devices {
        if let Some(f) = classify_pci_device(
            dev,
            secure_boot,
            &blacklisted,
            &firmware_errors,
            &is_deb_pkg_installed,
        ) {
            findings.push(f);
        }
    }

    let cups_installed = is_deb_pkg_installed("cups");
    findings.push(if cups_installed {
        HardwareFinding {
            category: HardwareCategory::Printer,
            device_name: "Hệ thống in ấn CUPS".to_string(),
            driver_in_use: Some("cupsd".to_string()),
            status: DriverStatus::Working,
            explanation: "Dịch vụ in ấn CUPS đã sẵn sàng cho máy in USB và máy in mạng (IPP)."
                .to_string(),
            recommended_packages: Vec::new(),
        }
    } else {
        HardwareFinding {
            category: HardwareCategory::Printer,
            device_name: "Hệ thống in ấn CUPS".to_string(),
            driver_in_use: None,
            status: DriverStatus::MissingDriver,
            explanation: "Chưa cài dịch vụ in ấn CUPS để nhận diện máy in USB/mạng.".to_string(),
            recommended_packages: vec!["cups".to_string()],
        }
    });

    DriverScanReport {
        secure_boot_enabled: secure_boot,
        findings,
        firmware_errors,
    }
}

pub fn is_live_session() -> bool {
    Path::new("/run/live/medium").exists()
}

pub fn caramos_user_state_dir() -> PathBuf {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".config/caramos")
}

pub fn export_hardware_report_to_desktop(report: &DriverScanReport) -> Result<PathBuf, String> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let desktop = home.join("Desktop");
    let target_dir = if desktop.is_dir() { desktop } else { home };
    let out_path = target_dir.join("caramos-hardware-report.txt");
    fs::write(&out_path, report.format_text_report())
        .map_err(|e| format!("Không ghi được báo cáo phần cứng: {}", e))?;
    Ok(out_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_catalog_has_all_caramos_core_groups_and_unique_ids() {
        let mut seen = BTreeSet::new();
        for app in CARAMOS_APP_CATALOG {
            assert!(seen.insert(app.id), "Duplicate app id: {}", app.id);
        }
        assert!(seen.contains("zalo"));
        assert!(seen.contains("onlyoffice"));
        assert!(seen.contains("libreoffice"));
        assert!(seen.contains("firefox"));
        assert!(seen.contains("vlc"));
    }

    #[test]
    fn test_validate_apt_target_blocks_shell_injection() {
        assert!(validate_apt_target("firefox-esr").is_ok());
        assert!(validate_apt_target("libgtk-4-1").is_ok());
        assert!(validate_apt_target("vlc; rm -rf /").is_err());
        assert!(validate_apt_target("../etc/passwd").is_err());
        assert!(validate_apt_target("").is_err());
    }

    #[test]
    fn test_select_zalo_amd64_deb() {
        let assets = [
            ("Zalo-Desktop-0.1.5-x86_64.AppImage", "https://ex/a"),
            ("zalo-desktop_0.1.5_amd64.deb", "https://ex/b"),
        ];
        assert_eq!(
            select_amd64_deb_url(&assets).as_deref(),
            Some("https://ex/b")
        );
    }

    #[test]
    fn test_driver_classifier_detects_missing_firmware_and_nvidia() {
        let wifi_dev = PciDeviceInfo {
            slot: "0000:02:00.0".to_string(),
            vendor_id: 0x8086,
            device_id: 0x2725,
            class_code: 0x028000,
            driver: Some("iwlwifi".to_string()),
            human_name: "Intel Wi-Fi 6 AX210".to_string(),
        };
        let fw_errs = vec!["iwlwifi: Direct firmware load for iwlwifi-ty.ucode failed".to_string()];
        let finding = classify_pci_device(
            &wifi_dev,
            false,
            &BTreeSet::new(),
            &fw_errs,
            &|_| false,
        );
        assert!(matches!(
            finding.as_ref().map(|f| f.status),
            Some(DriverStatus::MissingFirmware)
        ));

        let nvidia_dev = PciDeviceInfo {
            slot: "0000:01:00.0".to_string(),
            vendor_id: 0x10de,
            device_id: 0x2504,
            class_code: 0x030000,
            driver: Some("nouveau".to_string()),
            human_name: "NVIDIA GeForce RTX 3060".to_string(),
        };
        let nv_finding = classify_pci_device(
            &nvidia_dev,
            false,
            &BTreeSet::new(),
            &[],
            &|_| false,
        );
        assert!(matches!(
            nv_finding.as_ref().map(|f| f.status),
            Some(DriverStatus::MissingDriver)
        ));
    }
}

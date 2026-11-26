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

mod theme_apply;

use clap::{Parser, Subcommand};
use image::{ImageFormat, Rgba, RgbaImage};
use serde::Deserialize;
use std::fs;
use std::io::{self, BufRead, Cursor, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use tempfile::NamedTempFile;
use thiserror::Error;
use walkdir::WalkDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Debug, Error)]
pub enum CaramUiError {
    #[error("Lỗi I/O ({context}): {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error("Lỗi xử lý ảnh ({context}): {detail}")]
    Image { context: String, detail: String },
    #[error("Tham số không hợp lệ: {0}")]
    InvalidArgs(String),
}

#[derive(Debug, Parser)]
#[command(
    name = "caramos-ui",
    version,
    about = "CaramOS 2.0 Branding, Theme Applier & OTA TUI (Rust)"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Dựng cây overlay nhận diện GRUB2, Plymouth, LightDM Slick Greeter, Calamares & Desktop
    Branding {
        #[arg(long)]
        output_root: PathBuf,
        #[arg(long)]
        wallpaper_dir: Option<PathBuf>,
        #[arg(long)]
        assets_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 1920)]
        width: u32,
        #[arg(long, default_value_t = 1080)]
        height: u32,
    },
    /// Áp giao diện CaramOS chuẩn ở lần đăng nhập đầu tiên
    ApplyTheme {
        #[arg(long)]
        force: bool,
    },
    /// Kiểm tra cập nhật OTA dạng TUI ANSI trên terminal
    Ota {
        #[arg(
            long,
            default_value = "https://api.github.com/repos/LocShadowVN/CaramOS-testing/releases/latest"
        )]
        endpoint: String,
        #[arg(long)]
        dry_run: bool,
    },
}

pub(crate) fn atomic_write_bytes(dest: &Path, data: &[u8]) -> Result<(), CaramUiError> {
    atomic_write_bytes_with_mode(dest, data, 0o644)
}

fn atomic_write_bytes_with_mode(dest: &Path, data: &[u8], mode: u32) -> Result<(), CaramUiError> {
    let parent = dest.parent().ok_or_else(|| {
        CaramUiError::InvalidArgs(format!("Đường dẫn không có thư mục cha: {}", dest.display()))
    })?;
    fs::create_dir_all(parent).map_err(|e| CaramUiError::Io {
        context: format!("Tạo thư mục '{}'", parent.display()),
        source: e,
    })?;
    let mut tmp = NamedTempFile::new_in(parent).map_err(|e| CaramUiError::Io {
        context: format!("Tạo tệp tạm trong '{}'", parent.display()),
        source: e,
    })?;
    tmp.write_all(data).map_err(|e| CaramUiError::Io {
        context: format!("Ghi dữ liệu vào '{}'", dest.display()),
        source: e,
    })?;
    tmp.as_file().sync_all().map_err(|e| CaramUiError::Io {
        context: format!("fsync '{}'", dest.display()),
        source: e,
    })?;
    #[cfg(unix)]
    {
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode)).map_err(|e| {
            CaramUiError::Io {
                context: format!("Đặt quyền {:o} cho '{}'", mode, dest.display()),
                source: e,
            }
        })?;
    }
    #[cfg(not(unix))]
    let _ = mode;

    tmp.persist(dest).map_err(|e| CaramUiError::Io {
        context: format!("Lưu tệp '{}'", dest.display()),
        source: e.error,
    })?;
    Ok(())
}

fn save_png_atomically(dest: &Path, img: &RgbaImage) -> Result<(), CaramUiError> {
    let mut bytes = Vec::new();
    let mut cursor = Cursor::new(&mut bytes);
    img.write_to(&mut cursor, ImageFormat::Png)
        .map_err(|e| CaramUiError::Image {
            context: dest.display().to_string(),
            detail: e.to_string(),
        })?;
    atomic_write_bytes(dest, &bytes)
}

fn discover_existing_wallpaper(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let mut candidates: Vec<PathBuf> = WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file()
                && e.file_name()
                    .to_str()
                    .map(theme_apply::is_clean_wallpaper_filename)
                    .unwrap_or(false)
        })
        .map(|e| e.into_path())
        .collect();
    candidates.sort();
    candidates
        .iter()
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.eq_ignore_ascii_case("default.png") || s.contains("sage"))
                .unwrap_or(false)
        })
        .cloned()
        .or_else(|| candidates.into_iter().next())
}

fn discover_existing_logo(assets_dir: &Path) -> Option<PathBuf> {
    if !assets_dir.is_dir() {
        return None;
    }
    for name in ["caramos-logo.png", "logo.png", "icon.png"] {
        let candidate = assets_dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn render_fallback_wallpaper(width: u32, height: u32) -> RgbaImage {
    let w = width.max(640);
    let h = height.max(480);
    let mut img = RgbaImage::new(w, h);
    let denom_x = (w.saturating_sub(1)).max(1) as f32;
    let denom_y = (h.saturating_sub(1)).max(1) as f32;
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let tx = x as f32 / denom_x;
        let ty = y as f32 / denom_y;
        let r = (24.0 + 19.0 * (1.0 - ty) + 10.0 * tx).clamp(0.0, 255.0) as u8;
        let g = (52.0 + 54.0 * (1.0 - ty) + 12.0 * (1.0 - tx)).clamp(0.0, 255.0) as u8;
        let b = (42.0 + 26.0 * (1.0 - ty) + 16.0 * tx).clamp(0.0, 255.0) as u8;
        *pixel = Rgba([r, g, b, 255]);
    }
    img
}

fn render_fallback_logo(size: u32) -> RgbaImage {
    let s = size.max(64);
    let mut img = RgbaImage::new(s, s);
    let center = (s as f32) / 2.0;
    let outer_r = (s as f32) * 0.44;
    let inner_r = (s as f32) * 0.24;
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let dx = x as f32 - center;
        let dy = y as f32 - center;
        let dist = (dx * dx + dy * dy).sqrt();
        if dist <= outer_r {
            if dist >= inner_r && !(dx > 0.0 && dy.abs() < inner_r * 0.65) {
                *pixel = Rgba([255, 255, 255, 255]);
            } else {
                *pixel = Rgba([43, 106, 68, 255]);
            }
        } else {
            *pixel = Rgba([0, 0, 0, 0]);
        }
    }
    img
}

fn build_caramos_branding_overlay(
    output_root: &Path,
    wallpaper_dir: Option<&Path>,
    assets_dir: Option<&Path>,
    width: u32,
    height: u32,
) -> Result<(), CaramUiError> {
    if width < 640 || height < 480 {
        return Err(CaramUiError::InvalidArgs(
            "Độ phân giải tối thiểu là 640x480".to_string(),
        ));
    }

    let wallpaper_bytes = if let Some(wp_path) = wallpaper_dir.and_then(discover_existing_wallpaper)
    {
        fs::read(&wp_path).map_err(|e| CaramUiError::Io {
            context: format!("Đọc hình nền '{}'", wp_path.display()),
            source: e,
        })?
    } else {
        let fallback = render_fallback_wallpaper(width, height);
        let mut buf = Vec::new();
        let mut cur = Cursor::new(&mut buf);
        fallback
            .write_to(&mut cur, ImageFormat::Png)
            .map_err(|e| CaramUiError::Image {
                context: "render_fallback_wallpaper".to_string(),
                detail: e.to_string(),
            })?;
        buf
    };

    let logo_img = if let Some(lp) = assets_dir.and_then(discover_existing_logo) {
        image::open(&lp)
            .map(|i| i.to_rgba8())
            .unwrap_or_else(|_| render_fallback_logo(128))
    } else {
        render_fallback_logo(128)
    };

    let grub_dir = output_root.join("boot/grub/themes/caramos");
    let calamares_dir = output_root.join("etc/calamares/branding/caramos");
    let plymouth_dir = output_root.join("usr/share/plymouth/themes/caramos");
    atomic_write_bytes(&grub_dir.join("background.png"), &wallpaper_bytes)?;
    atomic_write_bytes(
        &output_root.join("usr/share/backgrounds/caramos/default.png"),
        &wallpaper_bytes,
    )?;
    atomic_write_bytes(
        &output_root.join("usr/share/backgrounds/caramos-login.png"),
        &wallpaper_bytes,
    )?;
    atomic_write_bytes(&calamares_dir.join("welcome.png"), &wallpaper_bytes)?;

    save_png_atomically(
        &output_root.join("usr/share/pixmaps/caramos-logo.png"),
        &logo_img,
    )?;
    save_png_atomically(&calamares_dir.join("logo.png"), &logo_img)?;
    save_png_atomically(&plymouth_dir.join("logo.png"), &logo_img)?;

    let grub_theme = r##"title-text: "CaramOS 2.0 (Debian 13)"
desktop-image: "background.png"
desktop-color: "#18342a"
terminal-font: "DejaVu Sans Mono Regular 12"

+ boot_menu {
    left = 20%
    top = 30%
    width = 60%
    height = 42%
    item_color = "#d6e6dc"
    selected_item_color = "#ffffff"
    item_height = 34
    item_spacing = 8
}

+ label {
    top = 86%
    left = 0
    width = 100%
    align = "center"
    text = "Dùng phím mũi tên để chọn và nhấn Enter để khởi động"
    color = "#b8d0c2"
}
"##;
    atomic_write_bytes(&grub_dir.join("theme.txt"), grub_theme.as_bytes())?;

    let plymouth_desc = r##"[Plymouth Theme]
Name=CaramOS
Description=CaramOS 2.0 Boot Splash
ModuleName=script

[script]
ImageDir=/usr/share/plymouth/themes/caramos
ScriptFile=/usr/share/plymouth/themes/caramos/caramos.script
"##;
    let plymouth_script = r##"Window.SetBackgroundTopColor(0.09, 0.20, 0.16);
Window.SetBackgroundBottomColor(0.06, 0.13, 0.10);
logo.image = Image("logo.png");
logo.sprite = Sprite(logo.image);
logo.sprite.SetX(Window.GetWidth() / 2 - logo.image.GetWidth() / 2);
logo.sprite.SetY(Window.GetHeight() / 2 - logo.image.GetHeight() / 2);
"##;
    atomic_write_bytes(&plymouth_dir.join("caramos.plymouth"), plymouth_desc.as_bytes())?;
    atomic_write_bytes(&plymouth_dir.join("caramos.script"), plymouth_script.as_bytes())?;

    let calamares_desc = r##"---
componentName: caramos
welcomeStyleCalamares: true
welcomeExpandingLogo: true
windowExpanding: normal
windowSize: 900px,560px
windowPlacement: center
sidebar: widget
navigation: widget

strings:
    productName:         CaramOS 2.0
    shortProductName:    CaramOS
    version:             2.0
    shortVersion:        2.0
    versionedName:       CaramOS 2.0 (Debian 13)
    shortVersionedName:  CaramOS 2.0
    bootloaderEntryName: CaramOS
    productUrl:          https://vietnamlinuxfamily.net/
    supportUrl:          https://github.com/LocShadowVN/CaramOS-testing

images:
    productLogo:         "logo.png"
    productIcon:         "logo.png"
    productWelcome:      "welcome.png"

slideshow:               "show.qml"

style:
    SidebarBackground:        "#1e3d2b"
    SidebarText:              "#f3f7f4"
    SidebarTextCurrent:       "#ffffff"
    SidebarBackgroundCurrent: "#2b6a44"
"##;
    let calamares_qml = r##"import QtQuick 2.0;
import calamares.slideshow 1.0;

Presentation {
    id: presentation
    Slide {
        Image {
            anchors.fill: parent
            source: "welcome.png"
            fillMode: Image.PreserveAspectCrop
        }
    }
}
"##;
    atomic_write_bytes(&calamares_dir.join("branding.desc"), calamares_desc.as_bytes())?;
    atomic_write_bytes(&calamares_dir.join("show.qml"), calamares_qml.as_bytes())?;

    let lightdm_conf = "[Seat:*]\nuser-session=cinnamon\ngreeter-session=slick-greeter\n";
    let slick_conf = r##"[Greeter]
background=/usr/share/backgrounds/caramos-login.png
draw-user-backgrounds=false
draw-grid=false
theme-name=Cinnamon-Delight
icon-theme-name=Tela-circle-light
cursor-theme-name=Bibata-Modern-Classic
font-name=Be Vietnam Pro 10
show-hostname=true
show-clock=true
clock-format=%H:%M
"##;
    atomic_write_bytes(
        &output_root.join("etc/lightdm/lightdm.conf.d/50-caramos.conf"),
        lightdm_conf.as_bytes(),
    )?;
    atomic_write_bytes(
        &output_root.join("etc/lightdm/slick-greeter.conf"),
        slick_conf.as_bytes(),
    )?;

    let island_light_css = r##"stage { font-family: "Be Vietnam Pro", "Noto Sans", sans-serif; font-size: 10pt; color: #1f2923; }
.panel-bottom { height: 48px; background-color: transparent; padding: 4px 10px; }
.panelLeft, .panelCenter, .panelRight {
    background-color: rgba(248, 250, 248, 0.90);
    border: 1px solid rgba(43, 106, 68, 0.18);
    border-radius: 14px;
    padding: 0 8px;
}
.grouped-window-list-item-box:checked, .grouped-window-list-item-box:active {
    border-bottom: 3px solid #2b6a44;
    background-color: rgba(43, 106, 68, 0.12);
    border-radius: 10px;
}
"##;
    let island_dark_css = r##"stage { font-family: "Be Vietnam Pro", "Noto Sans", sans-serif; font-size: 10pt; color: #eef3f0; }
.panel-bottom { height: 48px; background-color: transparent; padding: 4px 10px; }
.panelLeft, .panelCenter, .panelRight {
    background-color: rgba(24, 30, 27, 0.90);
    border: 1px solid rgba(140, 207, 165, 0.18);
    border-radius: 14px;
    padding: 0 8px;
}
.grouped-window-list-item-box:checked, .grouped-window-list-item-box:active {
    border-bottom: 3px solid #3a855a;
    background-color: rgba(58, 133, 90, 0.22);
    border-radius: 10px;
}
"##;
    atomic_write_bytes(
        &output_root.join("usr/share/themes/CaramOS-Island/cinnamon/cinnamon.css"),
        island_light_css.as_bytes(),
    )?;
    atomic_write_bytes(
        &output_root.join("usr/share/themes/CaramOS-Island-Dark/cinnamon/cinnamon.css"),
        island_dark_css.as_bytes(),
    )?;

    let apps_dir = output_root.join("usr/share/applications");
    let autostart_dir = output_root.join("etc/xdg/autostart");

    let app_center_desktop = r##"[Desktop Entry]
Type=Application
Version=1.0
Name=Ứng dụng CaramOS
GenericName=Trung tâm Ứng dụng
Comment=Cài đặt nhanh bộ văn phòng, trình duyệt, Zalo và phần mềm học tập
Exec=/usr/local/bin/caramos-app-center
Icon=system-software-install
Terminal=false
Categories=System;Settings;
StartupNotify=true
"##;
    let app_center_link_desktop = r##"[Desktop Entry]
Type=Application
Version=1.0
Name=Cài đặt ứng dụng CaramOS
Exec=/usr/local/bin/caramos-app-center %u
Icon=system-software-install
Terminal=false
NoDisplay=true
MimeType=x-scheme-handler/caramos-app;
"##;
    let driver_check_desktop = r##"[Desktop Entry]
Type=Application
Version=1.0
Name=Kiểm tra Driver
GenericName=Trình điều khiển phần cứng
Comment=Kiểm tra và cài đặt driver Wi-Fi, đồ họa, âm thanh và máy in trên Debian
Exec=/usr/local/bin/caramos-driver-check
Icon=preferences-system
Terminal=false
Categories=System;Settings;HardwareSettings;
StartupNotify=true
"##;
    let app_center_firstboot = r##"[Desktop Entry]
Type=Application
Name=CaramOS App Center First Boot
Exec=/usr/local/bin/caramos-app-center --first-boot
X-GNOME-Autostart-enabled=true
NoDisplay=true
OnlyShowIn=X-Cinnamon;
"##;
    let driver_check_autostart = r##"[Desktop Entry]
Type=Application
Name=CaramOS Driver Check Autostart
Exec=/usr/local/bin/caramos-driver-check --autostart
X-GNOME-Autostart-enabled=true
NoDisplay=true
OnlyShowIn=X-Cinnamon;
"##;

    atomic_write_bytes_with_mode(
        &apps_dir.join("caramos-app-center.desktop"),
        app_center_desktop.as_bytes(),
        0o644,
    )?;
    atomic_write_bytes_with_mode(
        &apps_dir.join("caramos-app-center-link.desktop"),
        app_center_link_desktop.as_bytes(),
        0o644,
    )?;
    atomic_write_bytes_with_mode(
        &apps_dir.join("caramos-driver-check.desktop"),
        driver_check_desktop.as_bytes(),
        0o644,
    )?;
    atomic_write_bytes_with_mode(
        &autostart_dir.join("caramos-app-center-firstboot.desktop"),
        app_center_firstboot.as_bytes(),
        0o644,
    )?;
    atomic_write_bytes_with_mode(
        &autostart_dir.join("caramos-driver-check-autostart.desktop"),
        driver_check_autostart.as_bytes(),
        0o644,
    )?;

    Ok(())
}

#[derive(Debug, Deserialize)]
struct OtaReleaseInfo {
    tag_name: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    body: String,
}

fn fetch_ota_summary(endpoint: &str, dry_run: bool) -> String {
    if dry_run {
        return format!(
            "Chế độ kiểm thử (--dry-run)\nNguồn: {}\nHệ thống: CaramOS 2.0 (Debian 13 Trixie)\nTrạng thái: Sẵn sàng.",
            endpoint
        );
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(12))
        .timeout_read(Duration::from_secs(20))
        .build();
    match agent
        .get(endpoint)
        .set("User-Agent", "caramos-ui-ota/2.0")
        .set("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(resp) => match resp.into_json::<OtaReleaseInfo>() {
            Ok(rel) => format!(
                "Phiên bản mới nhất: {} ({})\n\n{}",
                rel.tag_name,
                rel.name,
                if rel.body.trim().is_empty() {
                    "Không có ghi chú phát hành."
                } else {
                    rel.body.trim()
                }
            ),
            Err(e) => format!("Không đọc được JSON bản phát hành: {}", e),
        },
        Err(e) => format!("Không thể kết nối máy chủ cập nhật: {}", e),
    }
}

fn run_ota_tui(endpoint: &str, dry_run: bool) -> Result<(), CaramUiError> {
    let summary = fetch_ota_summary(endpoint, dry_run);
    let mut out = io::stdout();
    let is_tty = out.is_terminal();

    if is_tty {
        let _ = write!(out, "\x1b[2J\x1b[H");
    }
    let header_line = "══════════════════════════════════════════════════════════════";
    writeln!(out, "\x1b[1;38;2;140;207;165m{}\x1b[0m", header_line).map_err(|e| CaramUiError::Io {
        context: "Ghi tiêu đề OTA TUI".to_string(),
        source: e,
    })?;
    writeln!(
        out,
        "\x1b[1;38;2;140;207;165m  CaramOS 2.0 — Trình kiểm tra cập nhật OTA (Debian 13)\x1b[0m"
    )
    .map_err(|e| CaramUiError::Io {
        context: "Ghi tiêu đề OTA TUI".to_string(),
        source: e,
    })?;
    writeln!(out, "\x1b[1;38;2;140;207;165m{}\x1b[0m\n", header_line).map_err(|e| {
        CaramUiError::Io {
            context: "Ghi khung OTA TUI".to_string(),
            source: e,
        }
    })?;
    writeln!(out, "{}\n", summary).map_err(|e| CaramUiError::Io {
        context: "Ghi nội dung OTA TUI".to_string(),
        source: e,
    })?;
    out.flush().map_err(|e| CaramUiError::Io {
        context: "flush stdout".to_string(),
        source: e,
    })?;

    if is_tty && io::stdin().is_terminal() {
        let _ = write!(out, "Nhấn Enter để thoát...");
        let _ = out.flush();
        let mut buf = String::new();
        let _ = io::stdin().lock().read_line(&mut buf);
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Branding {
            output_root,
            wallpaper_dir,
            assets_dir,
            width,
            height,
        } => build_caramos_branding_overlay(
            &output_root,
            wallpaper_dir.as_deref(),
            assets_dir.as_deref(),
            width,
            height,
        ),
        Commands::ApplyTheme { force } => theme_apply::run_apply_theme(force),
        Commands::Ota { endpoint, dry_run } => run_ota_tui(&endpoint, dry_run),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[caramos-ui] Lỗi: {}", e);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn branding_emits_required_grub_calamares_slick_and_link_desktop() {
        let dir = tempdir().unwrap();
        build_caramos_branding_overlay(dir.path(), None, None, 1280, 720).unwrap();

        assert!(dir.path().join("boot/grub/themes/caramos/background.png").is_file());
        assert!(dir.path().join("boot/grub/themes/caramos/theme.txt").is_file());
        assert!(dir.path().join("etc/calamares/branding/caramos/branding.desc").is_file());
        assert!(dir.path().join("etc/calamares/branding/caramos/welcome.png").is_file());
        assert!(dir.path().join("etc/lightdm/slick-greeter.conf").is_file());
        assert!(dir.path().join("usr/share/applications/caramos-app-center.desktop").is_file());
        assert!(dir.path().join("usr/share/applications/caramos-app-center-link.desktop").is_file());
        assert!(dir.path().join("usr/share/applications/caramos-driver-check.desktop").is_file());
    }
}

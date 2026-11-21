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

//! # CaramOS 2.0 Branding Asset Stager & Atomic OTA TUI (`caramos-ui`)
//!
//! 1. `branding` (alias: `bootloader`): Tái sử dụng trực tiếp hình nền có sẵn trong
//!    `config/includes.chroot/usr/share/backgrounds/` và `assets/CaramOS_logo.png` cho
//!    GRUB2, Plymouth, Calamares GUI và sinh các tệp `.desktop` / `autostart` cho
//!    `caramos-app-center` & `caramos-driver-check`.
//! 2. `updater`: Trình cập nhật OTA TUI truyền luồng `O(1)` RAM, xác thực SHA-256 (`sha2`)
//!    và Rollback nguyên tử (`tempfile`).

use clap::{Parser, Subcommand};
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader};
use sha2::{Digest, Sha256};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use tempfile::NamedTempFile;
use thiserror::Error;
use walkdir::WalkDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn fmt_opt_path(path: &Option<PathBuf>) -> String {
    match path {
        Some(p) => format!(" tại '{}'", p.display()),
        None => String::new(),
    }
}

#[derive(Debug, Error)]
pub enum CaramUiError {
    #[error("[CLI_ERR] Tham số '{argument}' không hợp lệ: {reason}")]
    InvalidCliArgument { argument: String, reason: String },

    #[error("[VALIDATION_ERR] Trường '{field}' có giá trị '{value}' bị từ chối: {reason}")]
    ValidationFailed {
        field: &'static str,
        value: String,
        reason: &'static str,
    },

    #[error("[SECURITY_ERR] Đường dẫn '{}' bị chặn vì lý do an toàn: {reason}", path.display())]
    UnsafePath { path: PathBuf, reason: &'static str },

    #[error("[ASSET_ERR] Không tìm thấy hình nền có sẵn trong '{}'. Không tự tạo hình nền giả; hãy truyền --wallpaper <FILE> hoặc bổ sung ảnh vào thư mục backgrounds.", search_dir.display())]
    WallpaperNotFound { search_dir: PathBuf },

    #[error("[IMAGE_ERR] Không thể đọc/chuẩn hóa ảnh nền có sẵn '{}': {reason}", path.display())]
    ImageProcessingFailed { path: PathBuf, reason: String },

    #[error("[IO_ERR] Thao tác '{context}'{} thất bại: {source}", fmt_opt_path(path))]
    IoFailure {
        context: String,
        path: Option<PathBuf>,
        #[source]
        source: io::Error,
    },

    #[error("[CRYPTO_ERR] Sai lệch mã băm SHA-256 tại '{}': kỳ vọng '{expected}', thực tế '{actual}'", target.display())]
    DigestMismatch {
        target: PathBuf,
        expected: String,
        actual: String,
    },

    #[error("[CRITICAL_OTA_ERR] Lỗi gốc ({original_err}) VÀ lỗi khôi phục tại '{}': {rollback_err}", target.display())]
    RollbackFailure {
        target: PathBuf,
        original_err: String,
        #[source]
        rollback_err: io::Error,
    },
}

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_INDIGO: &str = "\x1b[38;5;75m";
const ANSI_SKY: &str = "\x1b[38;5;117m";
const ANSI_WHITE: &str = "\x1b[38;5;255m";
const ANSI_SAGE: &str = "\x1b[38;5;114m";
const ANSI_ROSE: &str = "\x1b[38;5;203m";
const ANSI_SLATE: &str = "\x1b[38;5;246m";

pub struct TerminalSessionGuard {
    active: bool,
}

impl TerminalSessionGuard {
    pub fn new(active: bool) -> io::Result<Self> {
        if active {
            let mut out = io::stdout().lock();
            out.write_all(ANSI_RESET.as_bytes())?;
            out.flush()?;
        }
        Ok(Self { active })
    }
}

impl Drop for TerminalSessionGuard {
    fn drop(&mut self) {
        if self.active {
            let mut out = io::stdout().lock();
            let _ = out.write_all(b"\x1b[0m\n");
            let _ = out.flush();
        }
    }
}

pub fn validate_safe_path(path: &Path) -> Result<(), CaramUiError> {
    let raw = path.to_string_lossy();
    if raw.is_empty() {
        return Err(CaramUiError::UnsafePath {
            path: path.to_path_buf(),
            reason: "Đường dẫn không được để trống",
        });
    }
    if raw.contains('\0') {
        return Err(CaramUiError::UnsafePath {
            path: path.to_path_buf(),
            reason: "Đường dẫn chứa byte NULL độc hại",
        });
    }
    for comp in path.components() {
        if matches!(comp, Component::ParentDir) {
            return Err(CaramUiError::UnsafePath {
                path: path.to_path_buf(),
                reason: "Chặn hành vi leo thang thư mục qua '..' (Path Traversal)",
            });
        }
    }
    Ok(())
}

fn append_filename_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(path.as_os_str());
    os.push(suffix);
    PathBuf::from(os)
}

#[cfg(unix)]
fn set_unix_mode(path: &Path, mode: u32) -> Result<(), CaramUiError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| {
        CaramUiError::IoFailure {
            context: format!("Thiết lập quyền POSIX {:o}", mode),
            path: Some(path.to_path_buf()),
            source: e,
        }
    })
}

#[cfg(not(unix))]
fn set_unix_mode(_path: &Path, _mode: u32) -> Result<(), CaramUiError> {
    Ok(())
}

fn atomic_write_bytes(target_path: &Path, content: &[u8]) -> Result<(), CaramUiError> {
    validate_safe_path(target_path)?;
    let parent = target_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| CaramUiError::IoFailure {
        context: "Tạo thư mục cha cho tệp đích".to_string(),
        path: Some(parent.to_path_buf()),
        source: e,
    })?;

    let mut tmp = NamedTempFile::new_in(parent).map_err(|e| CaramUiError::IoFailure {
        context: "Tạo tệp vùng đệm nguyên tử".to_string(),
        path: Some(parent.to_path_buf()),
        source: e,
    })?;
    let tmp_path = tmp.path().to_path_buf();

    tmp.write_all(content).map_err(|e| CaramUiError::IoFailure {
        context: "Ghi dữ liệu vào vùng đệm".to_string(),
        path: Some(tmp_path.clone()),
        source: e,
    })?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| CaramUiError::IoFailure {
            context: "Đồng bộ fsync vùng đệm".to_string(),
            path: Some(tmp_path),
            source: e,
        })?;
    set_unix_mode(tmp.path(), 0o644)?;

    tmp.persist(target_path).map_err(|e| CaramUiError::IoFailure {
        context: "Hoán đổi nguyên tử tệp đích".to_string(),
        path: Some(target_path.to_path_buf()),
        source: e.error,
    })?;
    Ok(())
}

fn is_supported_wallpaper_ext(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "webp"
    )
}

pub fn discover_existing_wallpaper(
    explicit_wallpaper: Option<&Path>,
    search_dir: &Path,
) -> Result<PathBuf, CaramUiError> {
    if let Some(explicit) = explicit_wallpaper {
        validate_safe_path(explicit)?;
        if !explicit.is_file() {
            return Err(CaramUiError::WallpaperNotFound {
                search_dir: explicit.to_path_buf(),
            });
        }
        return Ok(explicit.to_path_buf());
    }

    validate_safe_path(search_dir)?;
    if !search_dir.is_dir() {
        return Err(CaramUiError::WallpaperNotFound {
            search_dir: search_dir.to_path_buf(),
        });
    }

    let mut candidates: Vec<PathBuf> = WalkDir::new(search_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && is_supported_wallpaper_ext(e.path()))
        .map(|e| e.into_path())
        .collect();

    candidates.sort();

    let preferred_keywords = ["indigo", "night", "dark", "caramos"];
    for kw in preferred_keywords {
        if let Some(found) = candidates.iter().find(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase().contains(kw))
                .unwrap_or(false)
        }) {
            return Ok(found.clone());
        }
    }

    candidates
        .into_iter()
        .next()
        .ok_or_else(|| CaramUiError::WallpaperNotFound {
            search_dir: search_dir.to_path_buf(),
        })
}

fn stage_existing_image_png(
    src_image: &Path,
    dst_png: &Path,
    width: u16,
    height: u16,
) -> Result<(), CaramUiError> {
    if width < 64 || height < 64 || width > 3840 || height > 2160 {
        return Err(CaramUiError::ValidationFailed {
            field: "resolution",
            value: format!("{}x{}", width, height),
            reason: "Độ phân giải ảnh phải nằm trong khoảng 64x64 đến 3840x2160",
        });
    }

    let reader = ImageReader::open(src_image)
        .map_err(|e| CaramUiError::IoFailure {
            context: "Mở tệp hình ảnh có sẵn".to_string(),
            path: Some(src_image.to_path_buf()),
            source: e,
        })?
        .with_guessed_format()
        .map_err(|e| CaramUiError::IoFailure {
            context: "Nhận diện định dạng hình ảnh có sẵn".to_string(),
            path: Some(src_image.to_path_buf()),
            source: e,
        })?;

    let decoded = reader
        .decode()
        .map_err(|e| CaramUiError::ImageProcessingFailed {
            path: src_image.to_path_buf(),
            reason: e.to_string(),
        })?;

    let target_w = u32::from(width);
    let target_h = u32::from(height);
    let fitted = if decoded.width() == target_w && decoded.height() == target_h {
        decoded
    } else {
        decoded.resize_to_fill(target_w, target_h, FilterType::Lanczos3)
    };
    let rgb_image = DynamicImage::ImageRgb8(fitted.to_rgb8());

    let parent = dst_png
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| CaramUiError::IoFailure {
        context: "Tạo thư mục đích ảnh PNG".to_string(),
        path: Some(parent.to_path_buf()),
        source: e,
    })?;

    let mut tmp = NamedTempFile::new_in(parent).map_err(|e| CaramUiError::IoFailure {
        context: "Tạo tệp tạm PNG".to_string(),
        path: Some(parent.to_path_buf()),
        source: e,
    })?;
    let tmp_path = tmp.path().to_path_buf();

    {
        let mut buf_writer = BufWriter::new(tmp.as_file_mut());
        rgb_image
            .write_to(&mut buf_writer, ImageFormat::Png)
            .map_err(|e| CaramUiError::ImageProcessingFailed {
                path: dst_png.to_path_buf(),
                reason: e.to_string(),
            })?;
        buf_writer.flush().map_err(|e| CaramUiError::IoFailure {
            context: "Xả bộ đệm PNG".to_string(),
            path: Some(tmp_path.clone()),
            source: e,
        })?;
    }

    tmp.as_file()
        .sync_all()
        .map_err(|e| CaramUiError::IoFailure {
            context: "Đồng bộ fsync PNG".to_string(),
            path: Some(tmp_path),
            source: e,
        })?;
    set_unix_mode(tmp.path(), 0o644)?;

    tmp.persist(dst_png).map_err(|e| CaramUiError::IoFailure {
        context: "Lưu nguyên tử ảnh PNG".to_string(),
        path: Some(dst_png.to_path_buf()),
        source: e.error,
    })?;

    Ok(())
}

pub struct BrandingStageConfig<'a> {
    pub output_root: &'a Path,
    pub explicit_wallpaper: Option<&'a Path>,
    pub wallpaper_search_dir: &'a Path,
    pub assets_dir: &'a Path,
    pub width: u16,
    pub height: u16,
}

pub fn build_caramos_branding_overlay<W: Write>(
    cfg: &BrandingStageConfig<'_>,
    writer: &mut W,
) -> Result<PathBuf, CaramUiError> {
    validate_safe_path(cfg.output_root)?;
    validate_safe_path(cfg.assets_dir)?;

    let source_wallpaper =
        discover_existing_wallpaper(cfg.explicit_wallpaper, cfg.wallpaper_search_dir)?;

    let grub_theme_dir = cfg.output_root.join("boot/grub/themes/caramos");
    let plymouth_dir = cfg.output_root.join("usr/share/plymouth/themes/caramos");
    let etc_default_dir = cfg.output_root.join("etc/default");
    let calamares_brand_dir = cfg.output_root.join("etc/calamares/branding/caramos");
    let calamares_modules_dir = cfg.output_root.join("etc/calamares/modules");
    let applications_dir = cfg.output_root.join("usr/share/applications");
    let autostart_dir = cfg.output_root.join("etc/xdg/autostart");
    let pixmaps_dir = cfg.output_root.join("usr/share/pixmaps");

    for dir in [
        &grub_theme_dir,
        &plymouth_dir,
        &etc_default_dir,
        &calamares_brand_dir,
        &calamares_modules_dir,
        &applications_dir,
        &autostart_dir,
        &pixmaps_dir,
    ] {
        fs::create_dir_all(dir).map_err(|e| CaramUiError::IoFailure {
            context: "Tạo thư mục Branding GRUB2/Plymouth/Calamares".to_string(),
            path: Some(dir.clone()),
            source: e,
        })?;
    }

    let grub_bg_path = grub_theme_dir.join("background.png");
    stage_existing_image_png(&source_wallpaper, &grub_bg_path, cfg.width, cfg.height)?;

    let plymouth_bg_path = plymouth_dir.join("background.png");
    fs::copy(&grub_bg_path, &plymouth_bg_path).map_err(|e| CaramUiError::IoFailure {
        context: "Sao chép ảnh nền sang Plymouth theme".to_string(),
        path: Some(plymouth_bg_path.clone()),
        source: e,
    })?;
    set_unix_mode(&plymouth_bg_path, 0o644)?;

    let calamares_welcome_path = calamares_brand_dir.join("welcome.png");
    stage_existing_image_png(&source_wallpaper, &calamares_welcome_path, 800, 450)?;

    let logo_src = cfg.assets_dir.join("CaramOS_logo.png");
    let calamares_logo_path = calamares_brand_dir.join("logo.png");
    let has_logo = if logo_src.is_file() {
        let plymouth_logo = plymouth_dir.join("logo.png");
        fs::copy(&logo_src, &plymouth_logo).map_err(|e| CaramUiError::IoFailure {
            context: "Sao chép logo CaramOS sang Plymouth".to_string(),
            path: Some(plymouth_logo.clone()),
            source: e,
        })?;
        set_unix_mode(&plymouth_logo, 0o644)?;

        fs::copy(&logo_src, &calamares_logo_path).map_err(|e| CaramUiError::IoFailure {
            context: "Sao chép logo CaramOS sang Calamares branding".to_string(),
            path: Some(calamares_logo_path.clone()),
            source: e,
        })?;
        set_unix_mode(&calamares_logo_path, 0o644)?;

        let desktop_icon = pixmaps_dir.join("install-debian.png");
        fs::copy(&logo_src, &desktop_icon).map_err(|e| CaramUiError::IoFailure {
            context: "Cập nhật biểu tượng Calamares sang logo CaramOS".to_string(),
            path: Some(desktop_icon.clone()),
            source: e,
        })?;
        set_unix_mode(&desktop_icon, 0o644)?;
        true
    } else {
        stage_existing_image_png(&source_wallpaper, &calamares_logo_path, 128, 128)?;
        false
    };

    let grub_theme_txt = r##"# CaramOS Official GRUB2 Bootloader Theme
# Reuses existing CaramOS wallpaper & neutral Indigo Night / Sage Mist palette

title-text: ""
desktop-image: "background.png"
desktop-color: "#0F172A"

+ label {
    top = 10%
    left = 10%
    width = 80%
    height = 36
    text = "CaramOS 2.0  |  Debian 13 (Trixie)"
    color = "#F8FAFC"
    align = "center"
}

+ boot_menu {
    left = 18%
    top = 28%
    width = 64%
    height = 46%
    item_color = "#CBD5E1"
    selected_item_color = "#38BDF8"
    item_height = 32
    item_padding = 8
    item_spacing = 6
    scrollbar = true
    scrollbar_width = 6
    scrollbar_thumb = "#38BDF8"
}

+ progress_bar {
    id = "__timeout__"
    left = 18%
    top = 80%
    width = 64%
    height = 18
    fg_color = "#38BDF8"
    bg_color = "#1E293B"
    border_color = "#475569"
    text_color = "#F8FAFC"
    text = "@TIMEOUT_NOTIFICATION_LONG@"
}

+ label {
    top = 89%
    left = 10%
    width = 80%
    height = 24
    text = "[↑/↓] Chọn hệ thống   |   [Enter] Khởi động   |   [E] Chỉnh tham số"
    color = "#94A3B8"
    align = "center"
}
"##;
    let theme_txt_path = grub_theme_dir.join("theme.txt");
    atomic_write_bytes(&theme_txt_path, grub_theme_txt.as_bytes())?;

    let plymouth_conf = r#"[Plymouth Theme]
Name=CaramOS Boot Splash
Description=Official CaramOS Boot Splash using existing CaramOS wallpaper and logo
ModuleName=script

[script]
ImageDir=/usr/share/plymouth/themes/caramos
ScriptFile=/usr/share/plymouth/themes/caramos/caramos.script
"#;
    atomic_write_bytes(&plymouth_dir.join("caramos.plymouth"), plymouth_conf.as_bytes())?;

    let plymouth_script = if has_logo {
        r#"bg_img = Image("background.png");
scaled_bg = bg_img.Scale(Window.GetWidth(), Window.GetHeight());
bg_sprite = Sprite(scaled_bg);
bg_sprite.SetZ(-100);

logo_img = Image("logo.png");
logo_sprite = Sprite(logo_img);
logo_sprite.SetX(Window.GetWidth() / 2 - logo_img.GetWidth() / 2);
logo_sprite.SetY(Window.GetHeight() / 2 - logo_img.GetHeight() / 2);
logo_sprite.SetZ(10);
"#
    } else {
        r#"bg_img = Image("background.png");
scaled_bg = bg_img.Scale(Window.GetWidth(), Window.GetHeight());
bg_sprite = Sprite(scaled_bg);
bg_sprite.SetZ(-100);
"#
    };
    atomic_write_bytes(&plymouth_dir.join("caramos.script"), plymouth_script.as_bytes())?;

    let grub_default = format!(
        r#"# CaramOS 2.0 GRUB2 Configuration
GRUB_DEFAULT=0
GRUB_TIMEOUT=5
GRUB_DISTRIBUTOR="CaramOS"
GRUB_CMDLINE_LINUX_DEFAULT="quiet splash loglevel=3 mitigations=auto init_on_alloc=1"
GRUB_CMDLINE_LINUX=""
GRUB_GFXMODE={}x{}x24,auto
GRUB_GFXPAYLOAD_LINUX=keep
GRUB_THEME="/boot/grub/themes/caramos/theme.txt"
"#,
        cfg.width, cfg.height
    );
    atomic_write_bytes(&etc_default_dir.join("grub"), grub_default.as_bytes())?;

    let calamares_branding_desc = r##"---
componentName: caramos
welcomeStyleCalamares: false
welcomeExpandingLogo: true
windowExpanding: normal
windowSize: 900px,620px
windowPlacement: center

strings:
    productName:         "CaramOS 2.0"
    shortProductName:    "CaramOS"
    version:             "2.0 (Trixie)"
    shortVersion:        "2.0"
    versionedName:       "CaramOS 2.0 (Debian 13 Trixie)"
    shortVersionedName:  "CaramOS 2.0"
    bootloaderEntryName: "CaramOS"
    productUrl:          "https://caramos.vietnamlinuxfamily.net"
    supportUrl:          "https://github.com/LocShadowVN/CaramOS-testing/issues"
    knownIssuesUrl:      "https://github.com/LocShadowVN/CaramOS-testing/issues"
    releaseNotesUrl:     "https://github.com/LocShadowVN/CaramOS-testing"

images:
    productLogo:         "logo.png"
    productIcon:         "logo.png"
    productWelcome:      "welcome.png"

slideshow:               "show.qml"
slideshowAPI:            2

style:
    SidebarBackground:        "#0F172A"
    SidebarText:              "#E2E8F0"
    SidebarTextCurrent:       "#FFFFFF"
    SidebarBackgroundCurrent: "#1E293B"
"##;
    let branding_desc_path = calamares_brand_dir.join("branding.desc");
    atomic_write_bytes(&branding_desc_path, calamares_branding_desc.as_bytes())?;

    let calamares_show_qml = r##"import QtQuick 2.0
import calamares.slideshow 1.0

Presentation {
    id: presentation

    function nextSlide() {
        presentation.goToNextSlide();
    }

    Timer {
        id: advanceTimer
        interval: 8000
        running: presentation.activatedInCalamares
        repeat: true
        onTriggered: nextSlide()
    }

    function onActivate() {
        presentation.currentSlide = 0;
    }

    function onLeave() {
    }

    Slide {
        Image {
            anchors.fill: parent
            source: "welcome.png"
            fillMode: Image.PreserveAspectCrop
        }
        Rectangle {
            anchors.fill: parent
            color: "#AA0F172A"
        }
        Column {
            anchors.centerIn: parent
            width: parent.width * 0.82
            spacing: 14

            Image {
                anchors.horizontalCenter: parent.horizontalCenter
                source: "logo.png"
                width: 96
                height: 96
                fillMode: Image.PreserveAspectFit
            }
            Text {
                anchors.horizontalCenter: parent.horizontalCenter
                text: "Chào mừng đến với CaramOS 2.0"
                color: "#F8FAFC"
                font.pixelSize: 26
                font.bold: true
            }
            Text {
                width: parent.width
                horizontalAlignment: Text.AlignHCenter
                wrapMode: Text.WordWrap
                text: "Hệ điều hành Linux thân thiện cho người dùng Việt, dựng trực tiếp từ Debian 13 (Trixie) gọn nhẹ và ổn định."
                color: "#E2E8F0"
                font.pixelSize: 16
            }
        }
    }

    Slide {
        Image {
            anchors.fill: parent
            source: "welcome.png"
            fillMode: Image.PreserveAspectCrop
        }
        Rectangle {
            anchors.fill: parent
            color: "#AA0F172A"
        }
        Column {
            anchors.centerIn: parent
            width: parent.width * 0.82
            spacing: 12

            Text {
                anchors.horizontalCenter: parent.horizontalCenter
                text: "Trung tâm Ứng dụng & Bộ gõ Tiếng Việt Sẵn sàng"
                color: "#38BDF8"
                font.pixelSize: 24
                font.bold: true
            }
            Text {
                width: parent.width
                horizontalAlignment: Text.AlignHCenter
                wrapMode: Text.WordWrap
                text: "Fcitx5 + Lotus chuyển bằng Ctrl+Shift. Ngay lần đăng nhập đầu tiên, CaramOS App Center giúp bạn cài nhanh Zalo, ONLYOFFICE, trình duyệt và tiện ích quen thuộc."
                color: "#E2E8F0"
                font.pixelSize: 16
            }
        }
    }

    Slide {
        Image {
            anchors.fill: parent
            source: "welcome.png"
            fillMode: Image.PreserveAspectCrop
        }
        Rectangle {
            anchors.fill: parent
            color: "#AA0F172A"
        }
        Column {
            anchors.centerIn: parent
            width: parent.width * 0.82
            spacing: 12

            Text {
                anchors.horizontalCenter: parent.horizontalCenter
                text: "Kiểm tra Driver Thông minh & Giao diện Nguyên bản"
                color: "#38BDF8"
                font.pixelSize: 24
                font.bold: true
            }
            Text {
                width: parent.width
                horizontalAlignment: Text.AlignHCenter
                wrapMode: Text.WordWrap
                text: "Trình Kiểm tra Driver tự động phát hiện Wi-Fi, GPU, âm thanh và máy in — đi cùng bản sắc Cinnamon Delight, Sage Mist / Indigo Night đặc trưng của CaramOS."
                color: "#E2E8F0"
                font.pixelSize: 16
            }
        }
    }

    Slide {
        Image {
            anchors.fill: parent
            source: "welcome.png"
            fillMode: Image.PreserveAspectCrop
        }
        Rectangle {
            anchors.fill: parent
            color: "#AA0F172A"
        }
        Column {
            anchors.centerIn: parent
            width: parent.width * 0.82
            spacing: 12

            Text {
                anchors.horizontalCenter: parent.horizontalCenter
                text: "An toàn Tài khoản & Công cụ Hệ thống Rust"
                color: "#38BDF8"
                font.pixelSize: 24
                font.bold: true
            }
            Text {
                width: parent.width
                horizontalAlignment: Text.AlignHCenter
                wrapMode: Text.WordWrap
                text: "Mặc định dùng tài khoản người dùng chuẩn với quyền sudo (khóa đăng nhập root), đi kèm công cụ cập nhật OTA kiểm chứng SHA-256."
                color: "#E2E8F0"
                font.pixelSize: 16
            }
        }
    }
}
"##;
    atomic_write_bytes(
        &calamares_brand_dir.join("show.qml"),
        calamares_show_qml.as_bytes(),
    )?;

    let calamares_users_conf = r#"---
defaultGroups:
    - sudo
    - audio
    - video
    - plugdev
    - netdev
    - cdrom
    - dip
    - users
autologinGroup: autologin
doAutologin: false
sudoersGroup: sudo
setRootPassword: false
doReusePassword: false
allowWeakPasswords: true
allowWeakPasswordsDefault: false
passwordRequirements:
    minLength: 4
    maxLength: -1
userShell: /bin/bash
"#;
    let users_conf_path = calamares_modules_dir.join("users.conf");
    atomic_write_bytes(&users_conf_path, calamares_users_conf.as_bytes())?;

    let calamares_desktop = r#"[Desktop Entry]
Type=Application
Version=1.0
Name=Install CaramOS 2.0
Name[vi]=Cài đặt CaramOS 2.0
GenericName=System Installer
GenericName[vi]=Trình Cài đặt Hệ điều hành
Comment=Install CaramOS 2.0 onto your computer
Comment[vi]=Cài đặt hệ điều hành CaramOS 2.0 lên máy tính (Calamares)
Exec=/usr/bin/install-debian
Icon=install-debian
Terminal=false
StartupNotify=true
Categories=System;Settings;
"#;
    atomic_write_bytes(
        &applications_dir.join("install-debian.desktop"),
        calamares_desktop.as_bytes(),
    )?;

    let app_center_desktop = r#"[Desktop Entry]
Type=Application
Version=1.0
Name=CaramOS App Center
Name[vi]=Trung tâm Ứng dụng CaramOS
Comment=Install Zalo, ONLYOFFICE, browsers and everyday apps
Comment[vi]=Chọn và cài đặt nhanh Zalo, ONLYOFFICE, trình duyệt và tiện ích
Exec=/usr/local/bin/caramos-app-center
Icon=system-software-install
Terminal=false
Categories=System;Settings;
"#;
    atomic_write_bytes(
        &applications_dir.join("caramos-app-center.desktop"),
        app_center_desktop.as_bytes(),
    )?;

    let app_center_autostart = r#"[Desktop Entry]
Type=Application
Version=1.0
Name=CaramOS App Center (First Login)
Exec=/usr/local/bin/caramos-app-center --first-boot
Icon=system-software-install
Terminal=false
X-GNOME-Autostart-enabled=true
"#;
    atomic_write_bytes(
        &autostart_dir.join("caramos-app-center.desktop"),
        app_center_autostart.as_bytes(),
    )?;

    let driver_check_desktop = r#"[Desktop Entry]
Type=Application
Version=1.0
Name=CaramOS Driver Check
Name[vi]=Kiểm tra Driver CaramOS
Comment=Inspect Wi-Fi, graphics, sound and printer drivers
Comment[vi]=Kiểm tra tương thích phần cứng và cài driver/firmware đề xuất
Exec=/usr/local/bin/caramos-driver-check
Icon=preferences-system-devices
Terminal=false
Categories=System;HardwareSettings;Settings;
"#;
    atomic_write_bytes(
        &applications_dir.join("caramos-driver-check.desktop"),
        driver_check_desktop.as_bytes(),
    )?;

    let driver_check_autostart = r#"[Desktop Entry]
Type=Application
Version=1.0
Name=CaramOS Driver Check (Autostart)
Exec=/usr/local/bin/caramos-driver-check --autostart
Icon=preferences-system-devices
Terminal=false
X-GNOME-Autostart-enabled=true
"#;
    atomic_write_bytes(
        &autostart_dir.join("caramos-driver-check.desktop"),
        driver_check_autostart.as_bytes(),
    )?;

    let updater_desktop = r#"[Desktop Entry]
Type=Application
Version=1.0
Name=CaramOS Transactional Updater (OTA)
Name[vi]=Trung tâm Cập nhật Hệ thống CaramOS (OTA)
Comment=SHA-256 Verified Atomic System Updater
Comment[vi]=Cập nhật hệ thống nguyên tử có kiểm định SHA-256
Exec=sudo /usr/local/bin/caramos-ui updater --interactive
Icon=system-software-update
Terminal=true
Categories=System;Settings;
"#;
    atomic_write_bytes(
        &applications_dir.join("caramos-updater.desktop"),
        updater_desktop.as_bytes(),
    )?;

    writeln!(
        writer,
        "{}{}✔ Đã đồng bộ giao diện GRUB2, Plymouth, Calamares, App Center & Driver Check CaramOS:{}",
        ANSI_BOLD, ANSI_SAGE, ANSI_RESET
    )
    .map_err(|e| CaramUiError::IoFailure {
        context: "In kết quả Branding".to_string(),
        path: None,
        source: e,
    })?;
    writeln!(writer, "  • Hình nền gốc      : {}", source_wallpaper.display()).map_err(|e| {
        CaramUiError::IoFailure {
            context: "In đường dẫn hình nền gốc".to_string(),
            path: None,
            source: e,
        }
    })?;
    writeln!(writer, "  • GRUB2 Theme       : {}", theme_txt_path.display()).map_err(|e| {
        CaramUiError::IoFailure {
            context: "In đường dẫn GRUB theme".to_string(),
            path: None,
            source: e,
        }
    })?;
    writeln!(writer, "  • Calamares Branding: {}", branding_desc_path.display()).map_err(|e| {
        CaramUiError::IoFailure {
            context: "In đường dẫn Calamares branding".to_string(),
            path: None,
            source: e,
        }
    })?;

    Ok(source_wallpaper)
}

pub fn compute_file_sha256(path: &Path) -> Result<String, CaramUiError> {
    validate_safe_path(path)?;
    let mut file = File::open(path).map_err(|e| CaramUiError::IoFailure {
        context: "Mở tệp để tính SHA-256".to_string(),
        path: Some(path.to_path_buf()),
        source: e,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65_536];
    loop {
        let n = file.read(&mut buf).map_err(|e| CaramUiError::IoFailure {
            context: "Đọc luồng dữ liệu SHA-256".to_string(),
            path: Some(path.to_path_buf()),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            hasher.update(chunk);
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

pub struct OtaUpdateRequest<'a> {
    pub root_dir: &'a Path,
    pub relative_target: &'a Path,
    pub payload_path: &'a Path,
    pub expected_sha256: &'a str,
    pub new_version: &'a str,
    pub channel: &'a str,
}

pub fn render_progress_bar<W: Write>(
    writer: &mut W,
    percentage: u8,
    label: &str,
) -> Result<(), CaramUiError> {
    let clamped = percentage.min(100);
    let total_blocks: usize = 28;
    let filled_blocks = (usize::from(clamped).saturating_mul(total_blocks)) / 100;
    let empty_blocks = total_blocks.saturating_sub(filled_blocks);

    let filled_str = "█".repeat(filled_blocks);
    let empty_str = "░".repeat(empty_blocks);

    writeln!(
        writer,
        "  {}[{}{}{}{}] {:>3}%{}  {}",
        ANSI_BOLD, ANSI_INDIGO, filled_str, ANSI_SLATE, empty_str, clamped, ANSI_RESET, label
    )
    .map_err(|e| CaramUiError::IoFailure {
        context: "Vẽ thanh tiến trình TUI".to_string(),
        path: None,
        source: e,
    })
}

pub fn run_ota_updater_ui<W: Write>(
    req: &OtaUpdateRequest<'_>,
    writer: &mut W,
) -> Result<(), CaramUiError> {
    validate_safe_path(req.root_dir)?;
    validate_safe_path(req.relative_target)?;
    validate_safe_path(req.payload_path)?;

    if req.relative_target.is_absolute() {
        return Err(CaramUiError::UnsafePath {
            path: req.relative_target.to_path_buf(),
            reason: "Đường dẫn tệp đích trong root_dir phải là đường dẫn tương đối",
        });
    }

    let expected_clean = req.expected_sha256.trim().to_ascii_lowercase();
    if expected_clean.len() != 64 || !expected_clean.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(CaramUiError::ValidationFailed {
            field: "expected_sha256",
            value: req.expected_sha256.to_string(),
            reason: "Mã băm SHA-256 kỳ vọng phải gồm đúng 64 ký tự hex",
        });
    }

    let target_full_path = req.root_dir.join(req.relative_target);
    let short_digest = expected_clean.get(..16).unwrap_or(expected_clean.as_str());

    writeln!(
        writer,
        "\n{}{}==============================================================================\n\
               CARAMOS 2.0 — TRUNG TÂM CẬP NHẬT HỆ THỐNG NGUYÊN TỬ (OTA)\n\
         =============================================================================={}",
        ANSI_BOLD, ANSI_INDIGO, ANSI_RESET
    )
    .map_err(|e| CaramUiError::IoFailure {
        context: "Vẽ tiêu đề TUI Updater".to_string(),
        path: None,
        source: e,
    })?;

    writeln!(
        writer,
        "{}  • Kênh phân phối : {}{}{}\n  \
           • Phiên bản đích : {}{}{}\n  \
           • Tệp cập nhật   : {}\n  \
           • Chữ ký SHA-256 : {}...{}\n",
        ANSI_WHITE,
        ANSI_BOLD,
        req.channel,
        ANSI_WHITE,
        ANSI_SAGE,
        req.new_version,
        ANSI_WHITE,
        target_full_path.display(),
        short_digest,
        ANSI_RESET
    )
    .map_err(|e| CaramUiError::IoFailure {
        context: "Hiển thị thông số gói OTA".to_string(),
        path: None,
        source: e,
    })?;

    let target_parent = target_full_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(req.root_dir);
    fs::create_dir_all(target_parent).map_err(|e| CaramUiError::IoFailure {
        context: "Tạo thư mục cha cho tệp đích OTA".to_string(),
        path: Some(target_parent.to_path_buf()),
        source: e,
    })?;

    render_progress_bar(
        writer,
        35,
        "Truyền luồng Payload vào vùng đệm & đối chiếu SHA-256...",
    )?;

    let mut src_file = File::open(req.payload_path).map_err(|e| CaramUiError::IoFailure {
        context: "Mở tệp payload OTA".to_string(),
        path: Some(req.payload_path.to_path_buf()),
        source: e,
    })?;
    let mut staging = NamedTempFile::new_in(target_parent).map_err(|e| CaramUiError::IoFailure {
        context: "Tạo tệp vùng đệm Staging OTA".to_string(),
        path: Some(target_parent.to_path_buf()),
        source: e,
    })?;
    let staging_path = staging.path().to_path_buf();

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65_536];
    loop {
        let n = src_file.read(&mut buf).map_err(|e| CaramUiError::IoFailure {
            context: "Đọc luồng gói Payload OTA".to_string(),
            path: Some(req.payload_path.to_path_buf()),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            hasher.update(chunk);
            staging.write_all(chunk).map_err(|e| CaramUiError::IoFailure {
                context: "Ghi luồng dữ liệu vào tệp đệm OTA".to_string(),
                path: Some(staging_path.clone()),
                source: e,
            })?;
        }
    }

    let actual_sha256 = hex::encode(hasher.finalize());
    if actual_sha256 != expected_clean {
        writeln!(
            writer,
            "\n{}{}✖ CẢNH BÁO BẢO MẬT: Sai lệch SHA-256! Đã hủy giao dịch nguyên tử.{}",
            ANSI_BOLD, ANSI_ROSE, ANSI_RESET
        )
        .map_err(|e| CaramUiError::IoFailure {
            context: "In cảnh báo sai lệch SHA-256".to_string(),
            path: None,
            source: e,
        })?;
        return Err(CaramUiError::DigestMismatch {
            target: req.payload_path.to_path_buf(),
            expected: expected_clean,
            actual: actual_sha256,
        });
    }

    staging
        .as_file()
        .sync_all()
        .map_err(|e| CaramUiError::IoFailure {
            context: "Đồng bộ fsync vùng đệm OTA".to_string(),
            path: Some(staging_path),
            source: e,
        })?;

    if let Ok(orig_meta) = fs::metadata(&target_full_path) {
        let _ = fs::set_permissions(staging.path(), orig_meta.permissions());
    } else {
        set_unix_mode(staging.path(), 0o644)?;
    }

    render_progress_bar(writer, 65, "Tạo điểm khôi phục nguyên tử (Atomic Backup)...")?;
    let backup_path = append_filename_suffix(&target_full_path, ".caramos-ota.bak");
    let had_existing_target = target_full_path.exists();

    if had_existing_target {
        fs::copy(&target_full_path, &backup_path).map_err(|e| CaramUiError::IoFailure {
            context: "Sao lưu tệp gốc trước khi nâng cấp".to_string(),
            path: Some(backup_path.clone()),
            source: e,
        })?;
    }

    render_progress_bar(writer, 85, "Hoán đổi nguyên tử tệp đích...")?;
    if let Err(persist_err) = staging.persist(&target_full_path) {
        let io_err = persist_err.error;
        if had_existing_target && backup_path.exists() {
            if let Err(rb_err) = fs::rename(&backup_path, &target_full_path) {
                return Err(CaramUiError::RollbackFailure {
                    target: target_full_path,
                    original_err: io_err.to_string(),
                    rollback_err: rb_err,
                });
            }
        }
        return Err(CaramUiError::IoFailure {
            context: "Hoán đổi nguyên tử tệp đích OTA".to_string(),
            path: Some(target_full_path),
            source: io_err,
        });
    }

    render_progress_bar(writer, 100, "Hậu kiểm SHA-256 & Dọn dẹp bản sao lưu...")?;
    let committed_sha256 = compute_file_sha256(&target_full_path)?;
    if committed_sha256 != expected_clean {
        if had_existing_target && backup_path.exists() {
            let _ = fs::rename(&backup_path, &target_full_path);
        }
        return Err(CaramUiError::DigestMismatch {
            target: target_full_path,
            expected: expected_clean,
            actual: committed_sha256,
        });
    }

    if backup_path.exists() {
        let _ = fs::remove_file(&backup_path);
    }

    writeln!(
        writer,
        "\n{}{}✔ CẬP NHẬT HỆ THỐNG THÀNH CÔNG LÊN PHIÊN BẢN {}! (SHA-256 Verified){}",
        ANSI_BOLD, ANSI_SAGE, req.new_version, ANSI_RESET
    )
    .map_err(|e| CaramUiError::IoFailure {
        context: "Ghi thông báo hoàn tất OTA".to_string(),
        path: None,
        source: e,
    })?;

    Ok(())
}

fn prompt_line_with_default(prompt: &str, default_val: &str) -> Result<String, CaramUiError> {
    let mut out = io::stdout().lock();
    write!(
        out,
        "{}{}  ? {}{} [Mặc định: {}{}{}]: ",
        ANSI_BOLD, ANSI_SKY, prompt, ANSI_RESET, ANSI_INDIGO, default_val, ANSI_RESET
    )
    .map_err(|e| CaramUiError::IoFailure {
        context: "Hiển thị lời nhắc nhập liệu".to_string(),
        path: None,
        source: e,
    })?;
    out.flush().map_err(|e| CaramUiError::IoFailure {
        context: "Xả bộ đệm stdout".to_string(),
        path: None,
        source: e,
    })?;

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| CaramUiError::IoFailure {
        context: "Đọc dữ liệu từ bàn phím".to_string(),
        path: None,
        source: e,
    })?;

    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(default_val.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "caramos-ui",
    version,
    about = "CaramOS 2.0 Branding Stager (GRUB2, Plymouth, Calamares, Desktop Entries) and Atomic OTA TUI"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Chuẩn bị giao diện GRUB2, Plymouth, Calamares & phím tắt App Center / Driver Check
    #[command(alias = "bootloader")]
    Branding {
        #[arg(long, default_value = "./build/ui_overlay")]
        output_root: PathBuf,
        #[arg(long)]
        wallpaper: Option<PathBuf>,
        #[arg(long, default_value = "./config/includes.chroot/usr/share/backgrounds")]
        wallpaper_search_dir: PathBuf,
        #[arg(long, default_value = "./assets")]
        assets_dir: PathBuf,
        #[arg(long, default_value_t = 1280)]
        width: u16,
        #[arg(long, default_value_t = 720)]
        height: u16,
    },
    /// Bảng điều khiển cập nhật OTA TUI kèm xác thực SHA-256 & Rollback nguyên tử
    Updater {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        target: Option<PathBuf>,
        #[arg(long)]
        payload: Option<PathBuf>,
        #[arg(long)]
        sha256: Option<String>,
        #[arg(long, default_value = "2.0.1")]
        version: String,
        #[arg(long, default_value = "stable")]
        channel: String,
        #[arg(long)]
        interactive: bool,
    },
}

pub fn run_cli(args: &[String]) -> Result<(), CaramUiError> {
    let _guard = TerminalSessionGuard::new(true).map_err(|e| CaramUiError::IoFailure {
        context: "Khởi tạo phiên Terminal".to_string(),
        path: None,
        source: e,
    })?;

    let mut stdout = io::stdout().lock();

    let cli = match Cli::try_parse_from(args) {
        Ok(parsed) => parsed,
        Err(err) => {
            if matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                write!(stdout, "{}", err).map_err(|e| CaramUiError::IoFailure {
                    context: "In hướng dẫn sử dụng".to_string(),
                    path: None,
                    source: e,
                })?;
                return Ok(());
            }
            return Err(CaramUiError::InvalidCliArgument {
                argument: "cli".to_string(),
                reason: err.to_string(),
            });
        }
    };

    match cli.command {
        Commands::Branding {
            output_root,
            wallpaper,
            wallpaper_search_dir,
            assets_dir,
            width,
            height,
        } => {
            let cfg = BrandingStageConfig {
                output_root: &output_root,
                explicit_wallpaper: wallpaper.as_deref(),
                wallpaper_search_dir: &wallpaper_search_dir,
                assets_dir: &assets_dir,
                width,
                height,
            };
            let _ = build_caramos_branding_overlay(&cfg, &mut stdout)?;
            Ok(())
        }
        Commands::Updater {
            mut root,
            mut target,
            mut payload,
            mut sha256,
            mut version,
            mut channel,
            interactive,
        } => {
            if interactive {
                drop(stdout);
                if root.is_none() {
                    root = Some(PathBuf::from(prompt_line_with_default(
                        "Thư mục gốc hệ thống (--root)",
                        "/",
                    )?));
                }
                if target.is_none() {
                    target = Some(PathBuf::from(prompt_line_with_default(
                        "Đường dẫn tương đối tệp đích (--target)",
                        "etc/caramos-release",
                    )?));
                }
                if payload.is_none() {
                    payload = Some(PathBuf::from(prompt_line_with_default(
                        "Đường dẫn tệp gói cập nhật (--payload)",
                        "",
                    )?));
                }
                if sha256.is_none() {
                    sha256 = Some(prompt_line_with_default(
                        "Mã băm SHA-256 kỳ vọng (64 hex)",
                        "",
                    )?);
                }
                version = prompt_line_with_default("Phiên bản đích", &version)?;
                channel = prompt_line_with_default("Kênh cập nhật", &channel)?;
                stdout = io::stdout().lock();
            }

            let root_dir = root.ok_or_else(|| CaramUiError::InvalidCliArgument {
                argument: "--root".to_string(),
                reason: "Bắt buộc cung cấp --root <DIR>".to_string(),
            })?;
            let rel_target = target.ok_or_else(|| CaramUiError::InvalidCliArgument {
                argument: "--target".to_string(),
                reason: "Bắt buộc cung cấp --target <REL_PATH>".to_string(),
            })?;
            let payload_path = payload.ok_or_else(|| CaramUiError::InvalidCliArgument {
                argument: "--payload".to_string(),
                reason: "Bắt buộc cung cấp --payload <FILE>".to_string(),
            })?;
            let expected_sha256 = sha256.ok_or_else(|| CaramUiError::InvalidCliArgument {
                argument: "--sha256".to_string(),
                reason: "Bắt buộc cung cấp --sha256 <HEX_64>".to_string(),
            })?;

            let req = OtaUpdateRequest {
                root_dir: &root_dir,
                relative_target: &rel_target,
                payload_path: &payload_path,
                expected_sha256: &expected_sha256,
                new_version: &version,
                channel: &channel,
            };
            run_ota_updater_ui(&req, &mut stdout)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    match run_cli(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "{}{}✖ {}{}", ANSI_BOLD, ANSI_ROSE, err, ANSI_RESET);
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbImage;
    use tempfile::TempDir;

    #[test]
    fn test_branding_stages_calamares_app_center_and_driver_check() -> Result<(), CaramUiError> {
        let temp_base = TempDir::new().map_err(|e| CaramUiError::IoFailure {
            context: "Tạo TempDir test branding".to_string(),
            path: None,
            source: e,
        })?;
        let bg_dir = temp_base.path().join("backgrounds");
        let assets_dir = temp_base.path().join("assets");
        let out_dir = temp_base.path().join("overlay");
        fs::create_dir_all(&bg_dir).map_err(|e| CaramUiError::IoFailure {
            context: "Tạo thư mục backgrounds test".to_string(),
            path: Some(bg_dir.clone()),
            source: e,
        })?;
        fs::create_dir_all(&assets_dir).map_err(|e| CaramUiError::IoFailure {
            context: "Tạo thư mục assets test".to_string(),
            path: Some(assets_dir.clone()),
            source: e,
        })?;

        let existing_bg = bg_dir.join("caramos-indigo-night.png");
        RgbImage::from_pixel(64, 64, image::Rgb([15, 23, 42]))
            .save_with_format(&existing_bg, ImageFormat::Png)
            .map_err(|e| CaramUiError::ImageProcessingFailed {
                path: existing_bg.clone(),
                reason: e.to_string(),
            })?;

        let mut log = Vec::new();
        let cfg = BrandingStageConfig {
            output_root: &out_dir,
            explicit_wallpaper: None,
            wallpaper_search_dir: &bg_dir,
            assets_dir: &assets_dir,
            width: 320,
            height: 240,
        };
        let _ = build_caramos_branding_overlay(&cfg, &mut log)?;

        assert!(out_dir
            .join("usr/share/applications/caramos-app-center.desktop")
            .is_file());
        assert!(out_dir
            .join("etc/xdg/autostart/caramos-app-center.desktop")
            .is_file());
        assert!(out_dir
            .join("usr/share/applications/caramos-driver-check.desktop")
            .is_file());
        assert!(out_dir
            .join("etc/xdg/autostart/caramos-driver-check.desktop")
            .is_file());
        assert!(out_dir
            .join("etc/calamares/branding/caramos/branding.desc")
            .is_file());

        Ok(())
    }
}

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

mod control;
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
#[command(name = "caramos-ui", version, about = "CaramOS 2.0 Branding, Control Backend, Theme Applier & OTA")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Dựng cây overlay GRUB2, Plymouth, Slick Greeter, Calamares Slideshow, Island Dock & Control Center
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
    /// Backend Rust điều khiển hệ thống cho applet Control Center trên thanh tác vụ
    Control {
        #[command(subcommand)]
        action: control::ControlAction,
    },
    /// Hoàn tất cấu hình người dùng mới (fcitx5-lotus-server + quyền uinput) sau khi cài Calamares
    PostInstall,
    /// Kiểm tra nhanh bản phát hành OTA dạng ANSI trên terminal
    Ota {
        #[arg(long, default_value = "https://api.github.com/repos/LocShadowVN/CaramOS-testing/releases/latest")]
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
        context: format!("Ghi '{}'", dest.display()),
        source: e,
    })?;
    tmp.as_file().sync_all().map_err(|e| CaramUiError::Io {
        context: format!("fsync '{}'", dest.display()),
        source: e,
    })?;
    #[cfg(unix)]
    {
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode)).map_err(|e| CaramUiError::Io {
            context: format!("Đặt quyền {:o} cho '{}'", mode, dest.display()),
            source: e,
        })?;
    }
    #[cfg(not(unix))]
    let _ = mode;

    tmp.persist(dest).map_err(|e| CaramUiError::Io {
        context: format!("Lưu '{}'", dest.display()),
        source: e.error,
    })?;
    Ok(())
}

fn encode_png(img: &RgbaImage, label: &str) -> Result<Vec<u8>, CaramUiError> {
    let mut bytes = Vec::new();
    let mut cursor = Cursor::new(&mut bytes);
    img.write_to(&mut cursor, ImageFormat::Png).map_err(|e| CaramUiError::Image {
        context: label.to_string(),
        detail: e.to_string(),
    })?;
    Ok(bytes)
}

fn save_png_atomically(dest: &Path, img: &RgbaImage) -> Result<(), CaramUiError> {
    let bytes = encode_png(img, &dest.display().to_string())?;
    atomic_write_bytes(dest, &bytes)
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

/// Dựng hình nền sáng Sage Mist chuẩn CaramOS (sương xanh dịu, không đóng dấu logo giữa màn hình).
fn render_sage_mist_wallpaper(width: u32, height: u32) -> RgbaImage {
    let w = width.max(640);
    let h = height.max(480);
    let mut img = RgbaImage::new(w, h);
    let dx = (w.saturating_sub(1)).max(1) as f32;
    let dy = (h.saturating_sub(1)).max(1) as f32;

    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let u = x as f32 / dx;
        let v = y as f32 / dy;
        let wave1 = (u * 3.8 + v * 1.4).sin() * 0.5 + 0.5;
        let wave2 = (u * 2.1 - v * 2.7 + 1.2).cos() * 0.5 + 0.5;
        let glow_x = u - 0.72;
        let glow_y = v - 0.28;
        let glow = (1.0 - (glow_x * glow_x + glow_y * glow_y).sqrt() * 1.35).clamp(0.0, 1.0);
        let hill = (v - (0.58 + 0.08 * (u * 4.2).sin() + 0.04 * (u * 7.5).cos())).clamp(0.0, 1.0);

        let r = 216.0 - 44.0 * v - 22.0 * hill + 18.0 * glow + 10.0 * wave1;
        let g = 232.0 - 26.0 * v - 18.0 * hill + 16.0 * glow + 11.0 * wave2;
        let b = 222.0 - 32.0 * v - 20.0 * hill + 14.0 * glow + 8.0 * wave1;
        *pixel = Rgba([
            r.clamp(0.0, 255.0) as u8,
            g.clamp(0.0, 255.0) as u8,
            b.clamp(0.0, 255.0) as u8,
            255,
        ]);
    }
    img
}

/// Dựng hình nền tối Indigo Night chuẩn CaramOS (xanh chàm - lục bảo sâu, tương phản tốt cho Dark mode & GRUB).
fn render_indigo_night_wallpaper(width: u32, height: u32) -> RgbaImage {
    let w = width.max(640);
    let h = height.max(480);
    let mut img = RgbaImage::new(w, h);
    let dx = (w.saturating_sub(1)).max(1) as f32;
    let dy = (h.saturating_sub(1)).max(1) as f32;

    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let u = x as f32 / dx;
        let v = y as f32 / dy;
        let aurora = ((u * 4.0 + (v * 3.0).sin() * 0.6).sin() * 0.5 + 0.5) * (1.0 - (v - 0.42).abs() * 2.1).clamp(0.0, 1.0);
        let gx = u - 0.28;
        let gy = v - 0.65;
        let emerald_glow = (1.0 - (gx * gx + gy * gy).sqrt() * 1.5).clamp(0.0, 1.0);

        let r = 17.0 + 18.0 * (1.0 - v) + 20.0 * aurora + 8.0 * emerald_glow;
        let g = 25.0 + 22.0 * (1.0 - v) + 46.0 * aurora + 38.0 * emerald_glow;
        let b = 46.0 + 36.0 * (1.0 - v) + 52.0 * aurora + 26.0 * emerald_glow;
        *pixel = Rgba([
            r.clamp(0.0, 255.0) as u8,
            g.clamp(0.0, 255.0) as u8,
            b.clamp(0.0, 255.0) as u8,
            255,
        ]);
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

fn stage_control_center_applet(output_root: &Path) -> Result<(), CaramUiError> {
    let dir = output_root.join("usr/share/cinnamon/applets/caramos-control-center@caramos");

    let metadata = r##"{
  "uuid": "caramos-control-center@caramos",
  "name": "Trung tâm Điều khiển CaramOS",
  "description": "Bảng điều khiển nhanh Giao diện, 8 màu nhấn, Ánh sáng đêm, Nguồn, Âm lượng và Độ sáng (Backend Rust)",
  "icon": "preferences-system-symbolic",
  "max-instances": 1
}
"##;

    let stylesheet = r##".caramos-cc-grid { spacing: 8px; padding: 6px 12px; }
.caramos-cc-pill {
    border-radius: 10px;
    padding: 8px 12px;
    background-color: rgba(128, 128, 128, 0.14);
    font-weight: 600;
}
.caramos-cc-pill:hover { background-color: rgba(43, 106, 68, 0.22); }
.caramos-cc-pill-active {
    background-color: #2b6a44;
    color: #ffffff;
}
.caramos-cc-swatches { spacing: 8px; padding: 6px 12px; }
.caramos-cc-swatch {
    width: 22px;
    height: 22px;
    border-radius: 11px;
    border: 2px solid rgba(255, 255, 255, 0.65);
}
.caramos-cc-section {
    font-weight: 600;
    font-size: 9pt;
    padding: 4px 12px 2px 12px;
    opacity: 0.72;
}
"##;

    let applet_js = r##"const Applet = imports.ui.applet;
const PopupMenu = imports.ui.popupMenu;
const St = imports.gi.St;
const Gio = imports.gi.Gio;
const GLib = imports.gi.GLib;

const CTL = "/usr/local/bin/caramos-ui";
const ACCENTS = [
    ["Xanh rừng", "#2b6a44"],
    ["Xanh lam", "#1d6fa5"],
    ["Chàm", "#4b5cc4"],
    ["Tím", "#7c4dff"],
    ["Hồng", "#c23b75"],
    ["Đỏ gạch", "#c04632"],
    ["Cam đất", "#c46d23"],
    ["Xám đá", "#4d6157"]
];

function spawnCtl(args) {
    try {
        GLib.spawn_async(null, [CTL, "control"].concat(args), null, GLib.SpawnFlags.SEARCH_PATH, null);
    } catch (e) {
        global.logError("CaramOS CC spawn error: " + e);
    }
}

class CaramControlCenterApplet extends Applet.IconApplet {
    constructor(metadata, orientation, panelHeight, instanceId) {
        super(orientation, panelHeight, instanceId);
        this.set_applet_icon_symbolic_name("emblem-system-symbolic");
        this.set_applet_tooltip("Trung tâm Điều khiển CaramOS");

        this.menuManager = new PopupMenu.PopupMenuManager(this);
        this.menu = new Applet.AppletPopupMenu(this, orientation);
        this.menuManager.addMenu(this.menu);

        this._buildMenu();
    }

    _buildMenu() {
        let topBox = new St.BoxLayout({ style_class: "caramos-cc-grid" });
        this._darkBtn = this._makePill("Chế độ Tối", () => {
            spawnCtl(["toggle-dark"]);
            this._refreshLater();
        });
        this._nightBtn = this._makePill("Ánh sáng đêm", () => {
            spawnCtl(["toggle-night-light"]);
            this._refreshLater();
        });
        let shotBtn = this._makePill("Chụp ảnh", () => {
            this.menu.close();
            spawnCtl(["action", "screenshot"]);
        });
        topBox.add_actor(this._darkBtn);
        topBox.add_actor(this._nightBtn);
        topBox.add_actor(shotBtn);
        this.menu.box.add_actor(topBox);

        this._addSection("Màu nhấn CaramOS");
        let swatchBox = new St.BoxLayout({ style_class: "caramos-cc-swatches" });
        for (let [label, hex] of ACCENTS) {
            let b = new St.Button({
                style_class: "caramos-cc-swatch",
                style: "background-color: " + hex + ";",
                reactive: true,
                can_focus: true
            });
            b.connect("clicked", () => spawnCtl(["set-accent", hex]));
            swatchBox.add_actor(b);
        }
        this.menu.box.add_actor(swatchBox);

        this._addSection("Chế độ nguồn");
        let pwrBox = new St.BoxLayout({ style_class: "caramos-cc-grid" });
        this._pwrBtns = {};
        for (let [id, lbl] of [["power-saver", "Tiết kiệm"], ["balanced", "Cân bằng"], ["performance", "Hiệu năng"]]) {
            let btn = this._makePill(lbl, () => {
                spawnCtl(["set-power", id]);
                this._refreshLater();
            });
            this._pwrBtns[id] = btn;
            pwrBox.add_actor(btn);
        }
        this.menu.box.add_actor(pwrBox);

        this._addSection("Âm lượng & Độ sáng");
        this._volSlider = new PopupMenu.PopupSliderMenuItem(0.75);
        this._volSlider.connect("value-changed", (s, val) => {
            spawnCtl(["set-volume", String(Math.round(val * 100))]);
        });
        this.menu.addMenuItem(this._volSlider);

        this._brightSlider = new PopupMenu.PopupSliderMenuItem(0.8);
        this._brightSlider.connect("value-changed", (s, val) => {
            spawnCtl(["set-brightness", String(Math.max(5, Math.round(val * 100)))]);
        });
        this.menu.addMenuItem(this._brightSlider);

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._addQuickItem("Trung tâm Ứng dụng", "apps");
        this._addQuickItem("Trung tâm Cập nhật", "updates");
        this._addQuickItem("Kiểm tra Driver", "drivers");
        this._addQuickItem("Cài đặt Mạng", "network");
        this._addQuickItem("Cài đặt Hệ thống", "settings");

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        let footBox = new St.BoxLayout({ style_class: "caramos-cc-grid" });
        footBox.add_actor(this._makePill("Khóa", () => { this.menu.close(); spawnCtl(["action", "lock"]); }));
        footBox.add_actor(this._makePill("Khởi động lại", () => { this.menu.close(); spawnCtl(["action", "reboot"]); }));
        footBox.add_actor(this._makePill("Tắt máy", () => { this.menu.close(); spawnCtl(["action", "poweroff"]); }));
        this.menu.box.add_actor(footBox);
    }

    _makePill(label, cb) {
        let btn = new St.Button({ label: label, style_class: "caramos-cc-pill", reactive: true, can_focus: true });
        btn.connect("clicked", cb);
        return btn;
    }

    _addSection(text) {
        this.menu.box.add_actor(new St.Label({ text: text, style_class: "caramos-cc-section" }));
    }

    _addQuickItem(label, actionName) {
        let item = new PopupMenu.PopupMenuItem(label);
        item.connect("activate", () => spawnCtl(["action", actionName]));
        this.menu.addMenuItem(item);
    }

    _refreshLater() {
        GLib.timeout_add(GLib.PRIORITY_DEFAULT, 250, () => {
            this._refreshStatus();
            return GLib.SOURCE_REMOVE;
        });
    }

    _refreshStatus() {
        try {
            let proc = Gio.Subprocess.new([CTL, "control", "status"], Gio.SubprocessFlags.STDOUT_PIPE);
            proc.communicate_utf8_async(null, null, (p, res) => {
                try {
                    let [ok, stdout] = p.communicate_utf8_finish(res);
                    if (!ok || !stdout) return;
                    let st = JSON.parse(stdout.trim());
                    this._darkBtn.set_style_class_name(st.dark ? "caramos-cc-pill caramos-cc-pill-active" : "caramos-cc-pill");
                    this._nightBtn.set_style_class_name(st.night_light ? "caramos-cc-pill caramos-cc-pill-active" : "caramos-cc-pill");
                    for (let k in this._pwrBtns) {
                        this._pwrBtns[k].set_style_class_name(
                            st.power === k ? "caramos-cc-pill caramos-cc-pill-active" : "caramos-cc-pill"
                        );
                    }
                    this._volSlider.setValue((st.volume || 0) / 100.0);
                    this._brightSlider.setValue((st.brightness || 100) / 100.0);
                } catch (e) {
                    global.logError("CaramOS CC parse error: " + e);
                }
            });
        } catch (e) {
            global.logError("CaramOS CC status error: " + e);
        }
    }

    on_applet_clicked() {
        this.menu.toggle();
        if (this.menu.isOpen) {
            this._refreshStatus();
        }
    }
}

function main(metadata, orientation, panelHeight, instanceId) {
    return new CaramControlCenterApplet(metadata, orientation, panelHeight, instanceId);
}
"##;

    atomic_write_bytes(&dir.join("metadata.json"), metadata.as_bytes())?;
    atomic_write_bytes(&dir.join("stylesheet.css"), stylesheet.as_bytes())?;
    atomic_write_bytes(&dir.join("applet.js"), applet_js.as_bytes())?;
    Ok(())
}

fn build_caramos_branding_overlay(
    output_root: &Path,
    _wallpaper_dir: Option<&Path>,
    assets_dir: Option<&Path>,
    width: u32,
    height: u32,
) -> Result<(), CaramUiError> {
    if width < 640 || height < 480 {
        return Err(CaramUiError::InvalidArgs("Độ phân giải tối thiểu là 640x480".to_string()));
    }

    // Dựng bộ đôi hình nền Sage Mist (Sáng) & Indigo Night (Tối) chuẩn CaramOS 2.0 (không dán logo giữa nền)
    let sage_bytes = encode_png(&render_sage_mist_wallpaper(width, height), "sage-mist.png")?;
    let indigo_bytes = encode_png(&render_indigo_night_wallpaper(width, height), "indigo-night.png")?;

    let logo_img = if let Some(lp) = assets_dir.and_then(discover_existing_logo) {
        image::open(&lp).map(|i| i.to_rgba8()).unwrap_or_else(|_| render_fallback_logo(128))
    } else {
        render_fallback_logo(128)
    };

    let bg_dir = output_root.join("usr/share/backgrounds/caramos");
    let grub_dir = output_root.join("boot/grub/themes/caramos");
    let calamares_dir = output_root.join("etc/calamares/branding/caramos");
    let plymouth_dir = output_root.join("usr/share/plymouth/themes/caramos");

    atomic_write_bytes(&bg_dir.join("sage-mist.png"), &sage_bytes)?;
    atomic_write_bytes(&bg_dir.join("indigo-night.png"), &indigo_bytes)?;
    atomic_write_bytes(&bg_dir.join("default.png"), &sage_bytes)?;
    atomic_write_bytes(&output_root.join("usr/share/backgrounds/caramos-login.png"), &indigo_bytes)?;
    atomic_write_bytes(&grub_dir.join("background.png"), &indigo_bytes)?;
    atomic_write_bytes(&calamares_dir.join("welcome.png"), &indigo_bytes)?;

    save_png_atomically(&output_root.join("usr/share/pixmaps/caramos-logo.png"), &logo_img)?;
    save_png_atomically(&calamares_dir.join("logo.png"), &logo_img)?;
    save_png_atomically(&plymouth_dir.join("logo.png"), &logo_img)?;

    let grub_theme = r##"title-text: "CaramOS 2.0 (Debian 13)"
desktop-image: "background.png"
desktop-color: "#11192e"
terminal-font: "Unifont Regular 16"

+ boot_menu {
    left = 18%
    top = 30%
    width = 64%
    height = 42%
    item_font = "Unifont Regular 16"
    item_color = "#d6e6dc"
    selected_item_color = "#ffffff"
    item_height = 36
    item_spacing = 10
}

+ label {
    top = 86%
    left = 0
    width = 100%
    align = "center"
    font = "Unifont Regular 16"
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
    let plymouth_script = r##"Window.SetBackgroundTopColor(0.07, 0.11, 0.19);
Window.SetBackgroundBottomColor(0.06, 0.16, 0.14);
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
windowSize: 920px,580px
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
    SidebarBackground:        "#15222c"
    SidebarText:              "#e5efe9"
    SidebarTextCurrent:       "#ffffff"
    SidebarBackgroundCurrent: "#2b6a44"
"##;

    // Slideshow 4 trang giới thiệu CaramOS trong khi Calamares cài đặt (chuẩn CaramOS gốc)
    let calamares_qml = r##"import QtQuick 2.0;

Item {
    id: root
    property int currentSlide: 0
    property var slides: [
        {
            tag: "01 / 04 — CHÀO MỪNG",
            title: "CaramOS 2.0 — Linux thuần Việt trên nền Debian 13",
            body: "Nhẹ, ổn định, bảo mật với toàn bộ công cụ lõi viết bằng Rust. Giao diện Sáng – Tối đồng bộ một chạm cùng thanh tác vụ 3 đảo nổi hiện đại."
        },
        {
            tag: "02 / 04 — TIẾNG VIỆT TỪ ĐẦU",
            title: "Bộ gõ Fcitx5 Lotus & Phông Be Vietnam Pro",
            body: "Gõ tiếng Việt Telex mượt mà ngay sau khi cài xong, không gạch chân khó chịu. Chuyển nhanh Anh/Việt bằng Ctrl + Shift, đầy đủ múi giờ và lịch Việt Nam."
        },
        {
            tag: "03 / 04 — ĐỦ ỨNG DỤNG QUEN THUỘC",
            title: "Trung tâm Ứng dụng & Kiểm tra Driver một chạm",
            body: "Chọn cài ONLYOFFICE, WPS, Google Chrome, Cốc Cốc, Zalo, phần mềm Giáo dục (Scratch 3, GeoGebra, Thonny) và tự chẩn đoán Wi-Fi, GPU, máy in."
        },
        {
            tag: "04 / 04 — CẬP NHẬT LIỀN MẠCH",
            title: "Nâng cấp trực tiếp không cần cài lại máy",
            body: "Trung tâm Cập nhật tự động theo dõi bản phát hành mới từ GitHub cùng bản vá bảo mật Debian 13 và Flathub — an toàn và không mất dữ liệu cá nhân."
        }
    ]

    Image {
        anchors.fill: parent
        source: "welcome.png"
        fillMode: Image.PreserveAspectCrop
    }

    Rectangle {
        anchors.fill: parent
        color: "#880f1822"
    }

    Timer {
        interval: 6500
        running: true
        repeat: true
        onTriggered: root.currentSlide = (root.currentSlide + 1) % root.slides.length
    }

    Rectangle {
        anchors.centerIn: parent
        width: parent.width * 0.84
        height: parent.height * 0.64
        radius: 16
        color: "#d816222c"
        border.color: "#448ccfa5"
        border.width: 1

        Column {
            anchors.fill: parent
            anchors.margins: 32
            spacing: 14

            Row {
                spacing: 12
                Image {
                    source: "logo.png"
                    width: 44
                    height: 44
                    fillMode: Image.PreserveAspectFit
                }
                Text {
                    anchors.verticalCenter: parent.verticalCenter
                    text: root.slides[root.currentSlide].tag
                    color: "#8ccfa5"
                    font.pixelSize: 13
                    font.bold: true
                }
            }

            Text {
                width: parent.width
                text: root.slides[root.currentSlide].title
                color: "#ffffff"
                font.pixelSize: 24
                font.bold: true
                wrapMode: Text.WordWrap
            }

            Text {
                width: parent.width
                text: root.slides[root.currentSlide].body
                color: "#dce8e1"
                font.pixelSize: 15
                lineHeight: 1.45
                wrapMode: Text.WordWrap
            }
        }
    }

    Row {
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.bottom: parent.bottom
        anchors.bottomMargin: 22
        spacing: 10
        Repeater {
            model: root.slides.length
            Rectangle {
                width: index === root.currentSlide ? 26 : 10
                height: 10
                radius: 5
                color: index === root.currentSlide ? "#3a855a" : "#66ffffff"
            }
        }
    }
}
"##;
    atomic_write_bytes(&calamares_dir.join("branding.desc"), calamares_desc.as_bytes())?;
    atomic_write_bytes(&calamares_dir.join("show.qml"), calamares_qml.as_bytes())?;

    let lightdm_conf = r##"[Seat:*]
user-session=cinnamon
autologin-session=cinnamon
greeter-session=slick-greeter
greeter-hide-users=false
"##;
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
    atomic_write_bytes(&output_root.join("etc/lightdm/lightdm.conf.d/50-caramos.conf"), lightdm_conf.as_bytes())?;
    atomic_write_bytes(&output_root.join("etc/lightdm/slick-greeter.conf"), slick_conf.as_bytes())?;

    let island_light_css = r##"stage { font-family: "Be Vietnam Pro", "Noto Sans", sans-serif; font-size: 10pt; color: #1f2923; }
.panel-bottom { height: 48px; background-color: transparent; padding: 4px 12px; }
.panelLeft, .panelCenter, .panelRight {
    background-color: rgba(248, 250, 248, 0.92);
    border: 1px solid rgba(43, 106, 68, 0.20);
    border-radius: 14px;
    padding: 0 10px;
    box-shadow: 0 3px 12px rgba(0, 0, 0, 0.14);
}
.grouped-window-list-item-box { border-radius: 10px; padding: 0 6px; transition-duration: 120ms; }
.grouped-window-list-item-box:hover { background-color: rgba(43, 106, 68, 0.10); }
.grouped-window-list-item-box:checked, .grouped-window-list-item-box:active {
    border-bottom: 3px solid #2b6a44;
    background-color: rgba(43, 106, 68, 0.14);
}
.menu, .popup-menu-boxpointer {
    -arrow-border-radius: 14px;
    -arrow-background-color: rgba(250, 252, 250, 0.96);
    -arrow-border-color: rgba(43, 106, 68, 0.18);
    -arrow-border-width: 1px;
    background-color: rgba(250, 252, 250, 0.96);
    color: #1f2923;
}
"##;
    let island_dark_css = r##"stage { font-family: "Be Vietnam Pro", "Noto Sans", sans-serif; font-size: 10pt; color: #eef3f0; }
.panel-bottom { height: 48px; background-color: transparent; padding: 4px 12px; }
.panelLeft, .panelCenter, .panelRight {
    background-color: rgba(24, 30, 27, 0.92);
    border: 1px solid rgba(140, 207, 165, 0.20);
    border-radius: 14px;
    padding: 0 10px;
    box-shadow: 0 3px 14px rgba(0, 0, 0, 0.35);
}
.grouped-window-list-item-box { border-radius: 10px; padding: 0 6px; transition-duration: 120ms; }
.grouped-window-list-item-box:hover { background-color: rgba(58, 133, 90, 0.18); }
.grouped-window-list-item-box:checked, .grouped-window-list-item-box:active {
    border-bottom: 3px solid #3a855a;
    background-color: rgba(58, 133, 90, 0.26);
}
.menu, .popup-menu-boxpointer {
    -arrow-border-radius: 14px;
    -arrow-background-color: rgba(24, 30, 27, 0.96);
    -arrow-border-color: rgba(140, 207, 165, 0.18);
    -arrow-border-width: 1px;
    background-color: rgba(24, 30, 27, 0.96);
    color: #eef3f0;
}
"##;
    atomic_write_bytes(&output_root.join("usr/share/themes/CaramOS-Island/cinnamon/cinnamon.css"), island_light_css.as_bytes())?;
    atomic_write_bytes(&output_root.join("usr/share/themes/CaramOS-Island-Dark/cinnamon/cinnamon.css"), island_dark_css.as_bytes())?;

    stage_control_center_applet(output_root)?;

    let apps_dir = output_root.join("usr/share/applications");
    let autostart_dir = output_root.join("etc/xdg/autostart");

    let desktop_files: &[(&str, &str)] = &[
        (
            "caramos-app-center.desktop",
            r##"[Desktop Entry]
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
"##,
        ),
        (
            "caramos-app-center-link.desktop",
            r##"[Desktop Entry]
Type=Application
Version=1.0
Name=Cài đặt ứng dụng CaramOS
Exec=/usr/local/bin/caramos-app-center %u
Icon=system-software-install
Terminal=false
NoDisplay=true
MimeType=x-scheme-handler/caramos-app;
"##,
        ),
        (
            "caramos-driver-check.desktop",
            r##"[Desktop Entry]
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
"##,
        ),
        (
            "caramos-update-center.desktop",
            r##"[Desktop Entry]
Type=Application
Version=1.0
Name=Trung tâm Cập nhật
GenericName=Cập nhật hệ thống CaramOS
Comment=Cập nhật hệ thống CaramOS, Debian 13 và ứng dụng Flathub an toàn
Exec=/usr/local/bin/caramos-update-center
Icon=system-software-update
Terminal=false
Categories=System;Settings;
StartupNotify=true
"##,
        ),
    ];
    for (name, body) in desktop_files {
        atomic_write_bytes_with_mode(&apps_dir.join(name), body.as_bytes(), 0o644)?;
    }

    let autostart_files: &[(&str, &str)] = &[
        (
            "caramos-app-center-firstboot.desktop",
            r##"[Desktop Entry]
Type=Application
Name=CaramOS App Center First Boot
Exec=/usr/local/bin/caramos-app-center --first-boot
X-GNOME-Autostart-enabled=true
NoDisplay=true
OnlyShowIn=X-Cinnamon;
"##,
        ),
        (
            "caramos-driver-check-autostart.desktop",
            r##"[Desktop Entry]
Type=Application
Name=CaramOS Driver Check Autostart
Exec=/usr/local/bin/caramos-driver-check --autostart
X-GNOME-Autostart-enabled=true
NoDisplay=true
OnlyShowIn=X-Cinnamon;
"##,
        ),
        (
            "caramos-update-notifier.desktop",
            r##"[Desktop Entry]
Type=Application
Name=CaramOS Update Notifier
Exec=/usr/local/bin/caramos-update-center --check
X-GNOME-Autostart-enabled=true
NoDisplay=true
OnlyShowIn=X-Cinnamon;
"##,
        ),
    ];
    for (name, body) in autostart_files {
        atomic_write_bytes_with_mode(&autostart_dir.join(name), body.as_bytes(), 0o644)?;
    }

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
                if rel.body.trim().is_empty() { "Không có ghi chú phát hành." } else { rel.body.trim() }
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
    let bar = "══════════════════════════════════════════════════════════════";
    writeln!(out, "\x1b[1;38;2;140;207;165m{}\x1b[0m", bar).map_err(|e| CaramUiError::Io {
        context: "Ghi OTA TUI".to_string(),
        source: e,
    })?;
    writeln!(out, "\x1b[1;38;2;140;207;165m  CaramOS 2.0 — Trình kiểm tra cập nhật OTA (Debian 13)\x1b[0m")
        .map_err(|e| CaramUiError::Io { context: "Ghi OTA TUI".to_string(), source: e })?;
    writeln!(out, "\x1b[1;38;2;140;207;165m{}\x1b[0m\n{}\n", bar, summary)
        .map_err(|e| CaramUiError::Io { context: "Ghi OTA TUI".to_string(), source: e })?;
    out.flush().map_err(|e| CaramUiError::Io { context: "flush stdout".to_string(), source: e })?;

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
        Commands::Branding { output_root, wallpaper_dir, assets_dir, width, height } => {
            build_caramos_branding_overlay(&output_root, wallpaper_dir.as_deref(), assets_dir.as_deref(), width, height)
        }
        Commands::ApplyTheme { force } => theme_apply::run_apply_theme(force),
        Commands::Control { action } => control::run_control_command(action),
        Commands::PostInstall => control::run_post_install(),
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
    fn branding_emits_sage_indigo_wallpapers_slideshow_and_control_center() {
        let dir = tempdir().unwrap();
        build_caramos_branding_overlay(dir.path(), None, None, 1280, 720).unwrap();

        assert!(dir.path().join("usr/share/backgrounds/caramos/sage-mist.png").is_file());
        assert!(dir.path().join("usr/share/backgrounds/caramos/indigo-night.png").is_file());
        assert!(dir.path().join("etc/calamares/branding/caramos/show.qml").is_file());
        assert!(dir.path().join("usr/share/applications/caramos-update-center.desktop").is_file());
        assert!(dir.path().join("usr/share/cinnamon/applets/caramos-control-center@caramos/applet.js").is_file());
    }
}

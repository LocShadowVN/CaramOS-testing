use crate::{CaramUiError, atomic_write_bytes};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn pick_first_existing(base: &Path, names: &[&str], required_child: &str) -> Option<String> {
    for &name in names {
        if base.join(name).join(required_child).exists() {
            return Some(name.to_string());
        }
    }
    None
}

pub fn is_clean_wallpaper_filename(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    let valid_ext = lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".webp");
    let is_junk = lower.starts_with("animation-")
        || lower.starts_with("throbber-")
        || lower.contains("plymouth")
        || lower.contains("logo")
        || lower.contains("icon")
        || matches!(
            lower.as_str(),
            "bullet.png" | "entry.png" | "lock.png" | "splash.png"
        );
    valid_ext && !is_junk
}

fn find_default_wallpaper_uri() -> Option<String> {
    let dir = Path::new("/usr/share/backgrounds/caramos");
    let default_png = dir.join("default.png");
    if default_png.is_file() {
        return Some(format!("file://{}", default_png.display()));
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(is_clean_wallpaper_filename)
                    .unwrap_or(false)
        })
        .collect();
    entries.sort();
    entries
        .first()
        .map(|p| format!("file://{}", p.display()))
}

fn has_be_vietnam_pro_font() -> bool {
    Command::new("fc-list")
        .args([":", "family"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Be Vietnam Pro"))
        .unwrap_or(false)
}

fn gsettings_set(schema: &str, key: &str, value: &str) {
    let _ = Command::new("gsettings")
        .args(["set", schema, key, value])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Áp giao diện CaramOS chuẩn ở lần đăng nhập đầu tiên (thay thế script Bash `caramos-theme-apply`).
pub fn run_apply_theme(force: bool) -> Result<(), CaramUiError> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let marker = home.join(".config/caramos-theme-applied");
    if !force && marker.exists() {
        return Ok(());
    }

    let themes_dir = Path::new("/usr/share/themes");
    let icons_dir = Path::new("/usr/share/icons");

    let shell_theme = pick_first_existing(
        themes_dir,
        &["Cinnamon-Delight", "CaramOS-Island", "Arc"],
        "cinnamon",
    )
    .unwrap_or_else(|| "CaramOS-Island".to_string());
    let gtk_theme = pick_first_existing(themes_dir, &["Cinnamon-Delight", "Arc"], "gtk-3.0")
        .unwrap_or_else(|| "Arc".to_string());
    let icon_theme = pick_first_existing(
        icons_dir,
        &["Tela-circle-light", "Tela-circle", "Papirus"],
        "index.theme",
    )
    .unwrap_or_else(|| "Papirus".to_string());
    let cursor_theme = pick_first_existing(
        icons_dir,
        &["Bibata-Modern-Classic", "breeze_cursors"],
        "cursors",
    )
    .unwrap_or_else(|| "breeze_cursors".to_string());

    let (ui_font, title_font) = if has_be_vietnam_pro_font() {
        ("Be Vietnam Pro 10", "Be Vietnam Pro Semi-Bold 10")
    } else {
        ("Noto Sans 10", "Noto Sans Semi-Bold 10")
    };

    gsettings_set("org.cinnamon.theme", "name", &shell_theme);
    gsettings_set("org.cinnamon.desktop.interface", "gtk-theme", &gtk_theme);
    gsettings_set("org.cinnamon.desktop.interface", "icon-theme", &icon_theme);
    gsettings_set("org.cinnamon.desktop.interface", "cursor-theme", &cursor_theme);
    gsettings_set("org.cinnamon.desktop.interface", "font-name", ui_font);
    gsettings_set("org.cinnamon.desktop.wm.preferences", "theme", &gtk_theme);
    gsettings_set("org.cinnamon.desktop.wm.preferences", "titlebar-font", title_font);
    gsettings_set(
        "org.cinnamon.desktop.wm.preferences",
        "button-layout",
        ":minimize,maximize,close",
    );
    gsettings_set("org.gnome.desktop.interface", "gtk-theme", &gtk_theme);
    gsettings_set("org.gnome.desktop.interface", "icon-theme", &icon_theme);
    gsettings_set("org.gnome.desktop.interface", "cursor-theme", &cursor_theme);
    gsettings_set("org.gnome.desktop.interface", "font-name", ui_font);

    // Ẩn biểu tượng hệ thống trên màn hình nền trừ Thùng rác (chuẩn CaramOS gốc)
    gsettings_set("org.nemo.desktop", "computer-icon-visible", "false");
    gsettings_set("org.nemo.desktop", "home-icon-visible", "false");
    gsettings_set("org.nemo.desktop", "trash-icon-visible", "true");
    gsettings_set("org.nemo.desktop", "network-icon-visible", "false");
    gsettings_set("org.nemo.desktop", "volumes-visible", "false");

    if let Some(uri) = find_default_wallpaper_uri() {
        gsettings_set("org.cinnamon.desktop.background", "picture-uri", &uri);
        gsettings_set("org.gnome.desktop.background", "picture-uri", &uri);
    }

    // Đăng ký giao thức sâu `caramos-app://` cho Trung tâm Ứng dụng
    let _ = Command::new("xdg-mime")
        .args([
            "default",
            "caramos-app-center-link.desktop",
            "x-scheme-handler/caramos-app",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    atomic_write_bytes(&marker, b"1\n")
}

#[cfg(test)]
mod tests {
    use super::is_clean_wallpaper_filename;

    #[test]
    fn filters_out_plymouth_frames_from_wallpapers() {
        assert!(!is_clean_wallpaper_filename("animation-0001.png"));
        assert!(!is_clean_wallpaper_filename("throbber-0036.png"));
        assert!(!is_clean_wallpaper_filename("plymouth-logo.png"));
        assert!(!is_clean_wallpaper_filename("splash.png"));
        assert!(is_clean_wallpaper_filename("default.png"));
        assert!(is_clean_wallpaper_filename("indigo-night.png"));
    }
}

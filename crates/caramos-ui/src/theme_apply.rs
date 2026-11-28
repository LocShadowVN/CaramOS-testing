use crate::{atomic_write_bytes, control, CaramUiError};
use std::env;
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
    gsettings_set(
        "org.cinnamon.desktop.interface",
        "cursor-theme",
        &cursor_theme,
    );
    gsettings_set("org.cinnamon.desktop.interface", "font-name", ui_font);
    gsettings_set("org.cinnamon.desktop.wm.preferences", "theme", &gtk_theme);
    gsettings_set(
        "org.cinnamon.desktop.wm.preferences",
        "titlebar-font",
        title_font,
    );
    gsettings_set(
        "org.cinnamon.desktop.wm.preferences",
        "button-layout",
        ":minimize,maximize,close",
    );
    gsettings_set("org.gnome.desktop.interface", "gtk-theme", &gtk_theme);
    gsettings_set("org.gnome.desktop.interface", "icon-theme", &icon_theme);
    gsettings_set("org.gnome.desktop.interface", "cursor-theme", &cursor_theme);
    gsettings_set("org.gnome.desktop.interface", "font-name", ui_font);

    gsettings_set("org.nemo.desktop", "computer-icon-visible", "false");
    gsettings_set("org.nemo.desktop", "home-icon-visible", "false");
    gsettings_set("org.nemo.desktop", "trash-icon-visible", "true");
    gsettings_set("org.nemo.desktop", "network-icon-visible", "false");
    gsettings_set("org.nemo.desktop", "volumes-visible", "false");

    let light_wp = control::find_themed_wallpaper(false);
    let dark_wp = control::find_themed_wallpaper(true);
    if let Some(ref uri) = light_wp {
        gsettings_set("org.cinnamon.desktop.background", "picture-uri", uri);
        gsettings_set("org.cinnamon.desktop.background", "picture-options", "zoom");
        gsettings_set("org.gnome.desktop.background", "picture-uri", uri);
        gsettings_set("org.gnome.desktop.background", "picture-options", "zoom");
    }
    if let Some(ref dark_uri) = dark_wp.or(light_wp) {
        gsettings_set("org.gnome.desktop.background", "picture-uri-dark", dark_uri);
    }

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

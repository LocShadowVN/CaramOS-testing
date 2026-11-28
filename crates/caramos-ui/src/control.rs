use crate::{atomic_write_bytes, CaramUiError};
use clap::Subcommand;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Subcommand)]
pub enum ControlAction {
    /// Xuất trạng thái hiện tại dạng JSON gọn cho applet Control Center
    Status,
    /// Đổi chế độ Sáng / Tối đồng bộ GTK, Cinnamon, biểu tượng & hình nền
    ToggleDark,
    /// Áp màu nhấn CaramOS (#RRGGBB) cho ứng dụng GTK4 & Control Center
    SetAccent { hex: String },
    /// Chuyển chế độ nguồn (performance | balanced | power-saver)
    SetPower { profile: String },
    /// Đặt mức âm lượng đầu ra (0..=100) qua PipeWire/wpctl
    SetVolume { percent: u8 },
    /// Bật/tắt tiếng đầu ra mặc định
    ToggleMute,
    /// Đặt độ sáng màn hình (5..=100) cho laptop (sysfs) và màn rời (ddcutil)
    SetBrightness { percent: u8 },
    /// Bật/tắt Ánh sáng đêm (lọc ánh sáng xanh qua xrandr)
    ToggleNightLight,
    /// Thực thi nhanh một hành động an toàn trong danh sách trắng
    Action { name: String },
}

fn user_home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn cmd_stdout(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn run_quiet(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn spawn_detached(bin: &str, args: &[&str]) {
    let _ = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

pub fn is_valid_hex_color(hex: &str) -> bool {
    hex.len() == 7 && hex.starts_with('#') && hex.bytes().skip(1).all(|b| b.is_ascii_hexdigit())
}

pub fn parse_wpctl_volume(output: &str) -> Option<(u8, bool)> {
    let raw = output.split_whitespace().nth(1)?;
    let val: f32 = raw.parse().ok()?;
    let pct = (val * 100.0).round().clamp(0.0, 100.0) as u8;
    let muted = output.contains("[MUTED]");
    Some((pct, muted))
}

pub fn parse_passwd_human_users(content: &str) -> Vec<(String, PathBuf)> {
    let mut users = Vec::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        let Some(&name) = parts.first() else { continue };
        let Some(uid) = parts.get(2).and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(&home) = parts.get(5) else { continue };
        let shell = parts.get(6).copied().unwrap_or("");
        if (1000..65_534).contains(&uid)
            && name != "nobody"
            && !shell.ends_with("nologin")
            && !shell.ends_with("/false")
        {
            users.push((name.to_string(), PathBuf::from(home)));
        }
    }
    users
}

fn read_sysfs_percent(val_path: &Path, max_path: &Path) -> Option<u8> {
    let val: u64 = fs::read_to_string(val_path).ok()?.trim().parse().ok()?;
    let max: u64 = fs::read_to_string(max_path).ok()?.trim().parse().ok()?;
    if max == 0 {
        return None;
    }
    Some((val.saturating_mul(100) / max).min(100) as u8)
}

fn read_backlight_percent() -> u8 {
    let Ok(rd) = fs::read_dir("/sys/class/backlight") else {
        return 100;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if let Some(pct) = read_sysfs_percent(&p.join("brightness"), &p.join("max_brightness")) {
            return pct;
        }
    }
    100
}

fn read_battery_percent() -> Option<u8> {
    let rd = fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("BAT") {
            if let Ok(raw) = fs::read_to_string(entry.path().join("capacity")) {
                if let Ok(pct) = raw.trim().parse::<u8>() {
                    return Some(pct.min(100));
                }
            }
        }
    }
    None
}

fn is_dark_mode() -> bool {
    cmd_stdout(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "color-scheme"],
    )
    .map(|s| s.contains("prefer-dark"))
    .unwrap_or(false)
}

fn read_power_profile() -> String {
    if let Some(p) = cmd_stdout("powerprofilesctl", &["get"]) {
        if matches!(p.as_str(), "performance" | "balanced" | "power-saver") {
            return p;
        }
    }
    "balanced".to_string()
}

fn print_control_status() {
    let home = user_home();
    let dark = is_dark_mode();
    let accent = fs::read_to_string(home.join(".config/caramos/accent.hex"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| is_valid_hex_color(s))
        .unwrap_or_else(|| if dark { "#3a855a" } else { "#2b6a44" }.to_string());
    let night_light = fs::read_to_string(home.join(".config/caramos/night-light"))
        .map(|s| s.trim() == "1")
        .unwrap_or(false);
    let power = read_power_profile();
    let (volume, muted) = cmd_stdout("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .and_then(|s| parse_wpctl_volume(&s))
        .unwrap_or((75, false));
    let brightness = read_backlight_percent();
    let battery_json = read_battery_percent()
        .map(|b| b.to_string())
        .unwrap_or_else(|| "null".to_string());

    println!(
        "{{\"dark\":{},\"accent\":\"{}\",\"night_light\":{},\"power\":\"{}\",\"volume\":{},\"muted\":{},\"brightness\":{},\"battery\":{}}}",
        dark, accent, night_light, power, volume, muted, brightness, battery_json
    );
}

fn gset(schema: &str, key: &str, val: &str) {
    let _ = run_quiet("gsettings", &["set", schema, key, val]);
}

fn pick_installed_dir(base: &str, names: &[&str], marker: &str) -> Option<String> {
    let root = Path::new(base);
    for &name in names {
        if root.join(name).join(marker).exists() {
            return Some(name.to_string());
        }
    }
    None
}

fn apply_wallpaper_pair(active_uri: &str, dark_uri: Option<&str>) {
    gset("org.cinnamon.desktop.background", "picture-uri", active_uri);
    gset("org.cinnamon.desktop.background", "picture-options", "zoom");
    gset("org.gnome.desktop.background", "picture-uri", active_uri);
    gset(
        "org.gnome.desktop.background",
        "picture-uri-dark",
        dark_uri.unwrap_or(active_uri),
    );
    gset("org.gnome.desktop.background", "picture-options", "zoom");
}

fn toggle_dark_mode() {
    let next_dark = !is_dark_mode();
    let light_wp = find_themed_wallpaper(false);
    let dark_wp = find_themed_wallpaper(true);

    if next_dark {
        gset("org.gnome.desktop.interface", "color-scheme", "prefer-dark");
        let shell = pick_installed_dir(
            "/usr/share/themes",
            &["CaramOS-Island-Dark", "Cinnamon-Delight", "Arc-Dark"],
            "cinnamon",
        )
        .unwrap_or_else(|| "CaramOS-Island-Dark".to_string());
        let gtk = pick_installed_dir(
            "/usr/share/themes",
            &["Arc-Dark", "Adwaita-dark", "Cinnamon-Delight"],
            "gtk-3.0",
        )
        .unwrap_or_else(|| "Adwaita-dark".to_string());
        let icon = pick_installed_dir(
            "/usr/share/icons",
            &["Tela-circle-dark", "Tela-circle", "Papirus-Dark", "Papirus"],
            "index.theme",
        )
        .unwrap_or_else(|| "Papirus".to_string());
        gset("org.cinnamon.theme", "name", &shell);
        gset("org.cinnamon.desktop.interface", "gtk-theme", &gtk);
        gset("org.gnome.desktop.interface", "gtk-theme", &gtk);
        gset("org.cinnamon.desktop.interface", "icon-theme", &icon);
        gset("org.gnome.desktop.interface", "icon-theme", &icon);
        if let Some(ref wp) = dark_wp {
            apply_wallpaper_pair(wp, Some(wp));
        }
    } else {
        gset("org.gnome.desktop.interface", "color-scheme", "default");
        let shell = pick_installed_dir(
            "/usr/share/themes",
            &["Cinnamon-Delight", "CaramOS-Island", "Arc"],
            "cinnamon",
        )
        .unwrap_or_else(|| "CaramOS-Island".to_string());
        let gtk = pick_installed_dir("/usr/share/themes", &["Cinnamon-Delight", "Arc"], "gtk-3.0")
            .unwrap_or_else(|| "Arc".to_string());
        let icon = pick_installed_dir(
            "/usr/share/icons",
            &["Tela-circle-light", "Tela-circle", "Papirus"],
            "index.theme",
        )
        .unwrap_or_else(|| "Papirus".to_string());
        gset("org.cinnamon.theme", "name", &shell);
        gset("org.cinnamon.desktop.interface", "gtk-theme", &gtk);
        gset("org.gnome.desktop.interface", "gtk-theme", &gtk);
        gset("org.cinnamon.desktop.interface", "icon-theme", &icon);
        gset("org.gnome.desktop.interface", "icon-theme", &icon);
        if let Some(ref wp) = light_wp {
            apply_wallpaper_pair(wp, dark_wp.as_deref());
        }
    }
}

pub(crate) fn find_themed_wallpaper(dark: bool) -> Option<String> {
    let dir = Path::new("/usr/share/backgrounds/caramos");
    let preferred: &[&str] = if dark {
        &["indigo-night.png", "indigo-night.jpg", "indigo-night.webp"]
    } else {
        &[
            "sage-mist.png",
            "sage-mist.jpg",
            "sage-mist.webp",
            "default.png",
        ]
    };
    for name in preferred {
        let p = dir.join(name);
        if p.is_file() {
            return Some(format!("file://{}", p.display()));
        }
    }

    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    entries.sort();

    let target = if dark {
        entries.iter().find(|p| {
            let s = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            s.contains("indigo") || s.contains("night") || s.contains("dark")
        })
    } else {
        entries.iter().find(|p| {
            let s = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            s == "default.png" || s.contains("sage") || s.contains("light")
        })
    };
    target.map(|p| format!("file://{}", p.display()))
}

fn set_accent_color(hex: &str) -> Result<(), CaramUiError> {
    if !is_valid_hex_color(hex) {
        return Err(CaramUiError::InvalidArgs(format!(
            "Mã màu phải đúng định dạng #RRGGBB, nhận được: '{}'",
            hex
        )));
    }
    let home = user_home();
    atomic_write_bytes(
        &home.join(".config/caramos/accent.hex"),
        format!("{}\n", hex).as_bytes(),
    )?;
    let gtk_css = format!(
        "@define-color accent_bg_color {h};\n@define-color accent_color {h};\n@define-color accent_fg_color #ffffff;\n",
        h = hex
    );
    atomic_write_bytes(&home.join(".config/gtk-4.0/gtk.css"), gtk_css.as_bytes())?;
    Ok(())
}

fn set_power_profile(profile: &str) -> Result<(), CaramUiError> {
    if !matches!(profile, "performance" | "balanced" | "power-saver") {
        return Err(CaramUiError::InvalidArgs(format!(
            "Chế độ nguồn không hợp lệ: '{}'",
            profile
        )));
    }
    let _ = run_quiet("powerprofilesctl", &["set", profile]);
    Ok(())
}

fn set_volume(percent: u8) {
    let clamped = percent.min(100);
    let frac = format!("{:.2}", f32::from(clamped) / 100.0);
    if !run_quiet("wpctl", &["set-volume", "@DEFAULT_AUDIO_SINK@", &frac]) {
        let pct_s = format!("{}%", clamped);
        let _ = run_quiet("pactl", &["set-sink-volume", "@DEFAULT_SINK@", &pct_s]);
    }
}

fn toggle_mute() {
    if !run_quiet("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]) {
        let _ = run_quiet("pactl", &["set-sink-mute", "@DEFAULT_SINK@", "toggle"]);
    }
}

fn set_brightness(percent: u8) {
    let clamped = percent.clamp(5, 100);
    if let Ok(rd) = fs::read_dir("/sys/class/backlight") {
        for entry in rd.flatten() {
            let p = entry.path();
            if let Ok(max_s) = fs::read_to_string(p.join("max_brightness")) {
                if let Ok(max_v) = max_s.trim().parse::<u64>() {
                    let target = max_v.saturating_mul(u64::from(clamped)) / 100;
                    let _ = fs::write(p.join("brightness"), target.to_string());
                }
            }
        }
    }
    spawn_detached("ddcutil", &["setvcp", "10", &clamped.to_string()]);
}

fn toggle_night_light() -> Result<(), CaramUiError> {
    let marker = user_home().join(".config/caramos/night-light");
    let currently_on = fs::read_to_string(&marker)
        .map(|s| s.trim() == "1")
        .unwrap_or(false);
    let next_on = !currently_on;
    let gamma = if next_on {
        "1.0:0.86:0.70"
    } else {
        "1.0:1.0:1.0"
    };
    if let Some(xr) = cmd_stdout("xrandr", &["--query"]) {
        for line in xr.lines() {
            if line.contains(" connected") {
                if let Some(output_name) = line.split_whitespace().next() {
                    let _ = run_quiet("xrandr", &["--output", output_name, "--gamma", gamma]);
                }
            }
        }
    }
    atomic_write_bytes(&marker, if next_on { b"1\n" } else { b"0\n" })
}

fn run_whitelisted_action(name: &str) -> Result<(), CaramUiError> {
    match name {
        "screenshot" => spawn_detached("flameshot", &["gui"]),
        "clipboard" => spawn_detached("copyq", &["toggle"]),
        "apps" => spawn_detached("/usr/local/bin/caramos-app-center", &[]),
        "drivers" => spawn_detached("/usr/local/bin/caramos-driver-check", &[]),
        "updates" => spawn_detached("/usr/local/bin/caramos-update-center", &[]),
        "settings" => spawn_detached("cinnamon-settings", &[]),
        "network" => spawn_detached("cinnamon-settings", &["network"]),
        "bluetooth" => spawn_detached("blueman-manager", &[]),
        "lock" => spawn_detached("cinnamon-screensaver-command", &["-l"]),
        "reboot" => spawn_detached("systemctl", &["reboot"]),
        "poweroff" => spawn_detached("systemctl", &["poweroff"]),
        _ => {
            return Err(CaramUiError::InvalidArgs(format!(
                "Hành động Control Center không được phép: '{}'",
                name
            )));
        }
    }
    Ok(())
}

pub fn run_control_command(action: ControlAction) -> Result<(), CaramUiError> {
    match action {
        ControlAction::Status => {
            print_control_status();
            Ok(())
        }
        ControlAction::ToggleDark => {
            toggle_dark_mode();
            Ok(())
        }
        ControlAction::SetAccent { hex } => set_accent_color(&hex),
        ControlAction::SetPower { profile } => set_power_profile(&profile),
        ControlAction::SetVolume { percent } => {
            set_volume(percent);
            Ok(())
        }
        ControlAction::ToggleMute => {
            toggle_mute();
            Ok(())
        }
        ControlAction::SetBrightness { percent } => {
            set_brightness(percent);
            Ok(())
        }
        ControlAction::ToggleNightLight => toggle_night_light(),
        ControlAction::Action { name } => run_whitelisted_action(&name),
    }
}

pub fn run_post_install() -> Result<(), CaramUiError> {
    let _ = run_quiet("systemd-sysusers", &[]);
    let passwd = fs::read_to_string("/etc/passwd").map_err(|e| CaramUiError::Io {
        context: "Đọc /etc/passwd".to_string(),
        source: e,
    })?;
    let lotus_service_exists = Path::new("/usr/lib/systemd/system/fcitx5-lotus-server@.service")
        .exists()
        || Path::new("/lib/systemd/system/fcitx5-lotus-server@.service").exists();

    for (username, home_dir) in parse_passwd_human_users(&passwd) {
        let _ = run_quiet("usermod", &["-aG", "input", &username]);
        if lotus_service_exists {
            let unit = format!("fcitx5-lotus-server@{}.service", username);
            let _ = run_quiet("systemctl", &["enable", &unit]);
        }
        let _ = fs::remove_file(home_dir.join("Desktop/install-caramos.desktop"));
    }
    let _ = fs::remove_file("/etc/skel/Desktop/install-caramos.desktop");
    let _ = run_quiet(
        "systemctl",
        &["disable", "fcitx5-lotus-server@user.service"],
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn validates_hex_color_and_rejects_css_injection() {
        assert!(is_valid_hex_color("#2b6a44"));
        assert!(is_valid_hex_color("#3A855A"));
        assert!(!is_valid_hex_color("2b6a44"));
        assert!(!is_valid_hex_color("#2b6a44; } body {"));
    }

    #[test]
    fn parses_wpctl_volume_and_mute_state() {
        assert_eq!(parse_wpctl_volume("Volume: 0.75"), Some((75, false)));
        assert_eq!(parse_wpctl_volume("Volume: 0.42 [MUTED]"), Some((42, true)));
    }

    #[test]
    fn parses_human_users_from_passwd() {
        let sample = "root:x:0:0:root:/root:/bin/bash\n\
                      nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n\
                      loc:x:1000:1000:Loc:/home/loc:/bin/bash\n";
        let users = parse_passwd_human_users(sample);
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].0, "loc");
        assert_eq!(users[0].1, PathBuf::from("/home/loc"));
    }
}

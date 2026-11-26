use crate::{CaramCtlError, atomic_write_file, copy_directory_tree_overlay};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::NamedTempFile;
use walkdir::WalkDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn download_archive(url: &str, dir: &Path) -> Result<PathBuf, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(240))
        .build();
    let resp = agent
        .get(url)
        .set("User-Agent", "caramos-ctl/2.0")
        .call()
        .map_err(|e| e.to_string())?;
    let mut reader = resp.into_reader();
    let mut tmp = NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 65_536];
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            tmp.write_all(chunk).map_err(|e| e.to_string())?;
        }
    }
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    let (_, path) = tmp.keep().map_err(|e| e.error.to_string())?;
    Ok(path)
}

fn run_ok(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn find_dir_named(root: &Path, target_name: &str) -> Option<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .find(|e| e.file_type().is_dir() && e.file_name() == target_name)
        .map(|e| e.into_path())
}

fn install_spice_zip(
    work_tmp: &Path,
    url: &str,
    uuid: &str,
    dst_base: &Path,
) -> Result<bool, CaramCtlError> {
    let Ok(zip_path) = download_archive(url, work_tmp) else {
        return Ok(false);
    };
    let extract_dir = work_tmp.join(format!("extract-{}", uuid));
    let _ = fs::remove_dir_all(&extract_dir);
    let _ = fs::create_dir_all(&extract_dir);

    let extracted = run_ok(
        "unzip",
        &[
            "-qo",
            &zip_path.to_string_lossy(),
            "-d",
            &extract_dir.to_string_lossy(),
        ],
    );
    let _ = fs::remove_file(&zip_path);
    if !extracted {
        let _ = fs::remove_dir_all(&extract_dir);
        return Ok(false);
    }

    let found = find_dir_named(&extract_dir, uuid);
    if let Some(src) = found {
        let dst = dst_base.join(uuid);
        let _ = fs::remove_dir_all(&dst);
        let _ = copy_directory_tree_overlay(&src, &dst)?;
        // Nếu là Nemo action có tệp mẫu `.nemo_action.in`, tạo `.nemo_action`
        let template_name = format!("{}.nemo_action.in", uuid);
        if let Some(tpl) = WalkDir::new(&extract_dir)
            .into_iter()
            .filter_map(Result::ok)
            .find(|e| e.file_type().is_file() && e.file_name() == template_name.as_str())
        {
            let _ = fs::copy(tpl.path(), dst_base.join(format!("{}.nemo_action", uuid)));
        }
        let _ = fs::remove_dir_all(&extract_dir);
        return Ok(true);
    }

    let _ = fs::remove_dir_all(&extract_dir);
    Ok(false)
}

/// Tải & cài bộ tài nguyên gốc CaramOS vào `rootfs` trong Stage 2 (Debian 13):
/// Be Vietnam Pro, Cinnamon-Delight, Tela-circle bản chuẩn, Bibata-Modern-Classic,
/// Cinnamenu@json, Nemo actions và giới hạn kích thước icon systray cho Fcitx5/Lotus.
pub fn install_caramos_desktop_assets<W: Write>(
    rootfs: &Path,
    writer: &mut W,
) -> Result<(), CaramCtlError> {
    let work_tmp = rootfs.join("tmp/caramos-assets-stage");
    let _ = fs::create_dir_all(&work_tmp);

    let _ = writeln!(
        writer,
        "▶ [CARAMOS ASSETS] Cài Be Vietnam Pro, Cinnamon-Delight, Tela-circle, Bibata & Cinnamenu..."
    );

    // 1. Phông Be Vietnam Pro
    if let Ok(zip_path) = download_archive(
        "https://github.com/bettergui/BeVietnamPro/archive/refs/heads/master.zip",
        &work_tmp,
    ) {
        let ext = work_tmp.join("be-vietnam-pro");
        let _ = fs::create_dir_all(&ext);
        if run_ok(
            "unzip",
            &["-qo", &zip_path.to_string_lossy(), "-d", &ext.to_string_lossy()],
        ) {
            let font_dst = rootfs.join("usr/share/fonts/truetype/be-vietnam-pro");
            let _ = fs::create_dir_all(&font_dst);
            for entry in WalkDir::new(&ext).into_iter().filter_map(Result::ok) {
                if entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("ttf"))
                        .unwrap_or(false)
                {
                    if let Some(name) = entry.path().file_name() {
                        let _ = fs::copy(entry.path(), font_dst.join(name));
                    }
                }
            }
            let _ = writeln!(writer, "  ✔ Đã cài phông Be Vietnam Pro.");
        }
        let _ = fs::remove_file(&zip_path);
        let _ = fs::remove_dir_all(&ext);
    }

    // 2. Con trỏ Bibata-Modern-Classic v2.0.6
    if let Ok(tar_path) = download_archive(
        "https://github.com/ful1e5/Bibata_Cursor/releases/download/v2.0.6/Bibata-Modern-Classic.tar.xz",
        &work_tmp,
    ) {
        let icons_dir = rootfs.join("usr/share/icons");
        let _ = fs::create_dir_all(&icons_dir);
        if run_ok(
            "tar",
            &[
                "-xf",
                &tar_path.to_string_lossy(),
                "-C",
                &icons_dir.to_string_lossy(),
            ],
        ) {
            let _ = writeln!(writer, "  ✔ Đã cài con trỏ Bibata-Modern-Classic.");
        }
        let _ = fs::remove_file(&tar_path);
    }

    // 3. Theme Cinnamon-Delight
    let delight_zip = download_archive(
        "https://github.com/DrMcC0y/Cinnamon-Delight/archive/refs/heads/main.zip",
        &work_tmp,
    )
    .or_else(|_| {
        download_archive(
            "https://github.com/DrMcC0y/Cinnamon-Delight/archive/refs/heads/master.zip",
            &work_tmp,
        )
    });
    if let Ok(zip_path) = delight_zip {
        let ext = work_tmp.join("cinnamon-delight");
        let _ = fs::create_dir_all(&ext);
        if run_ok(
            "unzip",
            &["-qo", &zip_path.to_string_lossy(), "-d", &ext.to_string_lossy()],
        ) {
            let src = find_dir_named(&ext, "Cinnamon-Delight").unwrap_or_else(|| ext.clone());
            let dst = rootfs.join("usr/share/themes/Cinnamon-Delight");
            let _ = fs::remove_dir_all(&dst);
            let _ = copy_directory_tree_overlay(&src, &dst)?;
            let _ = writeln!(writer, "  ✔ Đã cài theme Cinnamon-Delight.");
        }
        let _ = fs::remove_file(&zip_path);
        let _ = fs::remove_dir_all(&ext);
    }

    // 4. Icon Tela-circle (CHỈ cài gói 'standard': Tela-circle, -light, -dark)
    if let Ok(zip_path) = download_archive(
        "https://github.com/vinceliuice/Tela-circle-icon-theme/archive/refs/heads/master.zip",
        &work_tmp,
    ) {
        let ext = work_tmp.join("tela-circle");
        let _ = fs::create_dir_all(&ext);
        if run_ok(
            "unzip",
            &["-qo", &zip_path.to_string_lossy(), "-d", &ext.to_string_lossy()],
        ) {
            let installer = ext.join("Tela-circle-icon-theme-master/install.sh");
            if installer.is_file() {
                let icons_dst = rootfs.join("usr/share/icons");
                let icons_s = icons_dst.to_string_lossy();
let ok = Command::new("bash")
    .arg(&installer)
    .args(["-d", icons_s.as_ref(), "standard"])
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .map(|s| s.success())
    .unwrap_or(false);
                if ok {
                    let _ = writeln!(writer, "  ✔ Đã cài bộ biểu tượng Tela-circle (standard).");
                }
            }
        }
        let _ = fs::remove_file(&zip_path);
        let _ = fs::remove_dir_all(&ext);
    }

    // 5. Applet Cinnamenu@json & Nemo actions (Sao chép đường dẫn, Tạo lối tắt trên màn hình)
    let applets_dst = rootfs.join("usr/share/cinnamon/applets");
    let _ = fs::create_dir_all(&applets_dst);
    if install_spice_zip(
        &work_tmp,
        "https://github.com/linuxmint/cinnamon-spices-applets/archive/refs/heads/master.zip",
        "Cinnamenu@json",
        &applets_dst,
    )? {
        let cinnamenu_dir = applets_dst.join("Cinnamenu@json");
        let logo = rootfs.join("usr/share/pixmaps/caramos-logo.png");
        if logo.is_file() {
            let _ = fs::copy(&logo, cinnamenu_dir.join("icon.png"));
            let _ = fs::copy(&logo, cinnamenu_dir.join("icon-symbolic.png"));
        }
        let _ = writeln!(writer, "  ✔ Đã cài Start Menu Cinnamenu@json.");
    }

    let nemo_actions = rootfs.join("usr/share/nemo/actions");
    let _ = fs::create_dir_all(&nemo_actions);
    for uuid in [
        "copy-path-to-clipboard@claudiux",
        "create-desktop-shortcut@anaximeno",
    ] {
        let _ = install_spice_zip(
            &work_tmp,
            "https://github.com/linuxmint/cinnamon-spices-actions/archive/refs/heads/master.zip",
            uuid,
            &nemo_actions,
        )?;
    }
    #[cfg(unix)]
    for entry in WalkDir::new(&nemo_actions).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file()
            && matches!(
                entry.path().extension().and_then(|e| e.to_str()),
                Some("py" | "sh")
            )
        {
            let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o755));
        }
    }

    // 6. Vá lỗi icon Fcitx5/Lotus phình to trên systray (chuẩn 0170-cinnamon-task17-spices)
    let systray_js = rootfs.join("usr/share/cinnamon/applets/systray@cinnamon.org/applet.js");
    if let Ok(src) = fs::read_to_string(&systray_js) {
        let patched = src
            .replace(
                "this.icon_size = this.getPanelIconSize(St.IconType.FULLCOLOR) * global.ui_scale;",
                "this.icon_size = Math.min(this.getPanelIconSize(St.IconType.FULLCOLOR) * global.ui_scale, 16 * global.ui_scale);",
            )
            .replace(
                "icon.set_y_align(Clutter.ActorAlign.FILL);",
                "icon.set_y_align(Clutter.ActorAlign.CENTER);",
            )
            .replace(
                "button.set_y_align(Clutter.ActorAlign.FILL);",
                "button.set_y_align(Clutter.ActorAlign.CENTER);",
            );
        let _ = atomic_write_file(&systray_js, patched.as_bytes());
    }

    // 7. Vá StartupWMClass cho Nemo trên panel
    let nemo_desktop = rootfs.join("usr/share/applications/nemo.desktop");
    if let Ok(content) = fs::read_to_string(&nemo_desktop) {
        if !content.contains("StartupWMClass=") {
            let updated = format!("{}\nStartupWMClass=Nemo\n", content.trim_end());
            let _ = atomic_write_file(&nemo_desktop, updated.as_bytes());
        }
    }

    let _ = fs::remove_dir_all(&work_tmp);
    Ok(())
}

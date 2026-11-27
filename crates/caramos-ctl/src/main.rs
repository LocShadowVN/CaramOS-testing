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

mod caramos_assets;

use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;
use tempfile::NamedTempFile;
use thiserror::Error;
use walkdir::WalkDir;

#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};

#[derive(Debug, Error)]
pub enum CaramCtlError {
    #[error("Lỗi I/O ({context}): {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error("Lệnh '{cmd}' thất bại (mã thoát {code:?}): {detail}")]
    CommandFailed {
        cmd: String,
        code: Option<i32>,
        detail: String,
    },
    #[error("Cấu hình không hợp lệ: {0}")]
    InvalidConfig(String),
}

#[derive(Debug, Parser)]
#[command(
    name = "caramos-ctl",
    version,
    about = "CaramOS 2.0 Debian 13 ISO Builder & Pruner"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Quét và dọn dẹp các tệp di sản Mint / Ubiquity / Python / Plymouth bị xóa sót
    Prune {
        #[arg(long, default_value = ".")]
        repo_root: PathBuf,
        #[arg(long)]
        execute: bool,
    },
    /// Dựng ISO CaramOS 2.0 trên nền Debian 13 (Trixie) boot hybrid BIOS + UEFI
    Build {
        #[arg(long, default_value = "amd64")]
        arch: String,
        #[arg(long, default_value = "trixie")]
        suite: String,
        #[arg(long, default_value = "http://deb.debian.org/debian")]
        mirror: String,
        #[arg(long)]
        work_dir: PathBuf,
        #[arg(long)]
        includes_dir: Option<PathBuf>,
        #[arg(long = "extra-overlay")]
        extra_overlays: Vec<PathBuf>,
        #[arg(long = "stage-bin")]
        stage_bins: Vec<PathBuf>,
        #[arg(long)]
        packages_file: PathBuf,
        #[arg(long)]
        output_iso: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn atomic_write_file(dest: &Path, data: &[u8]) -> Result<(), CaramCtlError> {
    let parent = dest.parent().ok_or_else(|| {
        CaramCtlError::InvalidConfig(format!("Thiếu thư mục cha cho '{}'", dest.display()))
    })?;
    fs::create_dir_all(parent).map_err(|e| CaramCtlError::Io {
        context: format!("Tạo thư mục '{}'", parent.display()),
        source: e,
    })?;
    let mut tmp = NamedTempFile::new_in(parent).map_err(|e| CaramCtlError::Io {
        context: format!("Tạo tệp tạm trong '{}'", parent.display()),
        source: e,
    })?;
    tmp.write_all(data).map_err(|e| CaramCtlError::Io {
        context: format!("Ghi '{}'", dest.display()),
        source: e,
    })?;
    tmp.as_file().sync_all().map_err(|e| CaramCtlError::Io {
        context: format!("fsync '{}'", dest.display()),
        source: e,
    })?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o644));
    }
    tmp.persist(dest).map_err(|e| CaramCtlError::Io {
        context: format!("Lưu '{}'", dest.display()),
        source: e.error,
    })?;
    Ok(())
}

pub fn copy_directory_tree_overlay(src: &Path, dst: &Path) -> Result<usize, CaramCtlError> {
    if !src.exists() {
        return Ok(0);
    }
    let mut copied = 0usize;
    for entry in WalkDir::new(src).into_iter().filter_map(Result::ok) {
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|e| CaramCtlError::InvalidConfig(format!("Lỗi đường dẫn tương đối: {}", e)))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        if entry.path_is_symlink() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| CaramCtlError::Io {
                    context: format!("Tạo '{}'", parent.display()),
                    source: e,
                })?;
            }
            let _ = fs::remove_file(&target);
            #[cfg(unix)]
            {
                let link_target = fs::read_link(entry.path()).map_err(|e| CaramCtlError::Io {
                    context: format!("Đọc symlink '{}'", entry.path().display()),
                    source: e,
                })?;
                symlink(&link_target, &target).map_err(|e| CaramCtlError::Io {
                    context: format!("Tạo symlink '{}'", target.display()),
                    source: e,
                })?;
            }
            copied += 1;
        } else if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(|e| CaramCtlError::Io {
                context: format!("Tạo '{}'", target.display()),
                source: e,
            })?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| CaramCtlError::Io {
                    context: format!("Tạo '{}'", parent.display()),
                    source: e,
                })?;
            }
            fs::copy(entry.path(), &target).map_err(|e| CaramCtlError::Io {
                context: format!(
                    "Chép '{}' -> '{}'",
                    entry.path().display(),
                    target.display()
                ),
                source: e,
            })?;
            if let Ok(meta) = fs::metadata(entry.path()) {
                let _ = fs::set_permissions(&target, meta.permissions());
            }
            copied += 1;
        }
    }
    Ok(copied)
}

fn parse_packages_file(path: &Path) -> Result<Vec<String>, CaramCtlError> {
    let content = fs::read_to_string(path).map_err(|e| CaramCtlError::Io {
        context: format!("Đọc '{}'", path.display()),
        source: e,
    })?;
    let mut pkgs = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        if trimmed.is_empty() {
            continue;
        }
        let valid = trimmed
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '+'));
        if !valid {
            return Err(CaramCtlError::InvalidConfig(format!(
                "Tên gói Debian không hợp lệ ở dòng {}: '{}'",
                idx + 1,
                trimmed
            )));
        }
        pkgs.push(trimmed.to_string());
    }
    if pkgs.is_empty() {
        return Err(CaramCtlError::InvalidConfig(
            "Danh mục packages.txt rỗng".to_string(),
        ));
    }
    Ok(pkgs)
}

const KNOWN_LEGACY_RELPATHS: &[&str] = &[
    ".gitmodules",
    "caramos_vietnam_banner.png",
    "packages/caramos-app-center",
    "packages/caram-os-demo",
    "config/archives/linuxmint.key.binary",
    "config/archives/linuxmint.key.chroot",
    "config/archives/linuxmint.pref.chroot",
    "config/includes.chroot/etc/skel/.linuxmint",
    "config/includes.chroot/usr/share/applications/ubiquity.desktop",
    "config/includes.chroot/etc/os-release",
];

fn is_plymouth_bg_junk(name: &str) -> bool {
    name.starts_with("animation-")
        || name.starts_with("throbber-")
        || matches!(
            name,
            "plymouth-logo.png" | "bullet.png" | "entry.png" | "lock.png" | "splash.png"
        )
}

fn collect_legacy_paths(repo_root: &Path) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    for rel in KNOWN_LEGACY_RELPATHS {
        let full = repo_root.join(rel);
        if full.exists() {
            targets.push(full);
        }
    }
    let bg_dir = repo_root.join("config/includes.chroot/usr/share/backgrounds/caramos");
    if let Ok(rd) = fs::read_dir(&bg_dir) {
        for entry in rd.flatten() {
            if entry
                .file_name()
                .to_str()
                .map(is_plymouth_bg_junk)
                .unwrap_or(false)
            {
                targets.push(entry.path());
            }
        }
    }
    targets.sort();
    targets
}

fn run_prune(repo_root: &Path, execute: bool) -> Result<(), CaramCtlError> {
    let targets = collect_legacy_paths(repo_root);
    if targets.is_empty() {
        println!(
            "✔ Không phát hiện tệp di sản nào trong '{}'.",
            repo_root.display()
        );
        return Ok(());
    }
    println!(
        "▶ Phát hiện {} mục di sản ({}):",
        targets.len(),
        if execute {
            "ĐANG XÓA"
        } else {
            "CHẾ ĐỘ XEM TRƯỚC"
        }
    );
    for path in targets {
        println!("  - {}", path.display());
        if execute {
            if path.is_dir() {
                fs::remove_dir_all(&path).map_err(|e| CaramCtlError::Io {
                    context: format!("Xóa '{}'", path.display()),
                    source: e,
                })?;
            } else if path.exists() {
                fs::remove_file(&path).map_err(|e| CaramCtlError::Io {
                    context: format!("Xóa '{}'", path.display()),
                    source: e,
                })?;
            }
        }
    }
    Ok(())
}

/// RAII Guard: mượn `/etc/resolv.conf` của host trong khi chạy `apt-get`/`flatpak` trong chroot.
struct ResolvConfGuard {
    target: PathBuf,
    backup: Option<Vec<u8>>,
}

impl ResolvConfGuard {
    fn activate(rootfs: &Path) -> Self {
        let target = rootfs.join("etc/resolv.conf");
        let backup = fs::read(&target).ok();
        if let Ok(host_dns) = fs::read("/etc/resolv.conf") {
            let _ = atomic_write_file(&target, &host_dns);
        }
        Self { target, backup }
    }
}

impl Drop for ResolvConfGuard {
    fn drop(&mut self) {
        if let Some(ref prev) = self.backup {
            let _ = atomic_write_file(&self.target, prev);
        }
    }
}

struct BuildConfig {
    arch: String,
    suite: String,
    mirror: String,
    work_dir: PathBuf,
    includes_dir: Option<PathBuf>,
    extra_overlays: Vec<PathBuf>,
    stage_bins: Vec<PathBuf>,
    packages_file: PathBuf,
    output_iso: PathBuf,
    dry_run: bool,
}

struct IsoBuildPipeline {
    config: BuildConfig,
    rootfs_dir: PathBuf,
    iso_staging_dir: PathBuf,
}

fn run_cmd_checked(bin: &str, args: &[&str], label: &str) -> Result<(), CaramCtlError> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| CaramCtlError::Io {
            context: format!("Chạy '{}'", label),
            source: e,
        })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(CaramCtlError::CommandFailed {
            cmd: label.to_string(),
            code: out.status.code(),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

fn run_chroot_if_present(
    rootfs: &Path,
    guest_bin: &str,
    args: &[&str],
    label: &str,
) -> Result<(), CaramCtlError> {
    if !rootfs.join(guest_bin.trim_start_matches('/')).exists() {
        return Ok(());
    }
    let rootfs_str = rootfs.to_string_lossy();
    let mut full_args: Vec<&str> = vec![rootfs_str.as_ref(), guest_bin];
    full_args.extend_from_slice(args);
    run_cmd_checked("chroot", &full_args, label)
}

fn is_pkg_installed_in_chroot(rootfs: &Path, pkg: &str) -> bool {
    let rootfs_s = rootfs.to_string_lossy();
    Command::new("chroot")
        .args([
            rootfs_s.as_ref(),
            "/usr/bin/dpkg-query",
            "-W",
            "-f=${Status}",
            pkg,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).contains("install ok installed")
        })
        .unwrap_or(false)
}

#[derive(Debug, Deserialize)]
struct LotusRelease {
    assets: Vec<LotusAsset>,
}

#[derive(Debug, Deserialize)]
struct LotusAsset {
    name: String,
    browser_download_url: String,
}

/// Cài `fcitx5-lotus` chính chủ từ `fcitx5-lotus.pages.dev/apt/<suite>` (ưu tiên),
/// fallback sang GitHub Releases `LotusInputMethod/fcitx5-lotus`. Trả về `true` nếu cài thành công.
fn install_fcitx5_lotus_in_chroot<W: Write>(rootfs: &Path, suite: &str, writer: &mut W) -> bool {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(180))
        .build();
    let rootfs_s = rootfs.to_string_lossy().to_string();

    // Phương án 1: Kho APT chính thức của LotusInputMethod
    if let Ok(key_resp) = agent
        .get("https://fcitx5-lotus.pages.dev/pubkey.gpg")
        .set("User-Agent", "caramos-ctl/2.0")
        .call()
    {
        let mut key_bytes = Vec::new();
        if key_resp.into_reader().read_to_end(&mut key_bytes).is_ok() {
            let tmp_asc = rootfs.join("tmp/fcitx5-lotus.asc");
            let keyring_dir = rootfs.join("etc/apt/keyrings");
            let list_path = rootfs.join("etc/apt/sources.list.d/fcitx5-lotus.list");
            let _ = fs::create_dir_all(&keyring_dir);
            if atomic_write_file(&tmp_asc, &key_bytes).is_ok() {
                let dearmored = Command::new("chroot")
                    .args([
                        rootfs_s.as_str(),
                        "/usr/bin/gpg",
                        "--batch",
                        "--yes",
                        "--dearmor",
                        "-o",
                        "/etc/apt/keyrings/fcitx5-lotus.gpg",
                        "/tmp/fcitx5-lotus.asc",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                let _ = fs::remove_file(&tmp_asc);

                if dearmored {
                    let repo_line = format!(
                        "deb [arch=amd64 signed-by=/etc/apt/keyrings/fcitx5-lotus.gpg] https://fcitx5-lotus.pages.dev/apt/{s} {s} main\n",
                        s = suite
                    );
                    if atomic_write_file(&list_path, repo_line.as_bytes()).is_ok() {
                        let upd_ok = Command::new("chroot")
                            .args([rootfs_s.as_str(), "/usr/bin/apt-get", "update"])
                            .env("DEBIAN_FRONTEND", "noninteractive")
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status()
                            .map(|s| s.success())
                            .unwrap_or(false);
                        if upd_ok {
                            let _ = Command::new("chroot")
                                .args([
                                    rootfs_s.as_str(),
                                    "/usr/bin/apt-get",
                                    "install",
                                    "-y",
                                    "--no-install-recommends",
                                    "fcitx5-lotus",
                                ])
                                .env("DEBIAN_FRONTEND", "noninteractive")
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .status();
                        }
                        if is_pkg_installed_in_chroot(rootfs, "fcitx5-lotus") {
                            let _ = writeln!(
                                writer,
                                "  ✔ Đã cài đặt Fcitx5 Lotus từ kho APT chính thức ({}).",
                                suite
                            );
                            return true;
                        }
                        // Nếu kho chưa hỗ trợ codename này -> xóa tệp .list để tránh hỏng apt update về sau
                        let _ = fs::remove_file(&list_path);
                    }
                }
            }
        }
    }

    // Phương án 2: Tải .deb Debian/amd64 trực tiếp từ GitHub Releases LotusInputMethod/fcitx5-lotus
    let api = "https://api.github.com/repos/LotusInputMethod/fcitx5-lotus/releases/latest";
    if let Ok(resp) = agent
        .get(api)
        .set("User-Agent", "caramos-ctl/2.0")
        .set("Accept", "application/vnd.github+json")
        .call()
    {
        if let Ok(rel) = resp.into_json::<LotusRelease>() {
            let picked = rel
                .assets
                .iter()
                .find(|a| {
                    let l = a.name.to_ascii_lowercase();
                    l.ends_with(".deb")
                        && l.contains("amd64")
                        && l.contains(suite)
                        && !l.contains("dbgsym")
                })
                .or_else(|| {
                    rel.assets.iter().find(|a| {
                        let l = a.name.to_ascii_lowercase();
                        l.ends_with(".deb")
                            && l.contains("amd64")
                            && l.contains("debian")
                            && !l.contains("dbgsym")
                    })
                });
            if let Some(asset) = picked {
                let tmp_deb = rootfs.join("tmp/fcitx5-lotus.deb");
                if let Ok(dl) = agent
                    .get(&asset.browser_download_url)
                    .set("User-Agent", "caramos-ctl/2.0")
                    .call()
                {
                    let mut data = Vec::new();
                    if dl.into_reader().read_to_end(&mut data).is_ok()
                        && atomic_write_file(&tmp_deb, &data).is_ok()
                    {
                        let _ = Command::new("chroot")
                            .args([rootfs_s.as_str(), "/usr/bin/apt-get", "update"])
                            .env("DEBIAN_FRONTEND", "noninteractive")
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();
                        let _ = Command::new("chroot")
                            .args([
                                rootfs_s.as_str(),
                                "/usr/bin/apt-get",
                                "install",
                                "-y",
                                "--no-install-recommends",
                                "/tmp/fcitx5-lotus.deb",
                            ])
                            .env("DEBIAN_FRONTEND", "noninteractive")
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();
                        let _ = fs::remove_file(&tmp_deb);
                        if is_pkg_installed_in_chroot(rootfs, "fcitx5-lotus") {
                            let _ =
                                writeln!(writer, "  ✔ Đã cài đặt Fcitx5 Lotus ({}).", asset.name);
                            return true;
                        }
                    }
                }
            }
        }
    }

    let _ = writeln!(
        writer,
        "  [WARN] Không cài được fcitx5-lotus cho {}; dùng fcitx5-bamboo mặc định.",
        suite
    );
    false
}

fn write_fcitx5_profile_and_config(
    rootfs: &Path,
    lotus_installed: bool,
) -> Result<(), CaramCtlError> {
    let im_name = if lotus_installed { "lotus" } else { "bamboo" };
    let skel_fcitx = rootfs.join("etc/skel/.config/fcitx5");

    // Profile chuẩn CaramOS: bật sẵn tiếng Việt khi đăng nhập + giữ keyboard-us để chuyển bằng Ctrl+Shift
    let profile = format!(
        "[Groups/0]\n\
         Name=Default\n\
         Default Layout=us\n\
         DefaultIM={im}\n\n\
         [Groups/0/Items/0]\n\
         Name=keyboard-us\n\
         Layout=\n\n\
         [Groups/0/Items/1]\n\
         Name={im}\n\
         Layout=\n\n\
         [GroupOrder]\n\
         0=Default\n",
        im = im_name
    );
    atomic_write_file(&skel_fcitx.join("profile"), profile.as_bytes())?;

    // Phím tắt chuyển Anh/Việt chuẩn CaramOS gốc: Ctrl+Shift (trái/phải)
    let config = r##"[Hotkey]
EnumerateWithTriggerKeys=True
EnumerateForwardKeys=
EnumerateBackwardKeys=
EnumerateSkipFirst=False

[Hotkey/TriggerKeys]
0=Control+Shift_L
1=Control+Shift_R

[Behavior]
ActiveByDefault=True
ShareInputState=No
PreeditEnabledByDefault=False
ShowInputMethodInformation=True
ShowInputMethodInformationWhenFocusIn=False
CompactInputMethodInformation=True
ShowFirstInputMethodInformation=True
DefaultPageSize=5
"##;
    atomic_write_file(&skel_fcitx.join("config"), config.as_bytes())?;

    // Quyền thiết bị /dev/uinput để fcitx5-lotus-server gõ mượt không gạch chân & không delay
    let uinput_rule =
        "KERNEL==\"uinput\", MODE=\"0660\", GROUP=\"input\", OPTIONS+=\"static_node=uinput\"\n";
    atomic_write_file(
        &rootfs.join("etc/udev/rules.d/99-caramos-uinput.rules"),
        uinput_rule.as_bytes(),
    )?;
    atomic_write_file(&rootfs.join("etc/modules-load.d/uinput.conf"), b"uinput\n")?;

    // Đảm bảo Fcitx5 tự chạy khi đăng nhập Cinnamon (cả phiên Live và máy đã cài)
    let autostart = r##"[Desktop Entry]
Type=Application
Name=Fcitx 5
Exec=/usr/bin/fcitx5 -d --replace
X-GNOME-Autostart-enabled=true
NoDisplay=true
OnlyShowIn=X-Cinnamon;
"##;
    atomic_write_file(
        &rootfs.join("etc/xdg/autostart/caramos-fcitx5.desktop"),
        autostart.as_bytes(),
    )?;

    // Với bản Live (user mặc định của live-config là `user`), bật sẵn uinput server cho Lotus
    if lotus_installed {
        let _ = run_chroot_if_present(
            rootfs,
            "/usr/bin/systemctl",
            &["enable", "fcitx5-lotus-server@user.service"],
            "enable fcitx5-lotus-server@user",
        );
    }
    Ok(())
}

fn pick_existing_theme(base: &Path, candidates: &[&str], marker: &str) -> String {
    for &name in candidates {
        if base.join(name).join(marker).exists() {
            return name.to_string();
        }
    }
    candidates.last().copied().unwrap_or("Arc").to_string()
}

fn find_latest_boot_file(boot_dir: &Path, prefix: &str) -> Result<PathBuf, CaramCtlError> {
    let mut matches: Vec<PathBuf> = fs::read_dir(boot_dir)
        .map_err(|e| CaramCtlError::Io {
            context: format!("Đọc thư mục '{}'", boot_dir.display()),
            source: e,
        })?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with(prefix))
                    .unwrap_or(false)
        })
        .collect();
    matches.sort();
    matches.pop().ok_or_else(|| {
        CaramCtlError::InvalidConfig(format!(
            "Không tìm thấy tệp '{}' trong '{}'",
            prefix,
            boot_dir.display()
        ))
    })
}

impl IsoBuildPipeline {
    fn new(config: BuildConfig) -> Self {
        let rootfs_dir = config.work_dir.join("rootfs");
        let iso_staging_dir = config.work_dir.join("iso");
        Self {
            config,
            rootfs_dir,
            iso_staging_dir,
        }
    }

    fn run<W: Write>(&self, writer: &mut W) -> Result<(), CaramCtlError> {
        let packages = parse_packages_file(&self.config.packages_file)?;
        fs::create_dir_all(&self.rootfs_dir).map_err(|e| CaramCtlError::Io {
            context: format!("Tạo '{}'", self.rootfs_dir.display()),
            source: e,
        })?;
        fs::create_dir_all(&self.iso_staging_dir).map_err(|e| CaramCtlError::Io {
            context: format!("Tạo '{}'", self.iso_staging_dir.display()),
            source: e,
        })?;

        let _ = writeln!(
            writer,
            "▶ [STAGE 1] Bootstrap Debian {} ({}) với {} gói (dry_run={})",
            self.config.suite,
            self.config.arch,
            packages.len(),
            self.config.dry_run
        );

        if !self.config.dry_run {
            let include_arg = format!("--include={}", packages.join(","));
            let arch_arg = format!("--arch={}", self.config.arch);
            let rootfs_s = self.rootfs_dir.to_string_lossy().to_string();
            run_cmd_checked(
                "mmdebstrap",
                &[
                    "--variant=minbase",
                    &arch_arg,
                    "--components=main,contrib,non-free,non-free-firmware",
                    &include_arg,
                    &self.config.suite,
                    &rootfs_s,
                    &self.config.mirror,
                ],
                "mmdebstrap",
            )?;
        }

        self.configure_chroot_and_apply_overlay(writer)?;
        self.build_squashfs_and_iso(writer)?;
        Ok(())
    }

    fn configure_chroot_and_apply_overlay<W: Write>(
        &self,
        writer: &mut W,
    ) -> Result<(), CaramCtlError> {
        let _ = writeln!(
            writer,
            "▶ [STAGE 2] Áp overlay, nhị phân Rust, Fcitx5 Lotus & tài nguyên CaramOS..."
        );

        if let Some(ref inc) = self.config.includes_dir {
            let count = copy_directory_tree_overlay(inc, &self.rootfs_dir)?;
            let _ = writeln!(writer, "  ✔ Đã sao chép {} tệp từ includes_dir.", count);
        }
        for overlay in &self.config.extra_overlays {
            let count = copy_directory_tree_overlay(overlay, &self.rootfs_dir)?;
            let _ = writeln!(
                writer,
                "  ✔ Đã sao chép {} tệp từ overlay '{}'.",
                count,
                overlay.display()
            );
        }

        let bin_dir = self.rootfs_dir.join("usr/local/bin");
        fs::create_dir_all(&bin_dir).map_err(|e| CaramCtlError::Io {
            context: format!("Tạo '{}'", bin_dir.display()),
            source: e,
        })?;
        for bin in &self.config.stage_bins {
            let Some(name) = bin.file_name() else {
                continue;
            };
            let dst = bin_dir.join(name);
            fs::copy(bin, &dst).map_err(|e| CaramCtlError::Io {
                context: format!("Chép '{}'", bin.display()),
                source: e,
            })?;
            #[cfg(unix)]
            {
                let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755));
            }
        }

        let os_release = format!(
            "PRETTY_NAME=\"CaramOS 2.0 (Debian 13 Trixie)\"\n\
             NAME=\"CaramOS\"\n\
             VERSION_ID=\"2.0\"\n\
             VERSION=\"2.0 (Trixie)\"\n\
             VERSION_CODENAME=\"{}\"\n\
             ID=caramos\n\
             ID_LIKE=debian\n\
             HOME_URL=\"https://vietnamlinuxfamily.net/\"\n\
             SUPPORT_URL=\"https://github.com/LocShadowVN/CaramOS-testing\"\n",
            self.config.suite
        );
        atomic_write_file(
            &self.rootfs_dir.join("etc/os-release"),
            os_release.as_bytes(),
        )?;

        self.configure_vietnamese_caramos_defaults()?;

        let lotus_installed = if !self.config.dry_run {
            let _dns_guard = ResolvConfGuard::activate(&self.rootfs_dir);
            let ok = install_fcitx5_lotus_in_chroot(&self.rootfs_dir, &self.config.suite, writer);
            caramos_assets::install_caramos_desktop_assets(&self.rootfs_dir, writer)?;
            let _ = run_chroot_if_present(
                &self.rootfs_dir,
                "/usr/bin/flatpak",
                &[
                    "remote-add",
                    "--if-not-exists",
                    "flathub",
                    "https://dl.flathub.org/repo/flathub.flatpakrepo",
                ],
                "flatpak remote-add flathub",
            );
            run_chroot_if_present(&self.rootfs_dir, "/usr/bin/fc-cache", &["-f"], "fc-cache")?;
            run_chroot_if_present(&self.rootfs_dir, "/usr/sbin/locale-gen", &[], "locale-gen")?;
            let _ = run_chroot_if_present(
                &self.rootfs_dir,
                "/usr/sbin/plymouth-set-default-theme",
                &["-R", "caramos"],
                "plymouth-set-default-theme",
            );
            ok
        } else {
            true
        };

        write_fcitx5_profile_and_config(&self.rootfs_dir, lotus_installed)?;
        self.configure_cinnamon_caramos_desktop()?;

        if !self.config.dry_run {
            run_chroot_if_present(
                &self.rootfs_dir,
                "/usr/bin/dconf",
                &["update"],
                "dconf update",
            )?;
            run_chroot_if_present(
                &self.rootfs_dir,
                "/usr/bin/glib-compile-schemas",
                &["/usr/share/glib-2.0/schemas"],
                "glib-compile-schemas",
            )?;
            run_chroot_if_present(
                &self.rootfs_dir,
                "/usr/bin/update-desktop-database",
                &["/usr/share/applications"],
                "update-desktop-database",
            )?;
            let _ = run_chroot_if_present(
                &self.rootfs_dir,
                "/usr/bin/systemctl",
                &["enable", "lightdm", "NetworkManager", "earlyoom"],
                "systemctl enable",
            );
        }
        Ok(())
    }

    fn configure_vietnamese_caramos_defaults(&self) -> Result<(), CaramCtlError> {
        let etc = self.rootfs_dir.join("etc");
        atomic_write_file(
            &etc.join("locale.gen"),
            b"vi_VN.UTF-8 UTF-8\nen_US.UTF-8 UTF-8\n",
        )?;
        atomic_write_file(
            &etc.join("default/locale"),
            b"LANG=vi_VN.UTF-8\nLANGUAGE=vi_VN:vi\n",
        )?;
        atomic_write_file(&etc.join("timezone"), b"Asia/Ho_Chi_Minh\n")?;
        atomic_write_file(&etc.join("skel/.xinputrc"), b"run_im fcitx5\n")?;
        atomic_write_file(
            &etc.join("environment.d/60-fcitx5.conf"),
            b"GTK_IM_MODULE=fcitx\nQT_IM_MODULE=fcitx\nXMODIFIERS=@im=fcitx\nSDL_IM_MODULE=fcitx\n",
        )?;
        Ok(())
    }

    fn configure_cinnamon_caramos_desktop(&self) -> Result<(), CaramCtlError> {
        let themes = self.rootfs_dir.join("usr/share/themes");
        let icons = self.rootfs_dir.join("usr/share/icons");
        let applets = self.rootfs_dir.join("usr/share/cinnamon/applets");

        let shell_theme = pick_existing_theme(
            &themes,
            &["Cinnamon-Delight", "CaramOS-Island", "Arc"],
            "cinnamon",
        );
        let gtk_theme = pick_existing_theme(&themes, &["Cinnamon-Delight", "Arc"], "gtk-3.0");
        let icon_theme = pick_existing_theme(
            &icons,
            &["Tela-circle-light", "Tela-circle", "Papirus"],
            "index.theme",
        );
        let cursor_theme = pick_existing_theme(
            &icons,
            &["Bibata-Modern-Classic", "breeze_cursors"],
            "cursors",
        );
        let menu_applet = if applets.join("Cinnamenu@json").is_dir() {
            "Cinnamenu@json"
        } else {
            "menu@cinnamon.org"
        };
        let (ui_font, title_font) = if self
            .rootfs_dir
            .join("usr/share/fonts/truetype/be-vietnam-pro")
            .is_dir()
        {
            ("Be Vietnam Pro 10", "Be Vietnam Pro Semi-Bold 10")
        } else {
            ("Noto Sans 10", "Noto Sans Semi-Bold 10")
        };

        let dconf_island = format!(
            "[org/cinnamon]\n\
             panels-enabled=['1:0:bottom']\n\
             panels-autohide=['1:false']\n\
             panels-height=['1:48']\n\
             panel-scale-text-icons=false\n\
             panel-zone-icon-sizes='[{{\"panelId\": 1, \"left\": 32, \"center\": 32, \"right\": 18}}]'\n\
             enabled-applets=['panel1:left:0:{menu}:0', 'panel1:center:0:grouped-window-list@cinnamon.org:1', 'panel1:right:0:systray@cinnamon.org:2', 'panel1:right:1:notifications@cinnamon.org:5', 'panel1:right:2:calendar@cinnamon.org:7', 'panel1:right:3:network@cinnamon.org:4', 'panel1:right:4:sound@cinnamon.org:6', 'panel1:right:5:power@cinnamon.org:8', 'panel1:right:6:cornerbar@cinnamon.org:9']\n\n\
             [org/cinnamon/theme]\n\
             name='{shell}'\n\n\
             [org/cinnamon/desktop/interface]\n\
             gtk-theme='{gtk}'\n\
             icon-theme='{icon}'\n\
             cursor-theme='{cursor}'\n\
             font-name='{font}'\n\n\
             [org/cinnamon/desktop/wm/preferences]\n\
             theme='{gtk}'\n\
             titlebar-font='{title_font}'\n\
             button-layout=':minimize,maximize,close'\n\n\
             [org/cinnamon/applets/grouped-window-list]\n\
             pinned-apps=['firefox-esr.desktop','nemo.desktop','caramos-app-center.desktop','cinnamon-settings.desktop']\n",
            menu = menu_applet, shell = shell_theme, gtk = gtk_theme, icon = icon_theme,
            cursor = cursor_theme, font = ui_font, title_font = title_font
        );

        let gschema_override = format!(
            "[org.cinnamon]\n\
             panels-height=['1:48']\n\
             panel-scale-text-icons=false\n\
             panel-zone-icon-sizes='[{{\"panelId\": 1, \"left\": 32, \"center\": 32, \"right\": 18}}]'\n\n\
             [org.cinnamon.theme]\n\
             name='{shell}'\n\n\
             [org.cinnamon.desktop.interface]\n\
             gtk-theme='{gtk}'\n\
             icon-theme='{icon}'\n\
             cursor-theme='{cursor}'\n\
             font-name='{font}'\n\n\
             [org.cinnamon.desktop.wm.preferences]\n\
             theme='{gtk}'\n\
             titlebar-font='{title_font}'\n",
            shell = shell_theme, gtk = gtk_theme, icon = icon_theme,
            cursor = cursor_theme, font = ui_font, title_font = title_font
        );

        atomic_write_file(
            &self
                .rootfs_dir
                .join("etc/dconf/db/local.d/99-caramos-island"),
            dconf_island.as_bytes(),
        )?;
        atomic_write_file(
            &self
                .rootfs_dir
                .join("usr/share/glib-2.0/schemas/99-caramos-island.gschema.override"),
            gschema_override.as_bytes(),
        )?;
        Ok(())
    }

    fn build_squashfs_and_iso<W: Write>(&self, writer: &mut W) -> Result<(), CaramCtlError> {
        let live_dir = self.iso_staging_dir.join("live");
        let grub_dir = self.iso_staging_dir.join("boot/grub");
        fs::create_dir_all(&live_dir).map_err(|e| CaramCtlError::Io {
            context: format!("Tạo '{}'", live_dir.display()),
            source: e,
        })?;
        fs::create_dir_all(&grub_dir).map_err(|e| CaramCtlError::Io {
            context: format!("Tạo '{}'", grub_dir.display()),
            source: e,
        })?;

        let squashfs_path = live_dir.join("filesystem.squashfs");
        let grub_cfg = r##"set default=0
set timeout=8
insmod all_video
insmod gfxterm
insmod png
terminal_output gfxterm
if [ -f /boot/grub/themes/caramos/theme.txt ]; then
    set theme=/boot/grub/themes/caramos/theme.txt
fi

menuentry "CaramOS 2.0 (Debian 13 — Dùng thử & Cài đặt)" {
    linux /live/vmlinuz boot=live components locales=vi_VN.UTF-8 timezone=Asia/Ho_Chi_Minh quiet splash
    initrd /live/initrd.img
}

menuentry "CaramOS 2.0 (Chế độ đồ họa an toàn — nomodeset)" {
    linux /live/vmlinuz boot=live components locales=vi_VN.UTF-8 timezone=Asia/Ho_Chi_Minh nomodeset
    initrd /live/initrd.img
}
"##;
        atomic_write_file(&grub_dir.join("grub.cfg"), grub_cfg.as_bytes())?;

        let _ = writeln!(
            writer,
            "▶ [STAGE 3 & 4] Đóng gói '{}' -> '{}'",
            squashfs_path.display(),
            self.config.output_iso.display()
        );

        if self.config.dry_run {
            let _ = writeln!(
                writer,
                "✔ Hoàn tất chế độ kiểm thử --dry-run (không tạo ảnh ISO)."
            );
            return Ok(());
        }

        let boot_dir = self.rootfs_dir.join("boot");
        let vmlinuz = find_latest_boot_file(&boot_dir, "vmlinuz-")?;
        let initrd = find_latest_boot_file(&boot_dir, "initrd.img-")?;
        fs::copy(&vmlinuz, live_dir.join("vmlinuz")).map_err(|e| CaramCtlError::Io {
            context: format!("Sao chép kernel '{}'", vmlinuz.display()),
            source: e,
        })?;
        fs::copy(&initrd, live_dir.join("initrd.img")).map_err(|e| CaramCtlError::Io {
            context: format!("Sao chép initrd '{}'", initrd.display()),
            source: e,
        })?;

        let theme_src = self.rootfs_dir.join("boot/grub/themes/caramos");
        if theme_src.is_dir() {
            let _ = copy_directory_tree_overlay(&theme_src, &grub_dir.join("themes/caramos"))?;
        }

        // GIỮ /boot bên trong squashfs để Calamares giải nén lên ổ cứng có đầy đủ kernel & GRUB theme
        let rootfs_s = self.rootfs_dir.to_string_lossy().to_string();
        let sq_s = squashfs_path.to_string_lossy().to_string();
        run_cmd_checked(
            "mksquashfs",
            &[&rootfs_s, &sq_s, "-comp", "zstd", "-noappend"],
            "mksquashfs",
        )?;

        if let Some(parent) = self.config.output_iso.parent() {
            fs::create_dir_all(parent).map_err(|e| CaramCtlError::Io {
                context: format!("Tạo thư mục đầu ra '{}'", parent.display()),
                source: e,
            })?;
        }

        let iso_out = self.config.output_iso.to_string_lossy().to_string();
        let stage_s = self.iso_staging_dir.to_string_lossy().to_string();
        run_cmd_checked(
            "grub-mkrescue",
            &["-o", &iso_out, &stage_s, "--", "-volid", "CARAMOS_2_0"],
            "grub-mkrescue",
        )?;
        Ok(())
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Prune { repo_root, execute } => run_prune(&repo_root, execute),
        Commands::Build {
            arch,
            suite,
            mirror,
            work_dir,
            includes_dir,
            extra_overlays,
            stage_bins,
            packages_file,
            output_iso,
            dry_run,
        } => {
            let pipeline = IsoBuildPipeline::new(BuildConfig {
                arch,
                suite,
                mirror,
                work_dir,
                includes_dir,
                extra_overlays,
                stage_bins,
                packages_file,
                output_iso,
                dry_run,
            });
            pipeline.run(&mut io::stdout())
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[caramos-ctl] Lỗi: {}", e);
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
    fn parse_packages_filters_comments_and_rejects_injection() {
        let dir = tempdir().unwrap();
        let ok_file = dir.path().join("packages.txt");
        fs::write(
            &ok_file,
            "# comment\ncinnamon-core\nfirefox-esr # browser\n",
        )
        .unwrap();
        assert_eq!(
            parse_packages_file(&ok_file).unwrap(),
            vec!["cinnamon-core", "firefox-esr"]
        );

        let bad_file = dir.path().join("bad.txt");
        fs::write(&bad_file, "vlc;rm -rf /\n").unwrap();
        assert!(parse_packages_file(&bad_file).is_err());
    }

    #[test]
    fn dry_run_build_writes_debian_os_release_fcitx5_profile_grub_and_48px_panel() {
        let dir = tempdir().unwrap();
        let pkg = dir.path().join("packages.txt");
        fs::write(&pkg, "cinnamon-core\n").unwrap();
        let work = dir.path().join("work");
        let pipeline = IsoBuildPipeline::new(BuildConfig {
            arch: "amd64".to_string(),
            suite: "trixie".to_string(),
            mirror: "http://deb.debian.org/debian".to_string(),
            work_dir: work.clone(),
            includes_dir: None,
            extra_overlays: Vec::new(),
            stage_bins: Vec::new(),
            packages_file: pkg,
            output_iso: work.join("out.iso"),
            dry_run: true,
        });
        let mut log = Vec::new();
        pipeline.run(&mut log).unwrap();

        let os_rel = fs::read_to_string(work.join("rootfs/etc/os-release")).unwrap();
        assert!(os_rel.contains("ID_LIKE=debian"));
        let locale = fs::read_to_string(work.join("rootfs/etc/default/locale")).unwrap();
        assert!(!locale.contains("LC_ALL="));
        let profile =
            fs::read_to_string(work.join("rootfs/etc/skel/.config/fcitx5/profile")).unwrap();
        assert!(profile.contains("DefaultIM=lotus"));
        let fcitx_cfg =
            fs::read_to_string(work.join("rootfs/etc/skel/.config/fcitx5/config")).unwrap();
        assert!(fcitx_cfg.contains("Control+Shift_L"));
        let island =
            fs::read_to_string(work.join("rootfs/etc/dconf/db/local.d/99-caramos-island")).unwrap();
        assert!(island.contains("panels-height=['1:48']"));
        assert!(work.join("iso/boot/grub/grub.cfg").is_file());
    }
}

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

use caramos_desktop::{install_caramos_css, invoke_self_pkexec, CaramDesktopError};
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use libadwaita::prelude::AdwApplicationWindowExt;
use std::cell::{Cell, RefCell};
use std::env;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::Duration;

const APP_ID: &str = "org.caramos.UpdateCenter";

type RefreshFnSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

#[derive(Debug, Clone)]
struct PendingUpdate {
    source_label: &'static str,
    name: String,
    version_note: String,
}

#[derive(Debug, Clone)]
struct UpdateScan {
    apt_items: Vec<PendingUpdate>,
    flatpak_items: Vec<PendingUpdate>,
}

impl UpdateScan {
    fn total(&self) -> usize {
        self.apt_items.len() + self.flatpak_items.len()
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

fn scan_pending_updates() -> UpdateScan {
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
        apt_items,
        flatpak_items,
    }
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
        .default_width(660)
        .default_height(600)
        .build();

    let toast_overlay = adw::ToastOverlay::new();
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        "Trung tâm Cập nhật",
        "Hệ thống Debian 13 & ứng dụng Flathub",
    )));
    let refresh_btn = gtk4::Button::from_icon_name("view-refresh-symbolic");
    refresh_btn.set_tooltip_text(Some("Kiểm tra bản cập nhật mới"));
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
    let check_lbl = gtk4::Label::new(Some("Đang kiểm tra bản cập nhật mới..."));
    check_lbl.add_css_class("dim-label");
    check_box.append(&spin);
    check_box.append(&check_lbl);
    stack.add_named(&check_box, Some("checking"));

    // Trang 2: Đã mới nhất
    let up_to_date = adw::StatusPage::builder()
        .icon_name("emblem-ok-symbolic")
        .title("Hệ thống đã được cập nhật mới nhất")
        .description("Tất cả gói hệ thống Debian và ứng dụng Flathub đều ở phiên bản mới nhất.")
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
        .maximum_size(620)
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

    // Trang 4: Đang cập nhật
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
    let log_exp = gtk4::Expander::new(Some("Xem nhật ký cập nhật"));
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
        .title("Đã cập nhật xong")
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

    let render_scan = Rc::new({
        let updates_container = updates_container.clone();
        let footer_lbl = footer_lbl.clone();
        let stack = stack.clone();
        move |scan: &UpdateScan| {
            while let Some(c) = updates_container.first_child() {
                updates_container.remove(&c);
            }
            if scan.total() == 0 {
                stack.set_visible_child_name("up-to-date");
                return;
            }
            footer_lbl.set_text(&format!(
                "Có {} bản cập nhật ({} gói hệ thống, {} ứng dụng Flathub)",
                scan.total(),
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
        let current_scan = Rc::clone(&current_scan);
        let render_scan = Rc::clone(&render_scan);
        let is_busy = Rc::clone(&is_busy);
        move || {
            if is_busy.get() {
                return;
            }
            stack.set_visible_child_name("checking");
            refresh_btn.set_sensitive(false);
            let (tx, rx) = async_channel::bounded::<UpdateScan>(1);
            thread::spawn(move || {
                let _ = tx.send_blocking(scan_pending_updates());
            });
            let btn_d = refresh_btn.clone();
            let scan_d = Rc::clone(&current_scan);
            let render_d = Rc::clone(&render_scan);
            glib::spawn_future_local(async move {
                if let Ok(rep) = rx.recv().await {
                    render_d(&rep);
                    *scan_d.borrow_mut() = Some(rep);
                    btn_d.set_sensitive(true);
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
        let w = window.clone();
        done_close_btn.connect_clicked(move |_| w.close());
    }
    {
        let busy_c = Rc::clone(&is_busy);
        let toast_c = toast_overlay.clone();
        window.connect_close_request(move |win| {
            if busy_c.get() {
                win.set_visible(false);
                toast_c.add_toast(adw::Toast::new("Đang tiếp tục cập nhật ở chế độ nền"));
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    {
        let app_u = app.clone();
        let win_u = window.clone();
        let stack_u = stack.clone();
        let refresh_u = refresh_btn.clone();
        let status_u = prog_status.clone();
        let bar_u = prog_bar.clone();
        let log_u = log_buf.clone();
        let done_u = done_page.clone();
        let scan_u = Rc::clone(&current_scan);
        let busy_u = Rc::clone(&is_busy);
        let hold_u = Rc::clone(&hold_guard);

        update_now_btn.connect_clicked(move |_| {
            if busy_u.get() {
                return;
            }
            let (has_apt, has_fp) = scan_u
                .borrow()
                .as_ref()
                .map(|s| (!s.apt_items.is_empty(), !s.flatpak_items.is_empty()))
                .unwrap_or((true, true));
            busy_u.set(true);
            *hold_u.borrow_mut() = Some(app_u.hold());
            refresh_u.set_sensitive(false);
            log_u.set_text("");
            status_u.set_text("Đang bắt đầu quá trình cập nhật...");
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
            thread::spawn(move || {
                let mut errors: Vec<String> = Vec::new();
                if has_apt {
                    let _ = tx.send_blocking(UpgradeEvent::Step {
                        headline: "Đang nâng cấp các gói hệ thống Debian (APT)...".to_string(),
                        log: "pkexec caramos-update-center --pkexec-upgrade".to_string(),
                    });
                    if let Err(e) = invoke_self_pkexec("--pkexec-upgrade", &[]) {
                        errors.push(e.to_string());
                    }
                }
                if has_fp {
                    let _ = tx.send_blocking(UpgradeEvent::Step {
                        headline: "Đang cập nhật ứng dụng Flathub...".to_string(),
                        log: "flatpak update -y --noninteractive --user".to_string(),
                    });
                    let fp = Command::new("flatpak")
                        .args(["update", "-y", "--noninteractive", "--user"])
                        .output();
                    if let Ok(out) = fp {
                        if !out.status.success() {
                            errors.push(String::from_utf8_lossy(&out.stderr).trim().to_string());
                        }
                    }
                }
                let reboot_required = Path::new("/var/run/reboot-required").exists();
                let _ = tx.send_blocking(UpgradeEvent::Done {
                    ok: errors.is_empty(),
                    detail: if errors.is_empty() {
                        if reboot_required {
                            "Cập nhật thành công. Nên khởi động lại máy để nạp nhân Linux mới."
                                .to_string()
                        } else {
                            "Toàn bộ hệ thống và ứng dụng đã ở phiên bản mới nhất.".to_string()
                        }
                    } else {
                        errors.join("\n")
                    },
                    reboot_required,
                });
            });

            let app_d = app_u.clone();
            let win_d = win_u.clone();
            let stack_d = stack_u.clone();
            let refresh_d = refresh_u.clone();
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
                            let title = if ok && reboot_required {
                                "Đã cập nhật xong (Cần khởi động lại)"
                            } else if ok {
                                "Đã cập nhật xong"
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

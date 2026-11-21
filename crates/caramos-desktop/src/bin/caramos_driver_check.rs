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

use caramos_desktop::{
    caramos_user_state_dir, export_hardware_report_to_desktop, install_caramos_css,
    invoke_self_pkexec, is_live_session, run_privileged_apt_install, scan_system_drivers,
    DriverScanReport, DriverStatus, HardwareCategory,
};
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::env;
use std::fs;
use std::process::{Command, ExitCode};
use std::rc::Rc;

const VNLF_HELP_URL: &str = "https://github.com/VN-Linux-Family/CaramOS/issues";

enum DriverWorkerMsg {
    Finished { ok: bool, message: String },
}

fn populate_report_ui(
    content_box: &gtk4::Box,
    report: &DriverScanReport,
    fix_btn: &gtk4::Button,
    summary_label: &gtk4::Label,
) {
    while let Some(child) = content_box.first_child() {
        content_box.remove(&child);
    }

    let issues = report.issue_count();
    let rec_pkgs = report.all_recommended_packages();

    let banner = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    banner.add_css_class(if issues == 0 {
        "caramos-hero-ok"
    } else {
        "caramos-hero-warn"
    });

    let title = gtk4::Label::builder().xalign(0.0).wrap(true).build();
    title.add_css_class("title-2");
    let subtitle = gtk4::Label::builder().xalign(0.0).wrap(true).build();
    subtitle.add_css_class("dim-label");

    if issues == 0 {
        title.set_label("Máy tính này hoạt động tốt với CaramOS");
        subtitle.set_label(
            "Wi-Fi, đồ họa, âm thanh và dịch vụ in ấn đều đã nạp driver đầy đủ. Bạn có thể yên tâm sử dụng hoặc cài đặt lên máy tính.",
        );
    } else {
        title.set_label(&format!(
            "Phát hiện {} thành phần cần bổ sung driver hoặc firmware",
            issues
        ));
        subtitle.set_label(
            "Xem chi tiết từng thiết bị bên dưới. Bấm Cài driver đề xuất để CaramOS tự khắc phục chỉ với một cú bấm.",
        );
    }

    banner.append(&title);
    banner.append(&subtitle);

    if report.secure_boot_enabled {
        let sb_note = gtk4::Label::builder()
            .label("Lưu ý: Secure Boot đang BẬT. Nếu máy dùng card NVIDIA hoặc Wi-Fi Broadcom DKMS, có thể cần tắt Secure Boot trong BIOS/UEFI.")
            .xalign(0.0)
            .wrap(true)
            .build();
        sb_note.add_css_class("caption");
        sb_note.add_css_class("warning");
        banner.append(&sb_note);
    }

    content_box.append(&banner);

    for cat in HardwareCategory::ALL {
        let cat_items: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.category == cat)
            .collect();
        if cat_items.is_empty() {
            continue;
        }

        let group = adw::PreferencesGroup::builder().title(cat.title()).build();

        for item in cat_items {
            let pkg_note = if item.recommended_packages.is_empty() {
                String::new()
            } else {
                format!("\nGói đề xuất: {}", item.recommended_packages.join(", "))
            };
            let row_sub = format!("{}{}", item.explanation, pkg_note);

            let row = adw::ActionRow::builder()
                .title(&item.device_name)
                .subtitle(row_sub)
                .build();
            row.set_use_markup(false);

            let badge = gtk4::Label::new(Some(item.status.badge_text()));
            badge.set_valign(gtk4::Align::Center);
            match item.status {
                DriverStatus::Working => badge.add_css_class("caramos-pill-ok"),
                DriverStatus::MissingFirmware | DriverStatus::NeedsReboot => {
                    badge.add_css_class("caramos-pill-warn");
                }
                DriverStatus::MissingDriver | DriverStatus::Blocked => {
                    badge.add_css_class("caramos-pill-err");
                }
            }
            row.add_suffix(&badge);
            group.add(&row);
        }

        content_box.append(&group);
    }

    if rec_pkgs.is_empty() {
        fix_btn.set_sensitive(false);
        fix_btn.set_label("Không cần cài thêm gói");
        summary_label.set_label("Tất cả gói driver đề xuất đã được cài đặt");
    } else {
        fix_btn.set_sensitive(true);
        fix_btn.set_label(&format!("Cài {} gói driver đề xuất", rec_pkgs.len()));
        summary_label.set_label(&format!("Gói cần cài: {}", rec_pkgs.join(", ")));
    }
}

fn build_ui(app: &adw::Application, initial_report: DriverScanReport) {
    install_caramos_css();

    let report_rc = Rc::new(RefCell::new(initial_report));
    let busy_rc = Rc::new(RefCell::new(false));

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        "Kiểm tra Driver CaramOS",
        "Chẩn đoán Wi-Fi, Đồ họa, Âm thanh và Máy in",
    )));

    let rescan_btn = gtk4::Button::with_label("Quét lại");
    rescan_btn.add_css_class("flat");
    header.pack_start(&rescan_btn);

    let style_mgr = adw::StyleManager::default();
    let theme_btn = gtk4::Button::with_label(if style_mgr.is_dark() { "Sáng" } else { "Tối" });
    theme_btn.add_css_class("flat");
    {
        let mgr = style_mgr.clone();
        theme_btn.connect_clicked(move |btn| {
            if mgr.is_dark() {
                mgr.set_color_scheme(adw::ColorScheme::ForceLight);
                btn.set_label("Tối");
            } else {
                mgr.set_color_scheme(adw::ColorScheme::ForceDark);
                btn.set_label("Sáng");
            }
        });
    }
    header.pack_end(&theme_btn);

    let help_btn = gtk4::Button::with_label("Nhờ cộng đồng giúp đỡ");
    help_btn.add_css_class("flat");
    header.pack_end(&help_btn);

    let outer_box = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    let progress_bar = gtk4::ProgressBar::builder()
        .show_text(true)
        .hexpand(true)
        .visible(false)
        .build();
    let content_box = gtk4::Box::new(gtk4::Orientation::Vertical, 16);

    outer_box.append(&progress_bar);
    outer_box.append(&content_box);

    let summary_label = gtk4::Label::builder()
        .label("")
        .xalign(0.0)
        .hexpand(true)
        .wrap(true)
        .build();
    let fix_btn = gtk4::Button::with_label("Cài driver đề xuất");
    fix_btn.add_css_class("suggested-action");
    fix_btn.add_css_class("pill");

    populate_report_ui(&content_box, &report_rc.borrow(), &fix_btn, &summary_label);

    let (tx, rx) = async_channel::unbounded::<DriverWorkerMsg>();

    {
        let rep_rc = Rc::clone(&report_rc);
        let c_box = content_box.clone();
        let f_btn = fix_btn.clone();
        let s_lbl = summary_label.clone();
        let b_rc = Rc::clone(&busy_rc);
        rescan_btn.connect_clicked(move |_| {
            if *b_rc.borrow() {
                return;
            }
            let fresh = scan_system_drivers();
            populate_report_ui(&c_box, &fresh, &f_btn, &s_lbl);
            *rep_rc.borrow_mut() = fresh;
        });
    }

    {
        let rep_rc = Rc::clone(&report_rc);
        let p_bar = progress_bar.clone();
        help_btn.connect_clicked(move |_| {
            let report = rep_rc.borrow().clone();
            p_bar.set_visible(true);
            match export_hardware_report_to_desktop(&report) {
                Ok(path) => {
                    p_bar.set_fraction(1.0);
                    p_bar.set_text(Some(&format!(
                        "Đã lưu báo cáo phần cứng tại {} — hãy đính kèm khi gửi hỗ trợ.",
                        path.display()
                    )));
                }
                Err(e) => {
                    p_bar.set_fraction(0.0);
                    p_bar.set_text(Some(&e));
                }
            }
            let _ = Command::new("xdg-open").arg(VNLF_HELP_URL).spawn();
        });
    }

    {
        let rep_rc = Rc::clone(&report_rc);
        let b_rc = Rc::clone(&busy_rc);
        let f_btn = fix_btn.clone();
        let r_btn = rescan_btn.clone();
        let p_bar = progress_bar.clone();
        let tx_inst = tx.clone();

        fix_btn.connect_clicked(move |_| {
            if *b_rc.borrow() {
                return;
            }
            let pkgs = rep_rc.borrow().all_recommended_packages();
            if pkgs.is_empty() {
                return;
            }

            *b_rc.borrow_mut() = true;
            f_btn.set_sensitive(false);
            r_btn.set_sensitive(false);
            p_bar.set_visible(true);
            p_bar.set_fraction(0.35);
            p_bar.set_text(Some(&format!(
                "Đang cài đặt {} (yêu cầu mật khẩu quản trị 1 lần)...",
                pkgs.join(", ")
            )));

            let tx_worker = tx_inst.clone();
            std::thread::spawn(move || {
                let msg = match invoke_self_pkexec("--privileged-apt-install", &pkgs) {
                    Ok(()) => DriverWorkerMsg::Finished {
                        ok: true,
                        message: "Đã cài đặt driver/firmware xong! Hãy khởi động lại máy nếu thiết bị yêu cầu."
                            .to_string(),
                    },
                    Err(e) => DriverWorkerMsg::Finished {
                        ok: false,
                        message: format!("Cài driver thất bại: {}", e),
                    },
                };
                let _ = tx_worker.send_blocking(msg);
            });
        });
    }

    {
        let rep_rc = Rc::clone(&report_rc);
        let b_rc = Rc::clone(&busy_rc);
        let c_box = content_box.clone();
        let f_btn = fix_btn.clone();
        let r_btn = rescan_btn.clone();
        let s_lbl = summary_label.clone();
        let p_bar = progress_bar.clone();

        gtk4::glib::spawn_future_local(async move {
            while let Ok(DriverWorkerMsg::Finished { ok, message }) = rx.recv().await {
                *b_rc.borrow_mut() = false;
                r_btn.set_sensitive(true);
                p_bar.set_fraction(if ok { 1.0 } else { 0.0 });
                p_bar.set_text(Some(&message));
                let fresh = scan_system_drivers();
                populate_report_ui(&c_box, &fresh, &f_btn, &s_lbl);
                *rep_rc.borrow_mut() = fresh;
            }
        });
    }

    let clamp = adw::Clamp::builder()
        .maximum_size(820)
        .margin_top(16)
        .margin_bottom(24)
        .margin_start(16)
        .margin_end(16)
        .child(&outer_box)
        .build();

    let scrolled = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&clamp)
        .build();

    let bottom_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    bottom_box.set_margin_top(10);
    bottom_box.set_margin_bottom(10);
    bottom_box.set_margin_start(16);
    bottom_box.set_margin_end(16);
    bottom_box.append(&summary_label);
    bottom_box.append(&fix_btn);

    let bottom_container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    bottom_container.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    bottom_container.append(&bottom_box);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scrolled));
    toolbar.add_bottom_bar(&bottom_container);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Kiểm tra Driver CaramOS")
        .default_width(860)
        .default_height(640)
        .content(&toolbar)
        .build();
    window.present();
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    if let Some(mode) = args.get(1).map(String::as_str) {
        if mode == "--privileged-apt-install" {
            let pkgs: Vec<String> = args.iter().skip(2).cloned().collect();
            return match run_privileged_apt_install(&pkgs) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("✖ {}", e);
                    ExitCode::from(1)
                }
            };
        }
    }

    let report = scan_system_drivers();
    let is_autostart = args.iter().any(|a| a == "--autostart");

    if is_autostart && !is_live_session() {
        let state_dir = caramos_user_state_dir();
        let state_file = state_dir.join("driver-check-state.txt");
        let fp = report.fingerprint();

        if let Ok(prev_fp) = fs::read_to_string(&state_file) {
            if prev_fp.trim() == fp || report.issue_count() == 0 {
                return ExitCode::SUCCESS;
            }
        }
        let _ = fs::create_dir_all(&state_dir);
        let _ = fs::write(&state_file, format!("{}\n", fp));
    }

    let app = adw::Application::builder()
        .application_id("vn.caramos.DriverCheck")
        .build();

    let report_once = Rc::new(RefCell::new(Some(report)));
    app.connect_activate(move |app| {
        let initial = report_once
            .borrow_mut()
            .take()
            .unwrap_or_else(scan_system_drivers);
        build_ui(app, initial);
    });

    let exit_code = app.run_with_args::<&str>(&[]);
    if exit_code == gtk4::glib::ExitCode::SUCCESS {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

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
    install_caramos_css, invoke_self_pkexec, load_saved_fingerprint, run_full_hardware_scan,
    run_privileged_apt_install, save_fingerprint, DriverScanReport, DriverStatus, HardwareCategory,
    HardwareFinding,
};
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::env;
use std::process::ExitCode;
use std::rc::Rc;
use std::thread;
use std::time::Duration;

const APP_ID: &str = "org.caramos.DriverCheck";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    if args.get(1).map(String::as_str) == Some("--pkexec-install") {
        let targets: Vec<String> = args.into_iter().skip(2).collect();
        return match run_privileged_apt_install(&targets) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::FAILURE
            }
        };
    }

    let autostart_mode = args
        .iter()
        .any(|a| a == "--autostart" || a == "--check-changes");
    let pre_scanned = if autostart_mode {
        let report = run_full_hardware_scan();
        let fp = report.fingerprint();
        if report.issue_count() == 0 {
            let _ = save_fingerprint(&fp);
            return ExitCode::SUCCESS;
        }
        if load_saved_fingerprint().as_deref() == Some(fp.as_str()) {
            return ExitCode::SUCCESS;
        }
        let _ = save_fingerprint(&fp);
        Some(report)
    } else {
        None
    };

    let pre_slot = Rc::new(RefCell::new(pre_scanned));
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |application| {
        install_caramos_css();
        build_window(application, pre_slot.borrow_mut().take());
    });

    if app.run_with_args(&["caramos-driver-check"]) == glib::ExitCode::SUCCESS {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn build_window(app: &adw::Application, initial_report: Option<DriverScanReport>) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Kiểm tra Driver")
        .default_width(680)
        .default_height(620)
        .build();

    let toast_overlay = adw::ToastOverlay::new();
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        "Kiểm tra Driver",
        "Phần cứng và trình điều khiển Debian",
    )));
    let rescan_btn = gtk4::Button::from_icon_name("view-refresh-symbolic");
    rescan_btn.set_tooltip_text(Some("Quét lại phần cứng"));
    let copy_btn = gtk4::Button::from_icon_name("edit-copy-symbolic");
    copy_btn.set_tooltip_text(Some("Sao chép báo cáo chẩn đoán"));
    header.pack_end(&rescan_btn);
    header.pack_end(&copy_btn);
    root.append(&header);

    let stack = gtk4::Stack::new();
    stack.set_vexpand(true);
    stack.set_transition_type(gtk4::StackTransitionType::Crossfade);

    // Trang 1: Đang quét
    let scan_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    scan_box.set_valign(gtk4::Align::Center);
    scan_box.set_halign(gtk4::Align::Center);
    let scan_spin = gtk4::Spinner::new();
    scan_spin.set_size_request(36, 36);
    scan_spin.start();
    let scan_lbl = gtk4::Label::new(Some("Đang quét thiết bị PCI, USB và nhật ký firmware..."));
    scan_lbl.add_css_class("dim-label");
    scan_box.append(&scan_spin);
    scan_box.append(&scan_lbl);
    stack.add_named(&scan_box, Some("scanning"));

    // Trang 2: Báo cáo chi tiết
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .build();
    let clamp = adw::Clamp::builder()
        .maximum_size(640)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(24)
        .build();
    let report_box = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    clamp.set_child(Some(&report_box));
    scroll.set_child(Some(&clamp));
    stack.add_named(&scroll, Some("report"));

    // Trang 3: Đang cài driver qua pkexec
    let inst_box = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    inst_box.set_valign(gtk4::Align::Center);
    inst_box.set_margin_start(36);
    inst_box.set_margin_end(36);
    let inst_title = gtk4::Label::new(Some("Đang cài đặt trình điều khiển..."));
    inst_title.add_css_class("page-title");
    let inst_sub = gtk4::Label::new(Some("Vui lòng không tắt máy trong khi APT cài đặt gói."));
    inst_sub.add_css_class("dim-label");
    inst_sub.set_wrap(true);
    let inst_bar = gtk4::ProgressBar::new();
    inst_box.append(&inst_title);
    inst_box.append(&inst_sub);
    inst_box.append(&inst_bar);
    stack.add_named(&inst_box, Some("installing"));

    root.append(&stack);
    toast_overlay.set_child(Some(&root));
    window.set_content(Some(&toast_overlay));

    let latest_report: Rc<RefCell<Option<DriverScanReport>>> = Rc::new(RefCell::new(None));
    let is_installing = Rc::new(Cell::new(false));
    let trigger_scan_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

    let launch_install = Rc::new({
        let stack = stack.clone();
        let inst_sub = inst_sub.clone();
        let inst_bar = inst_bar.clone();
        let rescan_btn = rescan_btn.clone();
        let toast = toast_overlay.clone();
        let is_installing = Rc::clone(&is_installing);
        let trigger_scan_slot = Rc::clone(&trigger_scan_slot);
        move |packages: Vec<String>| {
            if packages.is_empty() || is_installing.get() {
                return;
            }
            is_installing.set(true);
            rescan_btn.set_sensitive(false);
            inst_sub.set_text(&format!("Gói đang cài: {}", packages.join(", ")));
            stack.set_visible_child_name("installing");

            let flag_pulse = Rc::clone(&is_installing);
            let bar_pulse = inst_bar.clone();
            glib::timeout_add_local(Duration::from_millis(120), move || {
                if flag_pulse.get() {
                    bar_pulse.pulse();
                    glib::ControlFlow::Continue
                } else {
                    glib::ControlFlow::Break
                }
            });

            let (tx, rx) = async_channel::bounded::<Result<(), String>>(1);
            thread::spawn(move || {
                let res =
                    invoke_self_pkexec("--pkexec-install", &packages).map_err(|e| e.to_string());
                let _ = tx.send_blocking(res);
            });

            let is_inst_done = Rc::clone(&is_installing);
            let rescan_done = rescan_btn.clone();
            let toast_done = toast.clone();
            let slot_done = Rc::clone(&trigger_scan_slot);
            glib::spawn_future_local(async move {
                if let Ok(res) = rx.recv().await {
                    is_inst_done.set(false);
                    rescan_done.set_sensitive(true);
                    match res {
                        Ok(()) => toast_done.add_toast(adw::Toast::new(
                            "Đã cài driver xong. Khởi động lại máy để áp dụng đầy đủ.",
                        )),
                        Err(e) => toast_done.add_toast(adw::Toast::new(&e)),
                    }
                    if let Some(scan_fn) = slot_done.borrow().as_ref() {
                        scan_fn();
                    }
                }
            });
        }
    });

    let render_report = Rc::new({
        let report_box = report_box.clone();
        let launch_install = Rc::clone(&launch_install);
        move |report: &DriverScanReport| {
            while let Some(c) = report_box.first_child() {
                report_box.remove(&c);
            }

            let net_online = gio::NetworkMonitor::default().is_network_available();
            let all_pkgs = report.all_recommended_packages();
            let disk_ok = report.has_enough_disk_for_fix();

            // Thẻ tóm tắt tổng quan
            let sum_card = gtk4::Box::new(gtk4::Orientation::Horizontal, 14);
            sum_card.add_css_class("card");
            sum_card.add_css_class("summary-card");
            sum_card.set_valign(gtk4::Align::Center);
            let issues = report.issue_count();
            let sum_icon = gtk4::Image::from_icon_name(if issues == 0 {
                "emblem-ok-symbolic"
            } else {
                "dialog-warning-symbolic"
            });
            sum_icon.set_pixel_size(32);
            let sum_text_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
            sum_text_box.set_hexpand(true);
            let sum_title = gtk4::Label::new(Some(&if issues == 0 {
                "Tất cả phần cứng đã sẵn sàng".to_string()
            } else {
                format!("Phát hiện {} thiết bị cần xử lý", issues)
            }));
            sum_title.set_xalign(0.0);
            sum_title.add_css_class("row-title");
            let sum_desc = gtk4::Label::new(Some(if issues == 0 {
                "Các thành phần Wi-Fi, đồ họa, âm thanh và ngoại vi đều hoạt động tốt."
            } else {
                "Xem chi tiết bên dưới và cài gói trình điều khiển đề xuất từ Debian."
            }));
            sum_desc.set_xalign(0.0);
            sum_desc.set_wrap(true);
            sum_desc.add_css_class("dim-label");
            sum_desc.add_css_class("secondary");
            sum_text_box.append(&sum_title);
            sum_text_box.append(&sum_desc);
            sum_card.append(&sum_icon);
            sum_card.append(&sum_text_box);

            if !all_pkgs.is_empty() {
                let fix_all_btn =
                    gtk4::Button::with_label(&format!("Cài tất cả ({} gói)", all_pkgs.len()));
                fix_all_btn.add_css_class("suggested-action");
                fix_all_btn.set_valign(gtk4::Align::Center);
                fix_all_btn.set_sensitive(net_online && disk_ok);
                let pkgs_clone = all_pkgs.clone();
                let inst_fn = Rc::clone(&launch_install);
                fix_all_btn.connect_clicked(move |_| inst_fn(pkgs_clone.clone()));
                sum_card.append(&fix_all_btn);
            }
            report_box.append(&sum_card);

            // Các thẻ cảnh báo ngữ cảnh (Ngoại tuyến/Wi-Fi, Secure Boot, nomodeset, Đĩa trống)
            if !net_online || report.has_wifi_issue() {
                report_box.append(&build_alert_card(
                    "Chưa có Internet hoặc Wi-Fi gặp sự cố",
                    "Hãy cắm dây mạng LAN hoặc cắm cáp USB từ điện thoại Android/iPhone bật 'Chia sẻ kết nối USB', sau đó bấm nút Quét lại.",
                ));
            }
            if report.secure_boot_enabled && report.requires_large_disk_space() {
                report_box.append(&build_alert_card(
                    "Secure Boot đang bật",
                    "Driver NVIDIA cần ký mô-đun nhân (MOK). Khi khởi động lại, hãy chọn Enroll MOK hoặc tắt Secure Boot trong BIOS/UEFI.",
                ));
            }
            if report.nomodeset_active {
                report_box.append(&build_alert_card(
                    "Đang khởi động ở chế độ nomodeset",
                    "Tham số nhân 'nomodeset' đang tắt tăng tốc đồ họa phần cứng. Hãy gỡ nomodeset trong /etc/default/grub sau khi cài driver.",
                ));
            }
            if !disk_ok && !all_pkgs.is_empty() {
                let free_mb = report.root_free_mb.unwrap_or(0);
                report_box.append(&build_alert_card(
                    "Dung lượng ổ đĩa hệ thống thấp",
                    &format!(
                        "Phân vùng '/' chỉ còn {} MB trống. Hãy giải phóng tối thiểu 2.5 GB trước khi cài driver đồ họa.",
                        free_mb
                    ),
                ));
            }

            // Nhóm thiết bị theo từng loại phần cứng
            for cat in HardwareCategory::ALL {
                let items: Vec<&HardwareFinding> = report
                    .findings
                    .iter()
                    .filter(|f| f.category == cat)
                    .collect();
                if items.is_empty() {
                    continue;
                }

                let heading = gtk4::Label::new(Some(cat.title()));
                heading.set_xalign(0.0);
                heading.add_css_class("section-heading");
                report_box.append(&heading);

                let list = gtk4::ListBox::new();
                list.set_selection_mode(gtk4::SelectionMode::None);
                list.add_css_class("boxed-list");

                for item in items {
                    let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
                    row_box.set_valign(gtk4::Align::Center);

                    let info_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 3);
                    info_vbox.set_hexpand(true);
                    let name_lbl = gtk4::Label::new(Some(&item.device_name));
                    name_lbl.set_xalign(0.0);
                    name_lbl.set_wrap(true);
                    name_lbl.add_css_class("row-title");
                    let drv_text = format!(
                        "Driver: {} — {}",
                        item.driver_in_use.as_deref().unwrap_or("Chưa nạp"),
                        item.explanation
                    );
                    let sub_lbl = gtk4::Label::new(Some(&drv_text));
                    sub_lbl.set_xalign(0.0);
                    sub_lbl.set_wrap(true);
                    sub_lbl.add_css_class("dim-label");
                    sub_lbl.add_css_class("secondary");
                    info_vbox.append(&name_lbl);
                    info_vbox.append(&sub_lbl);
                    row_box.append(&info_vbox);

                    let status_lbl = gtk4::Label::new(Some(item.status.status_text()));
                    status_lbl.add_css_class(match item.status {
                        DriverStatus::Working => "status-ok",
                        DriverStatus::NeedsReboot | DriverStatus::MissingFirmware => "status-warn",
                        DriverStatus::MissingDriver | DriverStatus::Blocked => "status-err",
                    });
                    row_box.append(&status_lbl);

                    if item.status.is_issue() && !item.recommended_packages.is_empty() {
                        let btn = gtk4::Button::with_label("Cài gói");
                        btn.set_sensitive(net_online && disk_ok);
                        let pkgs = item.recommended_packages.clone();
                        let inst_fn = Rc::clone(&launch_install);
                        btn.connect_clicked(move |_| inst_fn(pkgs.clone()));
                        row_box.append(&btn);
                    }

                    let row = gtk4::ListBoxRow::new();
                    row.add_css_class("app-row");
                    row.set_child(Some(&row_box));
                    list.append(&row);
                }

                report_box.append(&list);
            }

            if !report.firmware_errors.is_empty() {
                let exp = gtk4::Expander::new(Some(&format!(
                    "Cảnh báo firmware từ nhân Linux ({} dòng)",
                    report.firmware_errors.len()
                )));
                let tv = gtk4::TextView::new();
                tv.set_editable(false);
                tv.set_monospace(true);
                tv.set_wrap_mode(gtk4::WrapMode::WordChar);
                tv.buffer().set_text(&report.firmware_errors.join("\n"));
                exp.set_child(Some(&tv));
                report_box.append(&exp);
            }
        }
    });

    let trigger_scan: Rc<dyn Fn()> = Rc::new({
        let stack = stack.clone();
        let rescan_btn = rescan_btn.clone();
        let latest = Rc::clone(&latest_report);
        let render = Rc::clone(&render_report);
        move || {
            stack.set_visible_child_name("scanning");
            rescan_btn.set_sensitive(false);
            let (tx, rx) = async_channel::bounded::<DriverScanReport>(1);
            thread::spawn(move || {
                let _ = tx.send_blocking(run_full_hardware_scan());
            });
            let stack_d = stack.clone();
            let btn_d = rescan_btn.clone();
            let latest_d = Rc::clone(&latest);
            let render_d = Rc::clone(&render);
            glib::spawn_future_local(async move {
                if let Ok(rep) = rx.recv().await {
                    let _ = save_fingerprint(&rep.fingerprint());
                    render_d(&rep);
                    *latest_d.borrow_mut() = Some(rep);
                    btn_d.set_sensitive(true);
                    stack_d.set_visible_child_name("report");
                }
            });
        }
    });
    *trigger_scan_slot.borrow_mut() = Some(Rc::clone(&trigger_scan));

    {
        let scan_c = Rc::clone(&trigger_scan);
        rescan_btn.connect_clicked(move |_| scan_c());
    }
    {
        let latest_c = Rc::clone(&latest_report);
        let toast_c = toast_overlay.clone();
        copy_btn.connect_clicked(move |_| {
            if let Some(ref rep) = *latest_c.borrow() {
                if let Some(display) = gtk4::gdk::Display::default() {
                    display.clipboard().set_text(&rep.format_text_report());
                    toast_c.add_toast(adw::Toast::new("Đã sao chép báo cáo phần cứng"));
                }
            }
        });
    }

    if let Some(rep) = initial_report {
        render_report(&rep);
        *latest_report.borrow_mut() = Some(rep);
        stack.set_visible_child_name("report");
    } else {
        trigger_scan();
    }

    window.present();
}

fn build_alert_card(title: &str, body: &str) -> gtk4::Box {
    let card = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    card.add_css_class("more-apps-bar");
    let t = gtk4::Label::new(Some(title));
    t.set_xalign(0.0);
    t.add_css_class("row-title");
    let b = gtk4::Label::new(Some(body));
    b.set_xalign(0.0);
    b.set_wrap(true);
    b.add_css_class("dim-label");
    b.add_css_class("secondary");
    card.append(&t);
    card.append(&b);
    card
}

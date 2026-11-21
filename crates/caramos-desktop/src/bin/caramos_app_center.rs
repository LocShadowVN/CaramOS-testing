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
    caramos_user_state_dir, download_github_deb_to_tmp, install_caramos_css, install_flatpak_app,
    invoke_self_pkexec, is_app_installed, is_live_session, remove_flatpak_app,
    run_privileged_apt_install, run_privileged_apt_remove, AppCategory, AppEntry, InstallSource,
    CARAMOS_APP_CATALOG,
};
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::env;
use std::fs;
use std::process::{Command, ExitCode};
use std::rc::Rc;

const VNLF_APPS_URL: &str = "https://apps.vietnamlinuxfamily.net";

enum WorkerMsg {
    Progress { fraction: f64, text: String },
    Finished { ok: bool, summary: String },
}

struct RowHandle {
    app: &'static AppEntry,
    check: gtk4::CheckButton,
    rec_badge: gtk4::Label,
    installed_badge: gtk4::Label,
    remove_btn: gtk4::Button,
}

fn refresh_rows_and_footer(
    rows: &[RowHandle],
    count_label: &gtk4::Label,
    install_btn: &gtk4::Button,
    busy: bool,
) {
    let mut selected_count: usize = 0;
    let mut total_mb: u32 = 0;
    for r in rows {
        let installed = is_app_installed(r.app);
        r.installed_badge.set_visible(installed);
        r.remove_btn.set_visible(installed);
        r.remove_btn.set_sensitive(!busy);
        r.rec_badge.set_visible(!installed && r.app.recommended);

        if installed {
            r.check.set_active(false);
            r.check.set_sensitive(false);
        } else {
            r.check.set_sensitive(!busy);
            if r.check.is_active() {
                selected_count = selected_count.saturating_add(1);
                total_mb = total_mb.saturating_add(r.app.size_mb);
            }
        }
    }

    if selected_count == 0 {
        count_label.set_label("Chưa chọn ứng dụng mới nào");
        install_btn.set_label("Cài đặt đã chọn");
        install_btn.set_sensitive(false);
    } else {
        count_label.set_label(&format!(
            "Đã chọn {} ứng dụng (~{} MB tải về)",
            selected_count, total_mb
        ));
        install_btn.set_label(&format!("Cài {} ứng dụng", selected_count));
        install_btn.set_sensitive(!busy);
    }
}

fn build_ui(app: &adw::Application) {
    install_caramos_css();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        "Trung tâm Ứng dụng CaramOS",
        "Cài đặt những gì bạn cần — giữ nguyên sự gọn gàng cho máy tính",
    )));

    let select_rec_btn = gtk4::Button::with_label("Chọn đề xuất");
    select_rec_btn.add_css_class("flat");
    header.pack_start(&select_rec_btn);

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

    let more_btn = gtk4::Button::with_label("Kho VNLF");
    more_btn.add_css_class("flat");
    more_btn.connect_clicked(|_| {
        let _ = Command::new("xdg-open").arg(VNLF_APPS_URL).spawn();
    });
    header.pack_end(&more_btn);

    let main_box = gtk4::Box::new(gtk4::Orientation::Vertical, 18);

    // Thẻ Hero đầu trang theo phong cách CaramOS
    let hero_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    hero_box.add_css_class("caramos-hero");

    let intro_title = gtk4::Label::builder()
        .label("Chọn ứng dụng quen thuộc cho CaramOS của bạn")
        .xalign(0.0)
        .wrap(true)
        .build();
    intro_title.add_css_class("title-2");

    let intro_desc = gtk4::Label::builder()
        .label("Bộ gõ tiếng Việt Fcitx5 + Lotus đã cài sẵn (chuyển Anh/Việt bằng Ctrl+Shift). Các ứng dụng phổ biến nhất đã được chọn sẵn — bạn chỉ cần bấm Cài đặt và tiếp tục làm việc.")
        .xalign(0.0)
        .wrap(true)
        .build();
    intro_desc.add_css_class("dim-label");

    hero_box.append(&intro_title);
    hero_box.append(&intro_desc);
    main_box.append(&hero_box);

    let progress_bar = gtk4::ProgressBar::builder()
        .show_text(true)
        .hexpand(true)
        .visible(false)
        .build();
    main_box.append(&progress_bar);

    let rows_rc: Rc<RefCell<Vec<RowHandle>>> = Rc::new(RefCell::new(Vec::new()));
    let busy_rc: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));

    let count_label = gtk4::Label::builder()
        .label("")
        .xalign(0.0)
        .hexpand(true)
        .build();
    let install_btn = gtk4::Button::with_label("Cài đặt đã chọn");
    install_btn.add_css_class("suggested-action");
    install_btn.add_css_class("pill");

    let (tx, rx) = async_channel::unbounded::<WorkerMsg>();

    for category in AppCategory::ALL {
        let group = adw::PreferencesGroup::builder()
            .title(category.title())
            .description(category.subtitle())
            .build();

        for entry in CARAMOS_APP_CATALOG.iter().filter(|a| a.category == category) {
            let installed = is_app_installed(entry);
            let subtitle = format!(
                "{}\n{}  •  ~{} MB  •  Nguồn: {}",
                entry.summary,
                entry.pros_cons,
                entry.size_mb,
                entry.source.source_label()
            );

            let row = adw::ActionRow::builder()
                .title(entry.name)
                .subtitle(subtitle)
                .build();
            row.set_use_markup(false);

            let check = gtk4::CheckButton::builder()
                .active(!installed && entry.recommended)
                .sensitive(!installed)
                .valign(gtk4::Align::Center)
                .build();
            row.add_prefix(&check);

            let suffix_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            suffix_box.set_valign(gtk4::Align::Center);

            let rec_badge = gtk4::Label::new(Some("Đề xuất"));
            rec_badge.add_css_class("caramos-pill-rec");
            rec_badge.set_visible(!installed && entry.recommended);

            let installed_badge = gtk4::Label::new(Some("Đã cài"));
            installed_badge.add_css_class("caramos-pill-ok");
            installed_badge.set_visible(installed);

            let remove_btn = gtk4::Button::with_label("Gỡ bỏ");
            remove_btn.add_css_class("destructive-action");
            remove_btn.add_css_class("flat");
            remove_btn.set_visible(installed);

            suffix_box.append(&rec_badge);
            suffix_box.append(&installed_badge);
            suffix_box.append(&remove_btn);
            row.add_suffix(&suffix_box);

            let rows_for_toggle = Rc::clone(&rows_rc);
            let count_for_toggle = count_label.clone();
            let btn_for_toggle = install_btn.clone();
            let busy_for_toggle = Rc::clone(&busy_rc);
            check.connect_toggled(move |_| {
                if let Ok(rows) = rows_for_toggle.try_borrow() {
                    let busy = *busy_for_toggle.borrow();
                    refresh_rows_and_footer(&rows, &count_for_toggle, &btn_for_toggle, busy);
                }
            });

            let tx_rm = tx.clone();
            let busy_rm = Rc::clone(&busy_rc);
            let rows_rm = Rc::clone(&rows_rc);
            let count_rm = count_label.clone();
            let btn_rm = install_btn.clone();
            let prog_rm = progress_bar.clone();
            remove_btn.connect_clicked(move |_| {
                if *busy_rm.borrow() {
                    return;
                }
                *busy_rm.borrow_mut() = true;
                prog_rm.set_visible(true);
                prog_rm.set_fraction(0.2);
                prog_rm.set_text(Some(&format!("Đang gỡ bỏ {}...", entry.name)));
                if let Ok(rows) = rows_rm.try_borrow() {
                    refresh_rows_and_footer(&rows, &count_rm, &btn_rm, true);
                }

                let tx_worker = tx_rm.clone();
                std::thread::spawn(move || {
                    let res = match entry.source {
                        InstallSource::Apt { packages, .. } => {
                            let pkgs: Vec<String> =
                                packages.iter().map(|s| (*s).to_string()).collect();
                            invoke_self_pkexec("--privileged-apt-remove", &pkgs)
                        }
                        InstallSource::GithubDeb { remove_pkg, .. } => {
                            invoke_self_pkexec("--privileged-apt-remove", &[remove_pkg.to_string()])
                        }
                        InstallSource::Flatpak { app_id } => remove_flatpak_app(app_id),
                    };

                    let msg = match res {
                        Ok(()) => WorkerMsg::Finished {
                            ok: true,
                            summary: format!("Đã gỡ bỏ {} thành công.", entry.name),
                        },
                        Err(err) => WorkerMsg::Finished {
                            ok: false,
                            summary: format!("Gỡ {} thất bại: {}", entry.name, err),
                        },
                    };
                    let _ = tx_worker.send_blocking(msg);
                });
            });

            group.add(&row);
            rows_rc.borrow_mut().push(RowHandle {
                app: entry,
                check,
                rec_badge,
                installed_badge,
                remove_btn,
            });
        }

        main_box.append(&group);
    }

    // Nút chọn nhanh gói đề xuất
    {
        let rows_rec = Rc::clone(&rows_rc);
        let busy_rec = Rc::clone(&busy_rc);
        let count_rec = count_label.clone();
        let btn_rec = install_btn.clone();
        select_rec_btn.connect_clicked(move |_| {
            if *busy_rec.borrow() {
                return;
            }
            if let Ok(rows) = rows_rec.try_borrow() {
                for r in rows.iter() {
                    if r.check.is_sensitive() {
                        r.check.set_active(r.app.recommended);
                    }
                }
                refresh_rows_and_footer(&rows, &count_rec, &btn_rec, false);
            }
        });
    }

    // Nút Cài đặt các ứng dụng đã chọn
    {
        let rows_inst = Rc::clone(&rows_rc);
        let busy_inst = Rc::clone(&busy_rc);
        let count_inst = count_label.clone();
        let btn_inst = install_btn.clone();
        let prog_inst = progress_bar.clone();
        let tx_inst = tx.clone();

        install_btn.connect_clicked(move |_| {
            if *busy_inst.borrow() {
                return;
            }
            let selected: Vec<&'static AppEntry> = rows_inst
                .borrow()
                .iter()
                .filter(|r| r.check.is_sensitive() && r.check.is_active())
                .map(|r| r.app)
                .collect();

            if selected.is_empty() {
                return;
            }

            *busy_inst.borrow_mut() = true;
            prog_inst.set_visible(true);
            prog_inst.set_fraction(0.05);
            prog_inst.set_text(Some("Đang chuẩn bị cài đặt các ứng dụng đã chọn..."));
            if let Ok(rows) = rows_inst.try_borrow() {
                refresh_rows_and_footer(&rows, &count_inst, &btn_inst, true);
            }

            let tx_worker = tx_inst.clone();
            std::thread::spawn(move || {
                let mut apt_targets: Vec<String> = Vec::new();
                let mut temp_debs: Vec<std::path::PathBuf> = Vec::new();
                let mut flatpak_targets: Vec<(&'static str, &'static str)> = Vec::new();
                let mut errors: Vec<String> = Vec::new();

                for app in &selected {
                    match app.source {
                        InstallSource::Apt { packages, .. } => {
                            for p in packages {
                                apt_targets.push((*p).to_string());
                            }
                        }
                        InstallSource::GithubDeb { repo, .. } => {
                            let _ = tx_worker.send_blocking(WorkerMsg::Progress {
                                fraction: 0.2,
                                text: format!("Đang tải {} từ GitHub ({})...", app.name, repo),
                            });
                            match download_github_deb_to_tmp(repo, app.id) {
                                Ok(path) => {
                                    apt_targets.push(path.to_string_lossy().to_string());
                                    temp_debs.push(path);
                                }
                                Err(e) => errors.push(format!("{}: {}", app.name, e)),
                            }
                        }
                        InstallSource::Flatpak { app_id } => {
                            flatpak_targets.push((app.name, app_id));
                        }
                    }
                }

                if !apt_targets.is_empty() {
                    let _ = tx_worker.send_blocking(WorkerMsg::Progress {
                        fraction: 0.45,
                        text: "Đang cài đặt các gói Debian & .deb (chỉ nhập mật khẩu 1 lần)..."
                            .to_string(),
                    });
                    if let Err(e) = invoke_self_pkexec("--privileged-apt-install", &apt_targets) {
                        errors.push(e);
                    }
                }

                for deb in temp_debs {
                    let _ = fs::remove_file(deb);
                }

                let total_fp = flatpak_targets.len().max(1);
                for (i, (name, app_id)) in flatpak_targets.iter().enumerate() {
                    let frac = 0.55 + (0.40 * (i as f64 / total_fp as f64));
                    let _ = tx_worker.send_blocking(WorkerMsg::Progress {
                        fraction: frac,
                        text: format!("Đang cài đặt {} từ Flathub...", name),
                    });
                    if let Err(e) = install_flatpak_app(app_id) {
                        errors.push(e);
                    }
                }

                let msg = if errors.is_empty() {
                    WorkerMsg::Finished {
                        ok: true,
                        summary: "Hoàn tất! Tất cả ứng dụng đã chọn sẵn sàng trong Menu."
                            .to_string(),
                    }
                } else {
                    WorkerMsg::Finished {
                        ok: false,
                        summary: format!("Hoàn tất kèm lỗi: {}", errors.join(" | ")),
                    }
                };
                let _ = tx_worker.send_blocking(msg);
            });
        });
    }

    {
        let rows_ui = Rc::clone(&rows_rc);
        let busy_ui = Rc::clone(&busy_rc);
        let count_ui = count_label.clone();
        let btn_ui = install_btn.clone();
        let prog_ui = progress_bar.clone();

        gtk4::glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    WorkerMsg::Progress { fraction, text } => {
                        prog_ui.set_visible(true);
                        prog_ui.set_fraction(fraction);
                        prog_ui.set_text(Some(&text));
                    }
                    WorkerMsg::Finished { ok, summary } => {
                        *busy_ui.borrow_mut() = false;
                        prog_ui.set_fraction(if ok { 1.0 } else { 0.0 });
                        prog_ui.set_text(Some(&summary));
                        if let Ok(rows) = rows_ui.try_borrow() {
                            refresh_rows_and_footer(&rows, &count_ui, &btn_ui, false);
                        }
                    }
                }
            }
        });
    }

    if let Ok(rows) = rows_rc.try_borrow() {
        refresh_rows_and_footer(&rows, &count_label, &install_btn, false);
    }

    let clamp = adw::Clamp::builder()
        .maximum_size(860)
        .margin_top(16)
        .margin_bottom(24)
        .margin_start(16)
        .margin_end(16)
        .child(&main_box)
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
    bottom_box.append(&count_label);
    bottom_box.append(&install_btn);

    let bottom_container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    bottom_container.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    bottom_container.append(&bottom_box);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scrolled));
    toolbar.add_bottom_bar(&bottom_container);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Trung tâm Ứng dụng CaramOS")
        .default_width(920)
        .default_height(720)
        .content(&toolbar)
        .build();
    window.present();
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    if let Some(mode) = args.get(1).map(String::as_str) {
        if mode == "--privileged-apt-install" {
            let targets: Vec<String> = args.iter().skip(2).cloned().collect();
            return match run_privileged_apt_install(&targets) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("✖ {}", e);
                    ExitCode::from(1)
                }
            };
        }
        if mode == "--privileged-apt-remove" {
            let pkgs: Vec<String> = args.iter().skip(2).cloned().collect();
            return match run_privileged_apt_remove(&pkgs) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("✖ {}", e);
                    ExitCode::from(1)
                }
            };
        }
        if mode == "--first-boot" {
            if is_live_session() {
                return ExitCode::SUCCESS;
            }
            let state_dir = caramos_user_state_dir();
            let marker = state_dir.join("app-center-shown");
            if marker.exists() {
                return ExitCode::SUCCESS;
            }
            let _ = fs::create_dir_all(&state_dir);
            let _ = fs::write(&marker, b"1\n");
        }
    }

    let app = adw::Application::builder()
        .application_id("vn.caramos.AppCenter")
        .build();
    app.connect_activate(build_ui);

    let exit_code = app.run_with_args::<&str>(&[]);
    if exit_code == gtk4::glib::ExitCode::SUCCESS {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

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
    CARAMOS_APP_CATALOG, VNLF_APPS_URL, AppCategory, AppEntry, CaramDesktopError, FlathubAppMeta,
    InstallSource, StepSelectionMode, detect_installed_catalog_ids, download_direct_deb_to_tmp,
    download_github_deb_to_tmp, fetch_flathub_app_meta, find_catalog_app_by_flathub_id,
    format_size_mb, install_caramos_css, install_flatpak_app, invoke_self_pkexec,
    is_flatpak_installed, open_external_url, parse_install_link, remove_flatpak_app,
    run_privileged_apt_install, run_privileged_apt_remove,
};
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use libadwaita::prelude::{AdwApplicationWindowExt, AdwWindowExt};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::thread;

const APP_ID: &str = "org.caramos.AppCenter";

type WorkerJob = Box<dyn FnOnce(async_channel::Sender<WorkerEvent>) + Send>;
type WorkerLauncher = Rc<dyn Fn(&str, WorkerJob)>;

#[derive(Debug)]
enum WorkerEvent {
    Progress {
        fraction: f64,
        headline: String,
        log_line: String,
    },
    Finished {
        succeeded: Vec<String>,
        failed: Vec<(String, String)>,
        installed_after: HashSet<String>,
    },
}

#[derive(Debug)]
enum LinkLookupResult {
    Ready {
        app_id: String,
        title: String,
        summary: String,
        developer: String,
        licence: String,
        homepage: Option<String>,
        already_installed: bool,
    },
    Error {
        message: String,
    },
}

struct AppState {
    installed_ids: HashSet<String>,
    selected_ids: BTreeSet<String>,
    active_category_idx: usize,
    search_query: String,
    is_busy: bool,
    hold_guard: Option<gio::ApplicationHoldGuard>,
}

impl AppState {
    fn new() -> Self {
        Self {
            installed_ids: HashSet::new(),
            selected_ids: BTreeSet::new(),
            active_category_idx: 0,
            search_query: String::new(),
            is_busy: false,
            hold_guard: None,
        }
    }

    fn apply_initial_scan(&mut self, installed: HashSet<String>) {
        self.installed_ids = installed;
        self.selected_ids.clear();
        for entry in CARAMOS_APP_CATALOG {
            if entry.default_selected
                && !entry.is_preinstalled()
                && !self.installed_ids.contains(entry.id)
            {
                self.selected_ids.insert(entry.id.to_string());
            }
        }
    }

    fn selected_entries(&self) -> Vec<&'static AppEntry> {
        CARAMOS_APP_CATALOG
            .iter()
            .filter(|e| {
                self.selected_ids.contains(e.id)
                    && !self.installed_ids.contains(e.id)
                    && !e.is_preinstalled()
            })
            .collect()
    }
}

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
    if args.get(1).map(String::as_str) == Some("--pkexec-remove") {
        let pkgs: Vec<String> = args.into_iter().skip(2).collect();
        return match run_privileged_apt_remove(&pkgs) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::FAILURE
            }
        };
    }

    if args.iter().any(|a| a == "--first-boot") {
        if let Some(home) = env::var_os("HOME") {
            let marker = PathBuf::from(home).join(".config/caramos/app-center-first-boot-done");
            if marker.exists() {
                return ExitCode::SUCCESS;
            }
            if let Some(parent) = marker.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&marker, b"1\n");
        }
    }

    let gtk_args: Vec<String> = args.into_iter().filter(|a| a != "--first-boot").collect();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    let (link_tx, link_rx) = async_channel::unbounded::<String>();
    let link_rx_slot = Rc::new(RefCell::new(Some(link_rx)));
    let window_built = Rc::new(Cell::new(false));

    {
        let link_tx_open = link_tx.clone();
        app.connect_open(move |application, files, _hint| {
            for file in files {
                let _ = link_tx_open.send_blocking(file.uri().to_string());
            }
            application.activate();
        });
    }

    app.connect_activate(move |application| {
        if window_built.get() {
            if let Some(win) = application.active_window() {
                win.present();
            }
            return;
        }
        window_built.set(true);
        install_caramos_css();
        if let Some(rx) = link_rx_slot.borrow_mut().take() {
            build_main_window(application, rx);
        }
    });

    if app.run_with_args(&gtk_args) == glib::ExitCode::SUCCESS {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn build_main_window(app: &adw::Application, link_rx: async_channel::Receiver<String>) {
    let state = Rc::new(RefCell::new(AppState::new()));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Ứng dụng")
        .default_width(720)
        .default_height(660)
        .build();

    let toast_overlay = adw::ToastOverlay::new();
    let root_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Ứng dụng", "Chọn phần mềm quen thuộc cho CaramOS")));
    root_vbox.append(&header);

    let main_stack = gtk4::Stack::new();
    main_stack.set_vexpand(true);
    main_stack.set_transition_type(gtk4::StackTransitionType::Crossfade);

    // Trang 1: Đang quét
    let loading_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    loading_box.set_valign(gtk4::Align::Center);
    loading_box.set_halign(gtk4::Align::Center);
    let loading_spinner = gtk4::Spinner::new();
    loading_spinner.set_size_request(36, 36);
    loading_spinner.start();
    let loading_label = gtk4::Label::new(Some("Đang kiểm tra các ứng dụng đã cài..."));
    loading_label.add_css_class("dim-label");
    loading_box.append(&loading_spinner);
    loading_box.append(&loading_label);
    main_stack.add_named(&loading_box, Some("loading"));

    // Trang 2: Ngoại tuyến
    let offline_page = adw::StatusPage::builder()
        .icon_name("network-offline-symbolic")
        .title("Không có kết nối mạng")
        .description("Hãy kết nối Wi-Fi hoặc mạng dây để tải và cài ứng dụng.")
        .build();
    let retry_net_btn = gtk4::Button::with_label("Thử lại");
    retry_net_btn.add_css_class("pill");
    retry_net_btn.add_css_class("suggested-action");
    retry_net_btn.set_halign(gtk4::Align::Center);
    offline_page.set_child(Some(&retry_net_btn));
    main_stack.add_named(&offline_page, Some("offline"));

    // Trang 3: Duyệt 6 nhóm ứng dụng
    let browse_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let browse_scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .build();
    let browse_clamp = adw::Clamp::builder()
        .maximum_size(660)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(24)
        .build();
    let browse_inner = gtk4::Box::new(gtk4::Orientation::Vertical, 14);

    let search_entry = gtk4::SearchEntry::new();
    search_entry.set_placeholder_text(Some("Tìm ứng dụng theo tên hoặc công dụng..."));
    browse_inner.append(&search_entry);

    let cat_flow = gtk4::FlowBox::new();
    cat_flow.set_selection_mode(gtk4::SelectionMode::None);
    cat_flow.set_row_spacing(6);
    cat_flow.set_column_spacing(6);
    browse_inner.append(&cat_flow);

    let page_title_lbl = gtk4::Label::new(None);
    page_title_lbl.set_xalign(0.0);
    page_title_lbl.add_css_class("page-title");
    browse_inner.append(&page_title_lbl);

    let page_sub_lbl = gtk4::Label::new(None);
    page_sub_lbl.set_xalign(0.0);
    page_sub_lbl.set_wrap(true);
    page_sub_lbl.add_css_class("dim-label");
    browse_inner.append(&page_sub_lbl);

    let app_listbox = gtk4::ListBox::new();
    app_listbox.set_selection_mode(gtk4::SelectionMode::None);
    app_listbox.add_css_class("boxed-list");
    browse_inner.append(&app_listbox);

    let more_btn = gtk4::Button::new();
    more_btn.add_css_class("flat");
    more_btn.add_css_class("more-apps-bar");
    let more_hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    let more_lbl = gtk4::Label::new(Some("Tìm thêm ứng dụng khác trên apps.vietnamlinuxfamily.net"));
    more_lbl.set_xalign(0.0);
    more_lbl.set_hexpand(true);
    more_lbl.set_wrap(true);
    more_hbox.append(&more_lbl);
    more_hbox.append(&gtk4::Image::from_icon_name("adw-external-link-symbolic"));
    more_btn.set_child(Some(&more_hbox));
    more_btn.connect_clicked(|_| open_external_url(VNLF_APPS_URL));
    browse_inner.append(&more_btn);

    browse_clamp.set_child(Some(&browse_inner));
    browse_scroll.set_child(Some(&browse_clamp));
    browse_vbox.append(&browse_scroll);

    let footer_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    footer_bar.add_css_class("wizard-bar");
    let prev_btn = gtk4::Button::with_label("Quay lại");
    let summary_lbl = gtk4::Label::new(Some("Chưa chọn ứng dụng nào"));
    summary_lbl.set_hexpand(true);
    summary_lbl.set_xalign(0.0);
    summary_lbl.add_css_class("secondary");
    let next_btn = gtk4::Button::with_label("Nhóm tiếp");
    let install_btn = gtk4::Button::with_label("Cài đặt ngay");
    install_btn.add_css_class("suggested-action");

    footer_bar.append(&prev_btn);
    footer_bar.append(&summary_lbl);
    footer_bar.append(&next_btn);
    footer_bar.append(&install_btn);
    browse_vbox.append(&footer_bar);
    main_stack.add_named(&browse_vbox, Some("browse"));

    // Trang 4: Tiến trình
    let prog_clamp = adw::Clamp::builder()
        .maximum_size(640)
        .margin_start(16)
        .margin_end(16)
        .margin_top(28)
        .margin_bottom(24)
        .build();
    let prog_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    let prog_title = gtk4::Label::new(Some("Đang thực hiện cài đặt"));
    prog_title.set_xalign(0.0);
    prog_title.add_css_class("page-title");
    let prog_status = gtk4::Label::new(Some("Đang chuẩn bị..."));
    prog_status.set_xalign(0.0);
    prog_status.set_wrap(true);
    let prog_bar = gtk4::ProgressBar::new();
    prog_bar.set_show_text(true);
    let bg_hint = gtk4::Label::new(Some(
        "Bạn có thể đóng cửa sổ này; cài đặt sẽ tiếp tục chạy nền và gửi thông báo khi xong.",
    ));
    bg_hint.set_xalign(0.0);
    bg_hint.set_wrap(true);
    bg_hint.add_css_class("dim-label");
    bg_hint.add_css_class("secondary");

    let log_buffer = gtk4::TextBuffer::new(None);
    let log_view = gtk4::TextView::with_buffer(&log_buffer);
    log_view.set_editable(false);
    log_view.set_monospace(true);
    log_view.set_wrap_mode(gtk4::WrapMode::WordChar);
    let log_scroll = gtk4::ScrolledWindow::builder().min_content_height(180).child(&log_view).build();
    let log_expander = gtk4::Expander::new(Some("Xem chi tiết nhật ký"));
    log_expander.set_child(Some(&log_scroll));

    prog_vbox.append(&prog_title);
    prog_vbox.append(&prog_status);
    prog_vbox.append(&prog_bar);
    prog_vbox.append(&bg_hint);
    prog_vbox.append(&log_expander);
    prog_clamp.set_child(Some(&prog_vbox));
    main_stack.add_named(&prog_clamp, Some("progress"));

    // Trang 5: Hoàn tất
    let done_page = adw::StatusPage::builder().icon_name("emblem-ok-symbolic").title("Hoàn tất").build();
    let done_actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    done_actions.set_halign(gtk4::Align::Center);
    let back_to_list_btn = gtk4::Button::with_label("Quay lại danh sách");
    let close_done_btn = gtk4::Button::with_label("Đóng");
    close_done_btn.add_css_class("suggested-action");
    done_actions.append(&back_to_list_btn);
    done_actions.append(&close_done_btn);
    done_page.set_child(Some(&done_actions));
    main_stack.add_named(&done_page, Some("done"));

    // Trang 6: Liên kết sâu `caramos-app://install/<Flathub-id>`
    let link_clamp = adw::Clamp::builder()
        .maximum_size(600)
        .margin_start(16)
        .margin_end(16)
        .margin_top(28)
        .margin_bottom(24)
        .build();
    let link_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    let link_title = gtk4::Label::new(Some("Đang tra cứu ứng dụng trên Flathub..."));
    link_title.set_xalign(0.0);
    link_title.add_css_class("page-title");
    let link_desc = gtk4::Label::new(None);
    link_desc.set_xalign(0.0);
    link_desc.set_wrap(true);
    let link_meta = gtk4::Label::new(None);
    link_meta.set_xalign(0.0);
    link_meta.set_wrap(true);
    link_meta.add_css_class("dim-label");
    link_meta.add_css_class("secondary");
    let link_btn_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    let link_back_btn = gtk4::Button::with_label("Quay lại danh sách");
    let link_install_btn = gtk4::Button::with_label("Cài đặt từ Flathub");
    link_install_btn.add_css_class("suggested-action");
    link_install_btn.set_sensitive(false);
    link_btn_box.append(&link_back_btn);
    link_btn_box.append(&link_install_btn);
    link_vbox.append(&link_title);
    link_vbox.append(&link_desc);
    link_vbox.append(&link_meta);
    link_vbox.append(&link_btn_box);
    link_clamp.set_child(Some(&link_vbox));
    main_stack.add_named(&link_clamp, Some("link-confirm"));

    root_vbox.append(&main_stack);
    toast_overlay.set_child(Some(&root_vbox));
    window.set_content(Some(&toast_overlay));

    let cat_buttons: Rc<Vec<gtk4::ToggleButton>> = Rc::new(
        AppCategory::ALL
            .iter()
            .map(|cat| {
                let btn = gtk4::ToggleButton::with_label(cat.title());
                cat_flow.insert(&btn, -1);
                btn
            })
            .collect(),
    );

    let pending_link_flathub_id: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let start_worker: WorkerLauncher = Rc::new({
        let app = app.clone();
        let state = Rc::clone(&state);
        let main_stack = main_stack.clone();
        let prog_title = prog_title.clone();
        let prog_status = prog_status.clone();
        let prog_bar = prog_bar.clone();
        let log_buffer = log_buffer.clone();
        let done_page = done_page.clone();
        let window = window.clone();
        move |headline: &str, job: WorkerJob| {
            {
                let mut st = state.borrow_mut();
                if st.is_busy {
                    return;
                }
                st.is_busy = true;
                st.hold_guard = Some(app.hold());
            }
            prog_title.set_text(headline);
            prog_status.set_text("Đang chuẩn bị...");
            prog_bar.set_fraction(0.0);
            prog_bar.set_text(Some("0%"));
            log_buffer.set_text("");
            main_stack.set_visible_child_name("progress");

            let (tx, rx) = async_channel::unbounded::<WorkerEvent>();
            thread::spawn(move || job(tx));

            let app_done = app.clone();
            let state_done = Rc::clone(&state);
            let main_stack_done = main_stack.clone();
            let prog_status_done = prog_status.clone();
            let prog_bar_done = prog_bar.clone();
            let log_buffer_done = log_buffer.clone();
            let done_page_done = done_page.clone();
            let window_done = window.clone();

            glib::spawn_future_local(async move {
                while let Ok(ev) = rx.recv().await {
                    match ev {
                        WorkerEvent::Progress { fraction, headline, log_line } => {
                            prog_status_done.set_text(&headline);
                            let clamped = fraction.clamp(0.0, 1.0);
                            prog_bar_done.set_fraction(clamped);
                            prog_bar_done.set_text(Some(&format!("{}%", (clamped * 100.0).round() as u32)));
                            let mut end = log_buffer_done.end_iter();
                            log_buffer_done.insert(&mut end, &format!("{}\n", log_line));
                        }
                        WorkerEvent::Finished { succeeded, failed, installed_after } => {
                            {
                                let st = &mut *state_done.borrow_mut();
                                st.is_busy = false;
                                st.hold_guard = None;
                                st.installed_ids = installed_after;
                                let installed_ref = &st.installed_ids;
                                st.selected_ids.retain(|id| !installed_ref.contains(id));
                            }

                            let (icon, title, desc) = if failed.is_empty() {
                                (
                                    "emblem-ok-symbolic",
                                    "Hoàn tất thành công".to_string(),
                                    format!("Đã xử lý: {}", succeeded.join(", ")),
                                )
                            } else {
                                let err_lines: Vec<String> =
                                    failed.iter().map(|(n, e)| format!("• {}: {}", n, e)).collect();
                                (
                                    "dialog-warning-symbolic",
                                    "Hoàn tất với một số lỗi".to_string(),
                                    err_lines.join("\n"),
                                )
                            };

                            done_page_done.set_icon_name(Some(icon));
                            done_page_done.set_title(&title);
                            done_page_done.set_description(Some(&desc));
                            main_stack_done.set_visible_child_name("done");

                            if !window_done.is_visible() {
                                let notif = gio::Notification::new(&title);
                                notif.set_body(Some(&desc));
                                app_done.send_notification(Some("caramos-app-center-done"), &notif);
                            }
                        }
                    }
                }
            });
        }
    });

    let refresh_footer = Rc::new({
        let state = Rc::clone(&state);
        let summary_lbl = summary_lbl.clone();
        let install_btn = install_btn.clone();
        let prev_btn = prev_btn.clone();
        let next_btn = next_btn.clone();
        move || {
            let st = state.borrow();
            let selected = st.selected_entries();
            let count = selected.len();
            let total_mb: u32 = selected.iter().map(|e| e.size_mb).sum();
            if count == 0 {
                summary_lbl.set_text("Chưa chọn ứng dụng nào để cài thêm");
                install_btn.set_sensitive(false);
            } else {
                summary_lbl.set_text(&format!("Đã chọn {} ứng dụng ({})", count, format_size_mb(total_mb)));
                install_btn.set_sensitive(true);
            }
            prev_btn.set_sensitive(st.active_category_idx > 0 && st.search_query.is_empty());
            next_btn.set_sensitive(st.active_category_idx + 1 < AppCategory::ALL.len() && st.search_query.is_empty());
        }
    });

    let rebuild_rows = Rc::new({
        let state = Rc::clone(&state);
        let app_listbox = app_listbox.clone();
        let page_title_lbl = page_title_lbl.clone();
        let page_sub_lbl = page_sub_lbl.clone();
        let cat_buttons = Rc::clone(&cat_buttons);
        let refresh_footer = Rc::clone(&refresh_footer);
        let window = window.clone();
        let start_worker = Rc::clone(&start_worker);
        move || {
            while let Some(child) = app_listbox.first_child() {
                app_listbox.remove(&child);
            }

            let (cat_idx, query, installed_snap, selected_snap) = {
                let st = state.borrow();
                (
                    st.active_category_idx,
                    st.search_query.trim().to_ascii_lowercase(),
                    st.installed_ids.clone(),
                    st.selected_ids.clone(),
                )
            };

            for (idx, btn) in cat_buttons.iter().enumerate() {
                btn.set_active(query.is_empty() && idx == cat_idx);
            }

            let active_cat = AppCategory::ALL.get(cat_idx).copied().unwrap_or(AppCategory::Office);
            let filtered: Vec<&'static AppEntry> = if query.is_empty() {
                page_title_lbl.set_text(active_cat.title());
                page_sub_lbl.set_text(active_cat.subtitle());
                CARAMOS_APP_CATALOG.iter().filter(|e| e.category == active_cat).collect()
            } else {
                page_title_lbl.set_text("Kết quả tìm kiếm");
                page_sub_lbl.set_text(&format!("Đang lọc theo từ khóa \"{}\"", query));
                CARAMOS_APP_CATALOG
                    .iter()
                    .filter(|e| {
                        e.name.to_ascii_lowercase().contains(&query)
                            || e.summary.to_ascii_lowercase().contains(&query)
                    })
                    .collect()
            };

            let use_radio = query.is_empty() && active_cat.selection_mode() == StepSelectionMode::SingleOptional;
            let mut radio_anchor: Option<gtk4::CheckButton> = None;

            for entry in filtered {
                let is_installed = installed_snap.contains(entry.id) || entry.is_preinstalled();
                let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
                row_box.set_valign(gtk4::Align::Center);

                if !is_installed {
                    let check = gtk4::CheckButton::new();
                    if use_radio {
                        if let Some(ref anchor) = radio_anchor {
                            check.set_group(Some(anchor));
                        } else {
                            radio_anchor = Some(check.clone());
                        }
                    }
                    check.set_active(selected_snap.contains(entry.id));

                    let state_c = Rc::clone(&state);
                    let refresh_c = Rc::clone(&refresh_footer);
                    let id_str = entry.id.to_string();
                    check.connect_toggled(move |btn| {
                        {
                            let mut st = state_c.borrow_mut();
                            if btn.is_active() {
                                if use_radio {
                                    for other in CARAMOS_APP_CATALOG.iter().filter(|o| o.category == active_cat) {
                                        st.selected_ids.remove(other.id);
                                    }
                                }
                                st.selected_ids.insert(id_str.clone());
                            } else {
                                st.selected_ids.remove(&id_str);
                            }
                        }
                        refresh_c();
                    });
                    row_box.append(&check);
                }

                let icon = gtk4::Image::from_icon_name(entry.icon_name);
                icon.set_pixel_size(40);
                row_box.append(&icon);

                let text_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 3);
                text_vbox.set_hexpand(true);
                let title_suffix = if entry.is_preinstalled() {
                    " — Có sẵn"
                } else if is_installed {
                    " — Đã cài"
                } else if entry.recommended {
                    " — Khuyên dùng"
                } else {
                    ""
                };
                let title_lbl = gtk4::Label::new(Some(&format!("{}{}", entry.name, title_suffix)));
                title_lbl.set_xalign(0.0);
                title_lbl.add_css_class("row-title");
                let sub_lbl = gtk4::Label::new(Some(entry.summary));
                sub_lbl.set_xalign(0.0);
                sub_lbl.set_wrap(true);
                sub_lbl.add_css_class("dim-label");
                sub_lbl.add_css_class("secondary");
                text_vbox.append(&title_lbl);
                text_vbox.append(&sub_lbl);
                row_box.append(&text_vbox);

                if is_installed && !entry.is_preinstalled() {
                    let remove_btn = gtk4::Button::with_label("Gỡ");
                    remove_btn.add_css_class("flat");
                    remove_btn.add_css_class("destructive-action");
                    let win_u = window.clone();
                    let worker_u = Rc::clone(&start_worker);
                    remove_btn.connect_clicked(move |_| {
                        show_confirm_uninstall_dialog(&win_u, entry, Rc::clone(&worker_u));
                    });
                    row_box.append(&remove_btn);
                }

                let info_btn = gtk4::Button::from_icon_name("help-about-symbolic");
                info_btn.add_css_class("flat");
                info_btn.add_css_class("circular");
                info_btn.add_css_class("row-info");
                info_btn.set_tooltip_text(Some("Xem ưu, nhược điểm và giấy phép"));
                let win_i = window.clone();
                info_btn.connect_clicked(move |_| show_app_info_dialog(&win_i, entry));
                row_box.append(&info_btn);

                let row = gtk4::ListBoxRow::new();
                row.add_css_class("app-row");
                row.set_child(Some(&row_box));
                app_listbox.append(&row);
            }

            if use_radio {
                let any_office_selected = CARAMOS_APP_CATALOG
                    .iter()
                    .any(|e| e.category == active_cat && selected_snap.contains(e.id));
                let none_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
                let none_radio = gtk4::CheckButton::new();
                if let Some(ref anchor) = radio_anchor {
                    none_radio.set_group(Some(anchor));
                }
                none_radio.set_active(!any_office_selected);
                let state_n = Rc::clone(&state);
                let refresh_n = Rc::clone(&refresh_footer);
                none_radio.connect_toggled(move |btn| {
                    if btn.is_active() {
                        {
                            let mut st = state_n.borrow_mut();
                            for other in CARAMOS_APP_CATALOG.iter().filter(|o| o.category == active_cat) {
                                st.selected_ids.remove(other.id);
                            }
                        }
                        refresh_n();
                    }
                });
                let none_lbl = gtk4::Label::new(Some("Không cài lúc này"));
                none_lbl.add_css_class("dim-label");
                none_box.append(&none_radio);
                none_box.append(&none_lbl);

                let none_row = gtk4::ListBoxRow::new();
                none_row.add_css_class("text-row");
                none_row.set_child(Some(&none_box));
                app_listbox.append(&none_row);
            }

            refresh_footer();
        }
    });

    for (idx, btn) in cat_buttons.iter().enumerate() {
        let state_c = Rc::clone(&state);
        let rebuild_c = Rc::clone(&rebuild_rows);
        let search_c = search_entry.clone();
        btn.connect_clicked(move |_| {
            if !search_c.text().is_empty() {
                search_c.set_text("");
            }
            {
                let mut st = state_c.borrow_mut();
                if st.active_category_idx == idx && st.search_query.is_empty() {
                    return;
                }
                st.active_category_idx = idx;
                st.search_query.clear();
            }
            rebuild_c();
        });
    }

    {
        let state_s = Rc::clone(&state);
        let rebuild_s = Rc::clone(&rebuild_rows);
        search_entry.connect_search_changed(move |entry| {
            state_s.borrow_mut().search_query = entry.text().to_string();
            rebuild_s();
        });
    }
    {
        let state_p = Rc::clone(&state);
        let rebuild_p = Rc::clone(&rebuild_rows);
        prev_btn.connect_clicked(move |_| {
            {
                let mut st = state_p.borrow_mut();
                st.active_category_idx = st.active_category_idx.saturating_sub(1);
            }
            rebuild_p();
        });
    }
    {
        let state_n = Rc::clone(&state);
        let rebuild_n = Rc::clone(&rebuild_rows);
        next_btn.connect_clicked(move |_| {
            {
                let mut st = state_n.borrow_mut();
                if st.active_category_idx + 1 < AppCategory::ALL.len() {
                    st.active_category_idx += 1;
                }
            }
            rebuild_n();
        });
    }
    {
        let state_i = Rc::clone(&state);
        let worker_i = Rc::clone(&start_worker);
        install_btn.connect_clicked(move |_| {
            let entries = state_i.borrow().selected_entries();
            if entries.is_empty() {
                return;
            }
            worker_i(
                "Đang cài đặt các ứng dụng đã chọn",
                Box::new(move |tx| execute_batch_install(entries, tx)),
            );
        });
    }
    {
        let main_stack_b = main_stack.clone();
        let rebuild_b = Rc::clone(&rebuild_rows);
        back_to_list_btn.connect_clicked(move |_| {
            rebuild_b();
            main_stack_b.set_visible_child_name("browse");
        });
    }
    {
        let win_c = window.clone();
        close_done_btn.connect_clicked(move |_| win_c.close());
    }
    {
        let main_stack_lb = main_stack.clone();
        let rebuild_lb = Rc::clone(&rebuild_rows);
        link_back_btn.connect_clicked(move |_| {
            rebuild_lb();
            main_stack_lb.set_visible_child_name("browse");
        });
    }
    {
        let pending_id = Rc::clone(&pending_link_flathub_id);
        let worker_l = Rc::clone(&start_worker);
        link_install_btn.connect_clicked(move |_| {
            let Some(app_id) = pending_id.borrow().clone() else {
                return;
            };
            worker_l(
                "Đang cài đặt ứng dụng từ Flathub",
                Box::new(move |tx| {
                    let _ = tx.send_blocking(WorkerEvent::Progress {
                        fraction: 0.25,
                        headline: format!("Đang tải và cài {} từ Flathub...", app_id),
                        log_line: format!("flatpak install --user flathub {}", app_id),
                    });
                    let mut ok = Vec::new();
                    let mut err = Vec::new();
                    match install_flatpak_app(&app_id) {
                        Ok(()) => ok.push(app_id),
                        Err(e) => err.push((app_id, e.to_string())),
                    }
                    let installed_after = detect_installed_catalog_ids();
                    let _ = tx.send_blocking(WorkerEvent::Finished {
                        succeeded: ok,
                        failed: err,
                        installed_after,
                    });
                }),
            );
        });
    }
    {
        let state_close = Rc::clone(&state);
        let toast_close = toast_overlay.clone();
        window.connect_close_request(move |win| {
            if state_close.borrow().is_busy {
                win.set_visible(false);
                toast_close.add_toast(adw::Toast::new("Đang tiếp tục cài đặt ở chế độ nền"));
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    let net_mon = gio::NetworkMonitor::default();
    let check_network_and_route = Rc::new({
        let net_mon = net_mon.clone();
        let main_stack = main_stack.clone();
        let state = Rc::clone(&state);
        move || {
            if state.borrow().is_busy {
                return;
            }
            if net_mon.is_network_available() {
                if main_stack.visible_child_name().as_deref() == Some("offline") {
                    main_stack.set_visible_child_name("browse");
                }
            } else {
                main_stack.set_visible_child_name("offline");
            }
        }
    });
    {
        let route = Rc::clone(&check_network_and_route);
        retry_net_btn.connect_clicked(move |_| route());
    }
    {
        let route = Rc::clone(&check_network_and_route);
        net_mon.connect_network_changed(move |_, _| route());
    }

    {
        let (scan_tx, scan_rx) = async_channel::bounded::<HashSet<String>>(1);
        thread::spawn(move || {
            let _ = scan_tx.send_blocking(detect_installed_catalog_ids());
        });
        let state_init = Rc::clone(&state);
        let rebuild_init = Rc::clone(&rebuild_rows);
        let stack_init = main_stack.clone();
        let net_init = net_mon.clone();
        glib::spawn_future_local(async move {
            if let Ok(installed) = scan_rx.recv().await {
                state_init.borrow_mut().apply_initial_scan(installed);
                rebuild_init();
                if stack_init.visible_child_name().as_deref() == Some("loading") {
                    if net_init.is_network_available() {
                        stack_init.set_visible_child_name("browse");
                    } else {
                        stack_init.set_visible_child_name("offline");
                    }
                }
            }
        });
    }

    {
        let stack_l = main_stack.clone();
        let title_l = link_title.clone();
        let desc_l = link_desc.clone();
        let meta_l = link_meta.clone();
        let btn_l = link_install_btn.clone();
        let pending_l = Rc::clone(&pending_link_flathub_id);
        glib::spawn_future_local(async move {
            while let Ok(raw_uri) = link_rx.recv().await {
                let app_id = match parse_install_link(&raw_uri) {
                    Ok(id) => id,
                    Err(e) => {
                        title_l.set_text("Liên kết cài đặt không hợp lệ");
                        desc_l.set_text(&e.to_string());
                        meta_l.set_text("");
                        btn_l.set_sensitive(false);
                        stack_l.set_visible_child_name("link-confirm");
                        continue;
                    }
                };

                title_l.set_text("Đang tra cứu thông tin từ Flathub...");
                desc_l.set_text(&app_id);
                meta_l.set_text("");
                btn_l.set_sensitive(false);
                stack_l.set_visible_child_name("link-confirm");

                let (lu_tx, lu_rx) = async_channel::bounded::<LinkLookupResult>(1);
                let id_bg = app_id.clone();
                thread::spawn(move || {
                    let result = if let Some(entry) = find_catalog_app_by_flathub_id(&id_bg) {
                        LinkLookupResult::Ready {
                            app_id: id_bg.clone(),
                            title: entry.name.to_string(),
                            summary: entry.summary.to_string(),
                            developer: "Danh mục CaramOS".to_string(),
                            licence: entry.licence_note.to_string(),
                            homepage: Some(entry.website.to_string()),
                            already_installed: is_flatpak_installed(&id_bg),
                        }
                    } else {
                        match fetch_flathub_app_meta(&id_bg) {
                            Ok(FlathubAppMeta {
                                id, name, summary, developer_name, project_license, urls,
                            }) => LinkLookupResult::Ready {
                                already_installed: is_flatpak_installed(&id),
                                app_id: id,
                                title: name,
                                summary,
                                developer: developer_name.unwrap_or_else(|| "Cộng đồng Flathub".to_string()),
                                licence: project_license.unwrap_or_else(|| "Theo trang Flathub".to_string()),
                                homepage: urls.get("homepage").cloned(),
                            },
                            Err(e) => LinkLookupResult::Error { message: e.to_string() },
                        }
                    };
                    let _ = lu_tx.send_blocking(result);
                });

                if let Ok(res) = lu_rx.recv().await {
                    match res {
                        LinkLookupResult::Ready {
                            app_id, title, summary, developer, licence, homepage, already_installed,
                        } => {
                            title_l.set_text(&title);
                            desc_l.set_text(&summary);
                            let mut info = format!(
                                "Mã Flathub: {}\nNhà phát triển: {}\nGiấy phép: {}",
                                app_id, developer, licence
                            );
                            if let Some(hp) = homepage {
                                info.push_str(&format!("\nTrang chủ: {}", hp));
                            }
                            meta_l.set_text(&info);
                            *pending_l.borrow_mut() = Some(app_id);
                            if already_installed {
                                btn_l.set_label("Đã được cài trên máy");
                                btn_l.set_sensitive(false);
                            } else {
                                btn_l.set_label("Cài đặt từ Flathub");
                                btn_l.set_sensitive(true);
                            }
                        }
                        LinkLookupResult::Error { message } => {
                            title_l.set_text("Không tải được thông tin ứng dụng");
                            desc_l.set_text(&message);
                            meta_l.set_text("");
                            btn_l.set_sensitive(false);
                        }
                    }
                }
            }
        });
    }

    window.present();
}

fn execute_batch_install(entries: Vec<&'static AppEntry>, tx: async_channel::Sender<WorkerEvent>) {
    let total = entries.len().max(1) as f64;
    let mut succeeded: Vec<String> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut apt_targets: Vec<String> = Vec::new();
    let mut apt_names: Vec<String> = Vec::new();
    let mut temp_debs: Vec<PathBuf> = Vec::new();
    let mut flatpak_entries: Vec<&'static AppEntry> = Vec::new();

    for (idx, entry) in entries.iter().enumerate() {
        let base_frac = (idx as f64 / total) * 0.45;
        match entry.source {
            InstallSource::Preinstalled { .. } => {}
            InstallSource::Apt { packages, .. } => {
                for p in packages {
                    apt_targets.push((*p).to_string());
                }
                apt_names.push(entry.name.to_string());
            }
            InstallSource::GithubDeb { repo, .. } => {
                let _ = tx.send_blocking(WorkerEvent::Progress {
                    fraction: base_frac,
                    headline: format!("Đang tải gói cài đặt {}...", entry.name),
                    log_line: format!("Tải GitHub release: {}", repo),
                });
                match download_github_deb_to_tmp(repo, entry.id) {
                    Ok(path) => {
                        apt_targets.push(path.display().to_string());
                        apt_names.push(entry.name.to_string());
                        temp_debs.push(path);
                    }
                    Err(e) => failed.push((entry.name.to_string(), e.to_string())),
                }
            }
            InstallSource::DirectDeb { url, .. } => {
                let _ = tx.send_blocking(WorkerEvent::Progress {
                    fraction: base_frac,
                    headline: format!("Đang tải gói cài đặt {}...", entry.name),
                    log_line: format!("Tải trực tiếp: {}", url),
                });
                match download_direct_deb_to_tmp(url, entry.id) {
                    Ok(path) => {
                        apt_targets.push(path.display().to_string());
                        apt_names.push(entry.name.to_string());
                        temp_debs.push(path);
                    }
                    Err(e) => failed.push((entry.name.to_string(), e.to_string())),
                }
            }
            InstallSource::Flatpak { .. } => flatpak_entries.push(entry),
        }
    }

    if !apt_targets.is_empty() {
        let _ = tx.send_blocking(WorkerEvent::Progress {
            fraction: 0.5,
            headline: format!("Đang cài {} qua APT (chỉ hỏi mật khẩu 1 lần)...", apt_names.join(", ")),
            log_line: format!("pkexec apt-get install {}", apt_targets.join(" ")),
        });
        match invoke_self_pkexec("--pkexec-install", &apt_targets) {
            Ok(()) => succeeded.extend(apt_names),
            Err(CaramDesktopError::AuthCancelled) => {
                for name in apt_names {
                    failed.push((name, "Đã hủy xác thực quyền quản trị".to_string()));
                }
            }
            Err(e) => {
                for name in apt_names {
                    failed.push((name, e.to_string()));
                }
            }
        }
    }

    for deb in temp_debs {
        let _ = fs::remove_file(deb);
    }

    let fp_total = flatpak_entries.len().max(1) as f64;
    for (idx, entry) in flatpak_entries.iter().enumerate() {
        if let InstallSource::Flatpak { app_id } = entry.source {
            let frac = 0.55 + ((idx as f64) / fp_total) * 0.42;
            let _ = tx.send_blocking(WorkerEvent::Progress {
                fraction: frac,
                headline: format!("Đang cài {} từ Flathub...", entry.name),
                log_line: format!("flatpak install --user flathub {}", app_id),
            });
            match install_flatpak_app(app_id) {
                Ok(()) => succeeded.push(entry.name.to_string()),
                Err(e) => failed.push((entry.name.to_string(), e.to_string())),
            }
        }
    }

    let installed_after = detect_installed_catalog_ids();
    let _ = tx.send_blocking(WorkerEvent::Finished {
        succeeded,
        failed,
        installed_after,
    });
}

fn show_app_info_dialog(parent: &adw::ApplicationWindow, entry: &'static AppEntry) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(entry.name)
        .default_width(480)
        .build();

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    vbox.append(&adw::HeaderBar::new());

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    body.set_margin_start(20);
    body.set_margin_end(20);
    body.set_margin_top(16);
    body.set_margin_bottom(20);

    let sum = gtk4::Label::new(Some(entry.summary));
    sum.set_xalign(0.0);
    sum.set_wrap(true);
    body.append(&sum);

    let pros_hdr = gtk4::Label::new(Some("Ưu điểm"));
    pros_hdr.set_xalign(0.0);
    pros_hdr.add_css_class("section-heading");
    body.append(&pros_hdr);
    for p in entry.pros {
        let lbl = gtk4::Label::new(Some(&format!("• {}", p)));
        lbl.set_xalign(0.0);
        lbl.set_wrap(true);
        lbl.add_css_class("details-text");
        body.append(&lbl);
    }

    if !entry.cons.is_empty() {
        let cons_hdr = gtk4::Label::new(Some("Lưu ý"));
        cons_hdr.set_xalign(0.0);
        cons_hdr.add_css_class("section-heading");
        body.append(&cons_hdr);
        for c in entry.cons {
            let lbl = gtk4::Label::new(Some(&format!("• {}", c)));
            lbl.set_xalign(0.0);
            lbl.set_wrap(true);
            lbl.add_css_class("details-text");
            body.append(&lbl);
        }
    }

    let meta = gtk4::Label::new(Some(&format!(
        "Giấy phép: {}\nDung lượng ước tính: {}",
        entry.licence_note,
        format_size_mb(entry.size_mb)
    )));
    meta.set_xalign(0.0);
    meta.set_wrap(true);
    meta.add_css_class("dim-label");
    meta.add_css_class("secondary");
    body.append(&meta);

    let btn_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    btn_row.set_halign(gtk4::Align::End);
    let web_btn = gtk4::Button::with_label("Trang chủ");
    let url = entry.website;
    web_btn.connect_clicked(move |_| open_external_url(url));
    let close_btn = gtk4::Button::with_label("Đóng");
    close_btn.add_css_class("suggested-action");
    let dlg_close = dialog.clone();
    close_btn.connect_clicked(move |_| dlg_close.close());
    btn_row.append(&web_btn);
    btn_row.append(&close_btn);
    body.append(&btn_row);

    vbox.append(&body);
    dialog.set_content(Some(&vbox));
    dialog.present();
}

fn show_confirm_uninstall_dialog(
    parent: &adw::ApplicationWindow,
    entry: &'static AppEntry,
    start_worker: WorkerLauncher,
) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(&format!("Gỡ {}?", entry.name))
        .default_width(420)
        .build();

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    vbox.append(&adw::HeaderBar::new());

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    body.set_margin_start(20);
    body.set_margin_end(20);
    body.set_margin_top(16);
    body.set_margin_bottom(20);

    let msg = gtk4::Label::new(Some(&format!(
        "Ứng dụng {} sẽ được gỡ khỏi máy. Các tệp dữ liệu cá nhân của bạn không bị xóa.",
        entry.name
    )));
    msg.set_xalign(0.0);
    msg.set_wrap(true);
    body.append(&msg);

    let btn_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    btn_row.set_halign(gtk4::Align::End);
    let cancel_btn = gtk4::Button::with_label("Hủy");
    let dlg_c = dialog.clone();
    cancel_btn.connect_clicked(move |_| dlg_c.close());

    let confirm_btn = gtk4::Button::with_label("Gỡ cài đặt");
    confirm_btn.add_css_class("destructive-action");
    let dlg_ok = dialog.clone();
    confirm_btn.connect_clicked(move |_| {
        dlg_ok.close();
        start_worker(
            &format!("Đang gỡ {}", entry.name),
            Box::new(move |tx| {
                let _ = tx.send_blocking(WorkerEvent::Progress {
                    fraction: 0.35,
                    headline: format!("Đang gỡ cài đặt {}...", entry.name),
                    log_line: format!("Gỡ ứng dụng {}", entry.id),
                });
                let result = match entry.source {
                    InstallSource::Preinstalled { .. } => Ok(()),
                    InstallSource::Flatpak { app_id } => remove_flatpak_app(app_id),
                    InstallSource::Apt { packages, .. } => {
                        let pkgs: Vec<String> = packages.iter().map(|s| (*s).to_string()).collect();
                        invoke_self_pkexec("--pkexec-remove", &pkgs)
                    }
                    InstallSource::GithubDeb { remove_pkgs, .. }
                    | InstallSource::DirectDeb { remove_pkgs, .. } => {
                        let pkgs: Vec<String> = remove_pkgs.iter().map(|s| (*s).to_string()).collect();
                        invoke_self_pkexec("--pkexec-remove", &pkgs)
                    }
                };
                let (ok, err) = match result {
                    Ok(()) => (vec![format!("Đã gỡ {}", entry.name)], Vec::new()),
                    Err(e) => (Vec::new(), vec![(entry.name.to_string(), e.to_string())]),
                };
                let installed_after = detect_installed_catalog_ids();
                let _ = tx.send_blocking(WorkerEvent::Finished {
                    succeeded: ok,
                    failed: err,
                    installed_after,
                });
            }),
        );
    });

    btn_row.append(&cancel_btn);
    btn_row.append(&confirm_btn);
    body.append(&btn_row);
    vbox.append(&body);
    dialog.set_content(Some(&vbox));
    dialog.present();
}

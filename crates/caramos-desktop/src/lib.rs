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

//! Backend Rust thuần và hệ thống nhận diện `DESIGN.md` (xanh rừng `#2b6a44` / `#3a855a`,
//! phông Be Vietnam Pro 2 độ đậm 400 & 600, danh sách `boxed-list`) cho `caramos-app-center`
//! và `caramos-driver-check`.

use libadwaita as adw;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::NamedTempFile;
use thiserror::Error;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

// ============================================================================
// 1. KIẾN TRÚC LỖI ĐỊNH KIỂU (STRONGLY-TYPED DOMAIN ERRORS)
// ============================================================================

#[derive(Debug, Error)]
pub enum CaramDesktopError {
    #[error("Tham số gói '{arg}' bị từ chối: {reason}")]
    InvalidTarget { arg: String, reason: &'static str },

    #[error("Liên kết cài đặt không hợp lệ: {raw}")]
    InvalidInstallLink { raw: String },

    #[error("Không thể kết nối tới '{url}': {reason}")]
    Network { url: String, reason: String },

    #[error("Không tìm thấy ứng dụng '{app_id}' trên Flathub")]
    FlathubNotFound { app_id: String },

    #[error("Lỗi xác thực SHA-256 cho '{file}': dữ liệu tải về không toàn vẹn")]
    ChecksumMismatch { file: String },

    #[error("Không thể thực thi '{binary}': {source}")]
    SpawnFailed {
        binary: String,
        #[source]
        source: io::Error,
    },

    #[error("Thao tác '{action}' thất bại: {detail}")]
    CommandFailed { action: String, detail: String },

    #[error("Người dùng đã hủy xác thực quyền quản trị")]
    AuthCancelled,

    #[error("Lỗi đọc/ghi tệp ({context}): {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

// ============================================================================
// 2. CSS CHUẨN `DESIGN.md` (KHỚP 1:1 `style.css` & `style-dark.css` CARAMOS GỐC)
// ============================================================================

pub const CARAMOS_STYLE_CSS: &str = r#"
@define-color accent_bg_color #2b6a44;
@define-color accent_fg_color #ffffff;
@define-color accent_color #276140;

window, .background {
    font-family: "Be Vietnam Pro", "Noto Sans", sans-serif;
}

.title-1, .title-2, .title-3, .title-4, .heading, button {
    font-weight: 600;
}

list.boxed-list > row.app-row {
    padding: 12px 16px;
}

list.boxed-list > row.text-row {
    padding: 12px 16px;
}

.row-title {
    font-weight: 600;
}

.secondary {
    font-size: 92%;
}

.page-title {
    font-size: 160%;
    font-weight: 600;
    margin-bottom: 4px;
}

.section-heading {
    font-weight: 600;
    margin-top: 8px;
}

.summary-card {
    padding: 18px 20px;
}

.row-info {
    opacity: 0.65;
}

.row-info:hover {
    opacity: 1;
}

.details-text {
    font-size: 90%;
}

.more-apps-bar {
    padding: 10px 14px;
    border-radius: 12px;
    background-color: alpha(@window_fg_color, 0.05);
}

.more-apps-bar:hover {
    background-color: alpha(@window_fg_color, 0.09);
}

.wizard-bar {
    padding: 12px 24px;
    border-top: 1px solid @borders;
}

.status-ok {
    color: @accent_color;
    font-weight: 600;
}

.status-warn {
    color: @warning_color;
    font-weight: 600;
}

.status-err {
    color: @error_color;
    font-weight: 600;
}
"#;

pub const CARAMOS_STYLE_DARK_CSS: &str = r#"
@define-color accent_bg_color #3a855a;
@define-color accent_fg_color #ffffff;
@define-color accent_color #8ccfa5;
"#;

/// Cài CSS chuẩn `DESIGN.md` và tự động theo dõi chế độ Sáng/Tối của hệ thống.
pub fn install_caramos_css() {
    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };

    let base_provider = gtk4::CssProvider::new();
    base_provider.load_from_string(CARAMOS_STYLE_CSS);
    gtk4::style_context_add_provider_for_display(
        &display,
        &base_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let dark_provider = gtk4::CssProvider::new();
    let style_mgr = adw::StyleManager::default();
    dark_provider.load_from_string(if style_mgr.is_dark() {
        CARAMOS_STYLE_DARK_CSS
    } else {
        ""
    });
    gtk4::style_context_add_provider_for_display(
        &display,
        &dark_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );

    style_mgr.connect_dark_notify(move |mgr| {
        dark_provider.load_from_string(if mgr.is_dark() {
            CARAMOS_STYLE_DARK_CSS
        } else {
            ""
        });
    });
}

// ============================================================================
// 3. DANH MỤC ỨNG DỤNG CARAMOS APP CENTER (6 NHÓM CHUẨN CARAMOS GỐC, BỎ DOANH NGHIỆP)
// ============================================================================

pub const VNLF_APPS_URL: &str = "https://apps.vietnamlinuxfamily.net/?from=caramos";
pub const CARAMOS_LINK_PREFIX: &str = "caramos-app://install/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepSelectionMode {
    SingleOptional,
    Multi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppCategory {
    Office,
    Browsers,
    ChatAndMeetings,
    EverydayTools,
    MediaAndDesign,
    Education,
}

impl AppCategory {
    pub const ALL: [Self; 6] = [
        Self::Office,
        Self::Browsers,
        Self::ChatAndMeetings,
        Self::EverydayTools,
        Self::MediaAndDesign,
        Self::Education,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Office => "Bộ ứng dụng văn phòng",
            Self::Browsers => "Trình duyệt web",
            Self::ChatAndMeetings => "Liên lạc và họp trực tuyến",
            Self::EverydayTools => "Tiện ích thường dùng",
            Self::MediaAndDesign => "Phim, nhạc, ảnh và thiết kế",
            Self::Education => "Giáo dục và học tập",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Self::Office => {
                "Soạn văn bản, bảng tính và trình chiếu. Chọn một bộ, có thể cài thêm bộ khác bất cứ lúc nào."
            }
            Self::Browsers => "Firefox ESR đã có sẵn. Chọn thêm trình duyệt bạn quen dùng.",
            Self::ChatAndMeetings => {
                "Nhắn tin, gọi thoại, gọi video và họp trực tuyến. Zalo được chọn sẵn."
            }
            Self::EverydayTools => {
                "Phông chữ cho tệp Word, thư điện tử, ghi chú, hỗ trợ từ xa và VPN cơ quan."
            }
            Self::MediaAndDesign => {
                "Xem phim, nghe nhạc, quay màn hình, dựng video, sửa ảnh và vẽ vector."
            }
            Self::Education => {
                "Tập dùng chuột, gõ 10 ngón, vẽ, Scratch 3, Python, Toán và Thiên văn cho học sinh, giáo viên."
            }
        }
    }

    pub fn selection_mode(self) -> StepSelectionMode {
        match self {
            Self::Office => StepSelectionMode::SingleOptional,
            _ => StepSelectionMode::Multi,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSource {
    Preinstalled {
        check_pkgs: &'static [&'static str],
    },
    Apt {
        packages: &'static [&'static str],
        check_pkgs: &'static [&'static str],
    },
    Flatpak {
        app_id: &'static str,
    },
    GithubDeb {
        repo: &'static str,
        check_pkgs: &'static [&'static str],
        remove_pkgs: &'static [&'static str],
    },
    DirectDeb {
        url: &'static str,
        check_pkgs: &'static [&'static str],
        remove_pkgs: &'static [&'static str],
    },
}

#[derive(Debug, Clone, Copy)]
pub struct AppEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub category: AppCategory,
    pub icon_name: &'static str,
    pub summary: &'static str,
    pub pros: &'static [&'static str],
    pub cons: &'static [&'static str],
    pub licence_note: &'static str,
    pub website: &'static str,
    pub flathub_id: Option<&'static str>,
    pub size_mb: u32,
    pub recommended: bool,
    pub default_selected: bool,
    pub source: InstallSource,
}

impl AppEntry {
    pub fn is_preinstalled(&self) -> bool {
        matches!(self.source, InstallSource::Preinstalled { .. })
    }
}

pub const CARAMOS_APP_CATALOG: &[AppEntry] = &[
    // --- 1. Bộ ứng dụng văn phòng ---
    AppEntry {
        id: "onlyoffice",
        name: "ONLYOFFICE",
        category: AppCategory::Office,
        icon_name: "x-office-document",
        summary: "Văn bản, bảng tính, trình chiếu theo kiểu tab",
        pros: &[
            "Mở và lưu .docx, .xlsx, .pptx giữ định dạng rất tốt",
            "Giao diện ribbon quen thuộc như Microsoft Office",
            "Miễn phí, mã nguồn mở",
        ],
        cons: &[
            "Tốn dung lượng đĩa và RAM hơn LibreOffice",
            "Macro VBA phức tạp của Excel chạy hạn chế",
        ],
        licence_note: "Mã nguồn mở, dùng miễn phí cho cả cá nhân và công ty.",
        website: "https://www.onlyoffice.com/",
        flathub_id: Some("org.onlyoffice.desktopeditors"),
        size_mb: 1300,
        recommended: true,
        default_selected: true,
        source: InstallSource::Flatpak {
            app_id: "org.onlyoffice.desktopeditors",
        },
    },
    AppEntry {
        id: "wps-office",
        name: "WPS Office",
        category: AppCategory::Office,
        icon_name: "x-office-spreadsheet",
        summary: "Bộ văn phòng gọn nhẹ, giống Microsoft Office",
        pros: &[
            "Giao diện gần giống Microsoft Office nhất",
            "Nhẹ, mở nhanh trên máy cấu hình thấp",
            "Có sẵn trình đọc và chỉnh sửa PDF",
        ],
        cons: &[
            "Mã nguồn đóng",
            "Bản Linux qua Flathub do cộng đồng đóng gói lại",
        ],
        licence_note: "Mã nguồn đóng; điều khoản dùng trong doanh nghiệp theo quy định Kingsoft.",
        website: "https://www.wps.com/",
        flathub_id: Some("com.wps.Office"),
        size_mb: 1100,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "com.wps.Office",
        },
    },
    AppEntry {
        id: "libreoffice",
        name: "LibreOffice",
        category: AppCategory::Office,
        icon_name: "libreoffice-writer",
        summary: "Bộ văn phòng mã nguồn mở đầy đủ nhất, cài trực tiếp từ kho Debian",
        pros: &[
            "Đầy đủ Writer, Calc, Impress, Draw, Base, Math",
            "Giao diện tiếng Việt đầy đủ, kiểm tra chính tả tốt",
            "Nhẹ, cập nhật cùng kho phần mềm Debian",
        ],
        cons: &[
            "Giao diện khác Microsoft Office",
            "Tệp .docx quá phức tạp đôi khi lệch dòng nhẹ",
        ],
        licence_note: "Mã nguồn mở, dùng miễn phí cho cả cá nhân và công ty.",
        website: "https://www.libreoffice.org/",
        flathub_id: Some("org.libreoffice.LibreOffice"),
        size_mb: 420,
        recommended: false,
        default_selected: false,
        source: InstallSource::Apt {
            packages: &[
                "libreoffice-writer",
                "libreoffice-calc",
                "libreoffice-impress",
                "libreoffice-gtk3",
                "libreoffice-l10n-vi",
            ],
            check_pkgs: &["libreoffice-writer"],
        },
    },
    // --- 2. Trình duyệt web ---
    AppEntry {
        id: "google-chrome",
        name: "Google Chrome",
        category: AppCategory::Browsers,
        icon_name: "web-browser",
        summary: "Trình duyệt phổ biến nhất của Google",
        pros: &[
            "Đồng bộ dấu trang, mật khẩu với tài khoản Google",
            "Hầu hết trang web Việt Nam và dịch vụ công ưu tiên hỗ trợ Chrome",
            "Kho tiện ích mở rộng lớn nhất",
        ],
        cons: &["Tốn RAM hơn khi mở nhiều tab", "Mã nguồn đóng"],
        licence_note: "Miễn phí, mã nguồn đóng theo điều khoản Google.",
        website: "https://www.google.com/chrome/",
        flathub_id: Some("com.google.Chrome"),
        size_mb: 380,
        recommended: true,
        default_selected: true,
        source: InstallSource::Flatpak {
            app_id: "com.google.Chrome",
        },
    },
    AppEntry {
        id: "coccoc",
        name: "Cốc Cốc",
        category: AppCategory::Browsers,
        icon_name: "web-browser",
        summary: "Trình duyệt Việt Nam hỗ trợ tải video và chặn quảng cáo sẵn",
        pros: &[
            "Tải video, nhạc từ trang web chỉ một cú bấm",
            "Chặn quảng cáo có sẵn",
            "Giao diện và hỗ trợ bằng tiếng Việt",
        ],
        cons: &["Bản Linux đóng gói dạng .deb riêng", "Mã nguồn đóng"],
        licence_note: "Miễn phí, mã nguồn đóng theo điều khoản Cốc Cốc.",
        website: "https://coccoc.com/",
        flathub_id: None,
        size_mb: 450,
        recommended: false,
        default_selected: false,
        source: InstallSource::DirectDeb {
            url: "https://files.coccoc.com/browser/coccoc-browser-stable_amd64.deb",
            check_pkgs: &["coccoc-browser-stable"],
            remove_pkgs: &["coccoc-browser-stable"],
        },
    },
    AppEntry {
        id: "microsoft-edge",
        name: "Microsoft Edge",
        category: AppCategory::Browsers,
        icon_name: "web-browser",
        summary: "Trình duyệt của Microsoft, đồng bộ tài khoản Microsoft 365",
        pros: &[
            "Đồng bộ tốt với Outlook, OneDrive và tài khoản công việc",
            "Có chế độ đọc và dịch trang tích hợp",
        ],
        cons: &["Nhiều gợi ý dịch vụ Microsoft", "Mã nguồn đóng"],
        licence_note: "Miễn phí, mã nguồn đóng theo điều khoản Microsoft.",
        website: "https://www.microsoft.com/edge",
        flathub_id: Some("com.microsoft.Edge"),
        size_mb: 520,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "com.microsoft.Edge",
        },
    },
    AppEntry {
        id: "brave",
        name: "Brave",
        category: AppCategory::Browsers,
        icon_name: "web-browser",
        summary: "Trình duyệt chặn quảng cáo và trình theo dõi ngay khi cài",
        pros: &[
            "Chặn quảng cáo và theo dõi mặc định",
            "Dùng chung tiện ích mở rộng với Chrome",
        ],
        cons: &["Một số trang cần tắt khiên chặn mới hiển thị đúng"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://brave.com/",
        flathub_id: Some("com.brave.Browser"),
        size_mb: 420,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "com.brave.Browser",
        },
    },
    AppEntry {
        id: "firefox",
        name: "Firefox ESR",
        category: AppCategory::Browsers,
        icon_name: "firefox-esr",
        summary: "Trình duyệt mã nguồn mở có sẵn trong CaramOS",
        pros: &[
            "Có sẵn cùng hệ thống, nhận bản vá bảo mật đều đặn từ Debian",
            "Tôn trọng quyền riêng tư",
        ],
        cons: &["Một vài trang nội bộ chỉ tối ưu cho Chromium"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://www.mozilla.org/firefox/",
        flathub_id: Some("org.mozilla.firefox"),
        size_mb: 250,
        recommended: false,
        default_selected: false,
        source: InstallSource::Preinstalled {
            check_pkgs: &["firefox-esr", "firefox"],
        },
    },
    // --- 3. Liên lạc và họp trực tuyến ---
    AppEntry {
        id: "zalo-full",
        name: "Zalo",
        category: AppCategory::ChatAndMeetings,
        icon_name: "internet-chat",
        summary: "Nhắn tin, gọi thoại, gọi video không cần Wine (bản cộng đồng VNLF)",
        pros: &[
            "Gọi thoại, gọi video, chia sẻ màn hình mượt mà",
            "Tương thích bộ gõ Fcitx5 Lotus trên CaramOS",
        ],
        cons: &["Bản Linux do cộng đồng VNLF đóng gói, không phải bản chính thức từ Zalo"],
        licence_note: "Miễn phí, mã nguồn đóng theo điều khoản Zalo.",
        website: "https://github.com/VN-Linux-Family/Zalo-for-linux",
        flathub_id: None,
        size_mb: 170,
        recommended: true,
        default_selected: true,
        source: InstallSource::GithubDeb {
            repo: "VN-Linux-Family/Zalo-for-linux",
            check_pkgs: &["zalo-desktop", "zalo"],
            remove_pkgs: &["zalo-desktop", "zalo"],
        },
    },
    AppEntry {
        id: "telegram",
        name: "Telegram",
        category: AppCategory::ChatAndMeetings,
        icon_name: "internet-chat",
        summary: "Nhắn tin nhanh, nhóm lớn và gửi tệp tới 2 GB",
        pros: &[
            "Nhẹ, đồng bộ tức thì giữa máy tính và điện thoại",
            "Gửi tệp dung lượng lớn ổn định",
        ],
        cons: &["Cuộc trò chuyện thường không bật mã hóa đầu cuối mặc định"],
        licence_note: "Ứng dụng mã nguồn mở, dùng miễn phí.",
        website: "https://telegram.org/",
        flathub_id: Some("org.telegram.desktop"),
        size_mb: 200,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "org.telegram.desktop",
        },
    },
    AppEntry {
        id: "zoom",
        name: "Zoom",
        category: AppCategory::ChatAndMeetings,
        icon_name: "camera-video",
        summary: "Họp và học trực tuyến phổ biến",
        pros: &[
            "Trường học và doanh nghiệp sử dụng rộng rãi",
            "Chia sẻ màn hình và phòng thảo luận đầy đủ",
        ],
        cons: &["Mã nguồn đóng", "Gói cài đặt khá lớn"],
        licence_note: "Miễn phí có giới hạn thời lượng theo điều khoản Zoom.",
        website: "https://zoom.us/",
        flathub_id: Some("us.zoom.Zoom"),
        size_mb: 800,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "us.zoom.Zoom",
        },
    },
    AppEntry {
        id: "teams",
        name: "Teams for Linux",
        category: AppCategory::ChatAndMeetings,
        icon_name: "internet-chat",
        summary: "Dùng Microsoft Teams trên Linux",
        pros: &[
            "Tham gia họp và nhắn tin Teams của công ty, trường học",
            "Có thông báo desktop và chia sẻ màn hình",
        ],
        cons: &["Ứng dụng cộng đồng mã nguồn mở, không do Microsoft phát hành"],
        licence_note: "Ứng dụng mã nguồn mở; tài khoản Teams theo điều khoản Microsoft.",
        website: "https://github.com/IsmaelMartinez/teams-for-linux",
        flathub_id: Some("com.github.IsmaelMartinez.teams_for_linux"),
        size_mb: 300,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "com.github.IsmaelMartinez.teams_for_linux",
        },
    },
    AppEntry {
        id: "discord",
        name: "Discord",
        category: AppCategory::ChatAndMeetings,
        icon_name: "internet-chat",
        summary: "Trò chuyện thoại, cộng đồng học tập và công nghệ",
        pros: &[
            "Kênh thoại chất lượng ổn định",
            "Tự cập nhật gọn qua Flathub",
        ],
        cons: &["Tốn RAM mức trung bình", "Mã nguồn đóng"],
        licence_note: "Miễn phí, mã nguồn đóng theo điều khoản Discord.",
        website: "https://discord.com/",
        flathub_id: Some("com.discordapp.Discord"),
        size_mb: 300,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "com.discordapp.Discord",
        },
    },
    // --- 4. Tiện ích thường dùng ---
    AppEntry {
        id: "ms-fonts",
        name: "Phông chữ văn phòng tương thích Windows",
        category: AppCategory::EverydayTools,
        icon_name: "font-x-generic",
        summary: "Bộ phông thay thế khớp kích thước Times New Roman, Arial, Calibri, Cambria",
        pros: &[
            "Tệp Word, Excel từ Windows hiển thị không bị nhảy dòng",
            "Mã nguồn mở, cài tức thì từ kho Debian",
        ],
        cons: &["Không kèm các phông trang trí đặc thù của Windows"],
        licence_note: "Mã nguồn mở, dùng miễn phí cho cả cá nhân và công ty.",
        website: "https://wiki.debian.org/Fonts",
        flathub_id: None,
        size_mb: 12,
        recommended: true,
        default_selected: true,
        source: InstallSource::Apt {
            packages: &[
                "fonts-liberation",
                "fonts-crosextra-carlito",
                "fonts-crosextra-caladea",
            ],
            check_pkgs: &["fonts-crosextra-caladea"],
        },
    },
    AppEntry {
        id: "thunderbird",
        name: "Thunderbird",
        category: AppCategory::EverydayTools,
        icon_name: "mail-client",
        summary: "Đọc thư Gmail, Outlook và quản lý lịch ngay trên máy",
        pros: &[
            "Gom nhiều hộp thư vào một chỗ",
            "Có sẵn tiếng Việt và tích hợp lịch, danh bạ",
        ],
        cons: &["Không cần thiết nếu bạn chỉ đọc thư trên trình duyệt"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://www.thunderbird.net/",
        flathub_id: Some("org.mozilla.Thunderbird"),
        size_mb: 150,
        recommended: false,
        default_selected: false,
        source: InstallSource::Apt {
            packages: &["thunderbird", "thunderbird-l10n-vi"],
            check_pkgs: &["thunderbird"],
        },
    },
    AppEntry {
        id: "obsidian",
        name: "Obsidian",
        category: AppCategory::EverydayTools,
        icon_name: "accessories-text-editor",
        summary: "Ghi chú cá nhân lưu tệp Markdown trực tiếp trên máy",
        pros: &[
            "Dùng hoàn toàn ngoại tuyến, dữ liệu thuộc về bạn",
            "Liên kết ghi chú và tìm kiếm tức thì",
        ],
        cons: &["Mã nguồn đóng", "Đồng bộ đám mây chính hãng có phí"],
        licence_note: "Miễn phí cho cá nhân và thương mại theo điều khoản Obsidian.",
        website: "https://obsidian.md/",
        flathub_id: Some("md.obsidian.Obsidian"),
        size_mb: 670,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "md.obsidian.Obsidian",
        },
    },
    AppEntry {
        id: "rustdesk",
        name: "RustDesk",
        category: AppCategory::EverydayTools,
        icon_name: "preferences-desktop-remote-desktop",
        summary: "Điều khiển máy tính từ xa mã nguồn mở, thay UltraViewer",
        pros: &[
            "Nhờ người thân hoặc kỹ thuật hỗ trợ từ xa không giới hạn phiên",
            "Viết bằng Rust, miễn phí và mã nguồn mở",
        ],
        cons: &["Người hỗ trợ cũng cần cài RustDesk"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://rustdesk.com/",
        flathub_id: Some("com.rustdesk.RustDesk"),
        size_mb: 70,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "com.rustdesk.RustDesk",
        },
    },
    AppEntry {
        id: "openconnect",
        name: "OpenConnect VPN",
        category: AppCategory::EverydayTools,
        icon_name: "network-vpn",
        summary: "Kết nối VPN Cisco AnyConnect, GlobalProtect, Fortinet của cơ quan",
        pros: &[
            "Tích hợp thẳng vào Cài đặt mạng, không cần phần mềm Cisco",
            "Vào được cổng thông tin nội bộ cơ quan, trường học",
        ],
        cons: &["Cần địa chỉ máy chủ VPN và tài khoản do cơ quan cấp"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://www.infradead.org/openconnect/",
        flathub_id: None,
        size_mb: 11,
        recommended: false,
        default_selected: false,
        source: InstallSource::Apt {
            packages: &["openconnect", "network-manager-openconnect-gnome"],
            check_pkgs: &["network-manager-openconnect-gnome"],
        },
    },
    // --- 5. Phim, nhạc, ảnh và thiết kế ---
    AppEntry {
        id: "vlc",
        name: "VLC",
        category: AppCategory::MediaAndDesign,
        icon_name: "multimedia-video-player",
        summary: "Xem phim, nghe nhạc mọi định dạng không cần cài codec",
        pros: &[
            "Mở gần như mọi tệp phim, nhạc và phụ đề tiếng Việt",
            "Nhẹ, cài trực tiếp từ kho Debian",
        ],
        cons: &["Giao diện cổ điển đơn giản"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://www.videolan.org/vlc/",
        flathub_id: Some("org.videolan.VLC"),
        size_mb: 80,
        recommended: true,
        default_selected: true,
        source: InstallSource::Apt {
            packages: &["vlc"],
            check_pkgs: &["vlc"],
        },
    },
    AppEntry {
        id: "spotify",
        name: "Spotify",
        category: AppCategory::MediaAndDesign,
        icon_name: "audio-x-generic",
        summary: "Nghe nhạc và podcast trực tuyến",
        pros: &[
            "Đồng bộ danh sách nhạc với điện thoại",
            "Chạy ổn định qua gói Flathub",
        ],
        cons: &["Cần tài khoản Spotify và kết nối Internet"],
        licence_note: "Miễn phí có quảng cáo, mã nguồn đóng theo điều khoản Spotify.",
        website: "https://www.spotify.com/",
        flathub_id: Some("com.spotify.Client"),
        size_mb: 390,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "com.spotify.Client",
        },
    },
    AppEntry {
        id: "obs-studio",
        name: "OBS Studio",
        category: AppCategory::MediaAndDesign,
        icon_name: "camera-video",
        summary: "Quay màn hình bài giảng và phát trực tiếp chất lượng cao",
        pros: &[
            "Quay bài giảng mượt với PipeWire",
            "Livestream YouTube, Facebook",
        ],
        cons: &["Nhiều tùy chỉnh, cần làm quen ban đầu"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://obsproject.com/",
        flathub_id: Some("com.obsproject.Studio"),
        size_mb: 160,
        recommended: false,
        default_selected: false,
        source: InstallSource::Apt {
            packages: &["obs-studio"],
            check_pkgs: &["obs-studio"],
        },
    },
    AppEntry {
        id: "kdenlive",
        name: "Kdenlive",
        category: AppCategory::MediaAndDesign,
        icon_name: "applications-multimedia",
        summary: "Dựng video: cắt ghép, thêm chữ, nhạc nền và hiệu ứng",
        pros: &[
            "Biên tập bài giảng từ video quay bằng OBS Studio",
            "Miễn phí, không gắn watermark",
        ],
        cons: &["Cần máy đủ mạnh khi xuất video dài"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://kdenlive.org/",
        flathub_id: Some("org.kde.kdenlive"),
        size_mb: 1100,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "org.kde.kdenlive",
        },
    },
    AppEntry {
        id: "gimp",
        name: "GIMP",
        category: AppCategory::MediaAndDesign,
        icon_name: "applications-graphics",
        summary: "Chỉnh sửa ảnh chuyên sâu thay thế Photoshop",
        pros: &[
            "Đầy đủ layer, mặt nạ, bộ lọc và xuất PSD",
            "Miễn phí, mã nguồn mở",
        ],
        cons: &["Bố cục công cụ khác Photoshop"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://www.gimp.org/",
        flathub_id: Some("org.gimp.GIMP"),
        size_mb: 450,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "org.gimp.GIMP",
        },
    },
    AppEntry {
        id: "inkscape",
        name: "Inkscape",
        category: AppCategory::MediaAndDesign,
        icon_name: "applications-graphics",
        summary: "Vẽ hình vector: logo, áp phích và sơ đồ bài giảng",
        pros: &[
            "Thay thế Illustrator, xuất PDF, PNG, SVG sắc nét",
            "Miễn phí, mã nguồn mở",
        ],
        cons: &["Tệp .ai phức tạp có thể lệch chi tiết nhỏ"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://inkscape.org/",
        flathub_id: Some("org.inkscape.Inkscape"),
        size_mb: 840,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "org.inkscape.Inkscape",
        },
    },
    // --- 6. Giáo dục và học tập ---
    AppEntry {
        id: "gcompris",
        name: "GCompris",
        category: AppCategory::Education,
        icon_name: "applications-education",
        summary: "Hơn 180 trò chơi học tập cho bé từ 2 đến 10 tuổi",
        pros: &[
            "Tập dùng chuột, bàn phím cho tiết Tin học tiểu học",
            "Toán, đọc, khoa học, địa lý qua trò chơi trực quan",
        ],
        cons: &["Một số trò chơi dùng tiếng Anh"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://gcompris.net/",
        flathub_id: Some("org.kde.gcompris"),
        size_mb: 98,
        recommended: true,
        default_selected: false,
        source: InstallSource::Apt {
            packages: &["gcompris-qt"],
            check_pkgs: &["gcompris-qt"],
        },
    },
    AppEntry {
        id: "tuxpaint",
        name: "Tux Paint",
        category: AppCategory::Education,
        icon_name: "applications-graphics",
        summary: "Vẽ tranh cho học sinh tiểu học, có con dấu và giao diện tiếng Việt",
        pros: &[
            "Dễ dùng hơn Paint, rất hợp học sinh nhỏ",
            "Có giao diện tiếng Việt",
        ],
        cons: &["Không có lớp vẽ nâng cao"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://tuxpaint.org/",
        flathub_id: Some("org.tuxpaint.Tuxpaint"),
        size_mb: 276,
        recommended: false,
        default_selected: false,
        source: InstallSource::Apt {
            packages: &["tuxpaint"],
            check_pkgs: &["tuxpaint"],
        },
    },
    AppEntry {
        id: "kolourpaint",
        name: "KolourPaint",
        category: AppCategory::Education,
        icon_name: "applications-graphics",
        summary: "Vẽ và sửa ảnh đơn giản giống Paint trên Windows",
        pros: &["Công cụ quen thuộc cho bài vẽ Tin học", "Nhẹ, mở tức thì"],
        cons: &["Kéo theo một số thư viện KDE"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://apps.kde.org/kolourpaint/",
        flathub_id: Some("org.kde.kolourpaint"),
        size_mb: 82,
        recommended: false,
        default_selected: false,
        source: InstallSource::Apt {
            packages: &["kolourpaint"],
            check_pkgs: &["kolourpaint"],
        },
    },
    AppEntry {
        id: "klavaro",
        name: "Klavaro",
        category: AppCategory::Education,
        icon_name: "input-keyboard",
        summary: "Luyện gõ bàn phím 10 ngón có bài tập tiếng Việt",
        pros: &[
            "Rất nhẹ, chạy tốt trên máy cũ",
            "Có bài tập với văn bản tiếng Việt",
        ],
        cons: &["Giao diện tiếng Anh"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://klavaro.sourceforge.io/",
        flathub_id: Some("net.sourceforge.Klavaro"),
        size_mb: 4,
        recommended: false,
        default_selected: false,
        source: InstallSource::Apt {
            packages: &["klavaro"],
            check_pkgs: &["klavaro"],
        },
    },
    AppEntry {
        id: "turbowarp",
        name: "Scratch 3 (TurboWarp)",
        category: AppCategory::Education,
        icon_name: "applications-development",
        summary: "Lập trình kéo thả Scratch 3 ngoại tuyến có tiếng Việt",
        pros: &[
            "Mở và lưu tệp .sb3 như Scratch trên web mà không cần mạng",
            "Có giao diện tiếng Việt",
        ],
        cons: &["Lần đầu tải bộ chạy Flatpak"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://turbowarp.org/",
        flathub_id: Some("org.turbowarp.TurboWarp"),
        size_mb: 400,
        recommended: true,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "org.turbowarp.TurboWarp",
        },
    },
    AppEntry {
        id: "thonny",
        name: "Thonny",
        category: AppCategory::Education,
        icon_name: "applications-development",
        summary: "Viết Python trực quan cho học sinh THCS, THPT",
        pros: &[
            "Chạy từng dòng để xem biến thay đổi",
            "Gọn nhẹ, mở lên là lập trình được ngay",
        ],
        cons: &["Giao diện tiếng Anh"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://thonny.org/",
        flathub_id: Some("org.thonny.Thonny"),
        size_mb: 88,
        recommended: false,
        default_selected: false,
        source: InstallSource::Apt {
            packages: &["thonny"],
            check_pkgs: &["thonny"],
        },
    },
    AppEntry {
        id: "geogebra",
        name: "GeoGebra",
        category: AppCategory::Education,
        icon_name: "accessories-calculator",
        summary: "Hình học, đồ thị hàm số và đại số động cho môn Toán",
        pros: &[
            "Có tiếng Việt, mở được tệp .ggb",
            "Kéo thả hình học trực quan",
        ],
        cons: &["Miễn phí học tập, không phải mã nguồn mở"],
        licence_note: "Miễn phí cho học tập và sử dụng phi thương mại.",
        website: "https://www.geogebra.org/",
        flathub_id: Some("org.geogebra.GeoGebra"),
        size_mb: 350,
        recommended: false,
        default_selected: false,
        source: InstallSource::Flatpak {
            app_id: "org.geogebra.GeoGebra",
        },
    },
    AppEntry {
        id: "stellarium",
        name: "Stellarium",
        category: AppCategory::Education,
        icon_name: "applications-science",
        summary: "Bầu trời sao 3D cho môn Khoa học tự nhiên và Địa lý",
        pros: &["Giao diện tiếng Việt, mô phỏng chòm sao và nhật thực sống động"],
        cons: &["Dung lượng khoảng 575 MB, cần đồ họa OpenGL cơ bản"],
        licence_note: "Mã nguồn mở, dùng miễn phí.",
        website: "https://stellarium.org/",
        flathub_id: Some("org.stellarium.Stellarium"),
        size_mb: 575,
        recommended: false,
        default_selected: false,
        source: InstallSource::Apt {
            packages: &["stellarium"],
            check_pkgs: &["stellarium"],
        },
    },
];

pub fn find_catalog_app_by_flathub_id(flathub_id: &str) -> Option<&'static AppEntry> {
    CARAMOS_APP_CATALOG.iter().find(|entry| {
        entry.flathub_id == Some(flathub_id)
            || matches!(entry.source, InstallSource::Flatpak { app_id } if app_id == flathub_id)
    })
}

pub fn format_size_mb(size_mb: u32) -> String {
    if size_mb >= 1000 {
        let gb = f64::from(size_mb) / 1000.0;
        format!("~{:.1} GB", gb)
    } else {
        format!("~{} MB", size_mb)
    }
}

// ============================================================================
// 4. PHÂN TÍCH LIÊN KẾT SÂU `caramos-app://install/<Flathub-id>` & FLATHUB API
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct FlathubAppMeta {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub developer_name: Option<String>,
    #[serde(default)]
    pub project_license: Option<String>,
    #[serde(default)]
    pub urls: HashMap<String, String>,
}

/// Kiểm tra cú pháp ID ứng dụng Flathub dạng reverse-DNS (`org.videolan.VLC`), chặn ký tự độc hại.
pub fn parse_install_link(raw: &str) -> Result<String, CaramDesktopError> {
    let trimmed = raw.trim();
    let rest = trimmed.strip_prefix(CARAMOS_LINK_PREFIX).ok_or_else(|| {
        CaramDesktopError::InvalidInstallLink {
            raw: trimmed.to_string(),
        }
    })?;
    let app_id = rest
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');

    let segments: Vec<&str> = app_id.split('.').collect();
    let valid_len = !app_id.is_empty() && app_id.len() <= 160 && segments.len() >= 3;
    let valid_segments = segments.iter().all(|seg| {
        !seg.is_empty()
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            && seg
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic() || c == '_')
                .unwrap_or(false)
    });

    if valid_len && valid_segments {
        Ok(app_id.to_string())
    } else {
        Err(CaramDesktopError::InvalidInstallLink {
            raw: trimmed.to_string(),
        })
    }
}

pub fn fetch_flathub_app_meta(app_id: &str) -> Result<FlathubAppMeta, CaramDesktopError> {
    let url = format!("https://flathub.org/api/v2/appstream/{}", app_id);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(25))
        .build();

    let resp = match agent
        .get(&url)
        .set("User-Agent", "caramos-app-center/2.0")
        .set("Accept", "application/json")
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(404, _)) => {
            return Err(CaramDesktopError::FlathubNotFound {
                app_id: app_id.to_string(),
            });
        }
        Err(e) => {
            return Err(CaramDesktopError::Network {
                url,
                reason: e.to_string(),
            });
        }
    };

    resp.into_json::<FlathubAppMeta>()
        .map_err(|e| CaramDesktopError::Network {
            url,
            reason: format!("Phản hồi JSON Flathub không hợp lệ: {}", e),
        })
}

// ============================================================================
// 5. KIỂM ĐỊNH GÓI HÀNG LOẠT (NỀN), TẢI `.deb` AN TOÀN & THỰC THI ĐẶC QUYỀN
// ============================================================================

/// Đọc toàn bộ tên gói `.deb` đã cài bằng 1 lần gọi `dpkg-query` (O(1) subprocess thay vì O(N)).
pub fn snapshot_installed_debs() -> HashSet<String> {
    let Ok(out) = Command::new("dpkg-query")
        .args(["-W", "-f=${Package}\t${Status}\n"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return HashSet::new();
    };
    if !out.status.success() {
        return HashSet::new();
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut set = HashSet::new();
    for line in text.lines() {
        if let Some((pkg, status)) = line.split_once('\t') {
            if status.contains("install ok installed") {
                set.insert(pkg.trim().to_string());
            }
        }
    }
    set
}

/// Đọc toàn bộ ID Flatpak đã cài (cả `--user` và `--system`) bằng 1 lần gọi `flatpak list`.
pub fn snapshot_installed_flatpaks() -> HashSet<String> {
    let Ok(out) = Command::new("flatpak")
        .args(["list", "--app", "--columns=application"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return HashSet::new();
    };
    if !out.status.success() {
        return HashSet::new();
    }

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Quét toàn bộ danh mục ứng dụng chỉ với đúng 2 tiến trình hệ thống (`dpkg-query` + `flatpak list`).
pub fn detect_installed_catalog_ids() -> HashSet<String> {
    let debs = snapshot_installed_debs();
    let flatpaks = snapshot_installed_flatpaks();
    let mut installed = HashSet::new();

    for entry in CARAMOS_APP_CATALOG {
        let is_present = match entry.source {
            InstallSource::Preinstalled { check_pkgs }
            | InstallSource::Apt { check_pkgs, .. }
            | InstallSource::GithubDeb { check_pkgs, .. }
            | InstallSource::DirectDeb { check_pkgs, .. } => {
                check_pkgs.iter().any(|p| debs.contains(*p))
            }
            InstallSource::Flatpak { app_id } => flatpaks.contains(app_id),
        };
        if is_present {
            installed.insert(entry.id.to_string());
        }
    }
    installed
}

pub fn is_deb_pkg_installed(pkg: &str) -> bool {
    Command::new("dpkg-query")
        .args(["-W", "-f=${Status}", pkg])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).contains("install ok installed")
        })
        .unwrap_or(false)
}

pub fn is_flatpak_installed(app_id: &str) -> bool {
    Command::new("flatpak")
        .args(["info", app_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn private_download_dir() -> Result<PathBuf, CaramDesktopError> {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or_else(|| "user".to_string());
    let dir = base.join(format!("caramos-staging-{}", uid));
    fs::create_dir_all(&dir).map_err(|e| CaramDesktopError::Io {
        context: format!("Tạo thư mục tải tạm '{}'", dir.display()),
        source: e,
    })?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

pub fn validate_apt_target(arg: &str) -> Result<(), CaramDesktopError> {
    if arg.is_empty() || arg.len() > 220 || arg.contains("..") || arg.contains('\0') {
        return Err(CaramDesktopError::InvalidTarget {
            arg: arg.to_string(),
            reason: "Độ dài không hợp lệ hoặc chứa chuỗi leo thang đường dẫn",
        });
    }

    if arg.starts_with('/') {
        let valid_deb_path = arg.ends_with(".deb")
            && (arg.starts_with("/tmp/caramos-") || arg.starts_with("/run/user/"))
            && arg.contains("caramos-")
            && Path::new(arg).is_file();
        return if valid_deb_path {
            Ok(())
        } else {
            Err(CaramDesktopError::InvalidTarget {
                arg: arg.to_string(),
                reason: "Đường dẫn tệp .deb cục bộ không nằm trong vùng đệm CaramOS hợp lệ",
            })
        };
    }

    let valid_pkg = arg
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '+'))
        && arg
            .chars()
            .next()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false);

    if valid_pkg {
        Ok(())
    } else {
        Err(CaramDesktopError::InvalidTarget {
            arg: arg.to_string(),
            reason: "Tên gói Debian chứa ký tự không hợp lệ",
        })
    }
}

fn summarize_stderr(stderr_bytes: &[u8], fallback: &str) -> String {
    let text = String::from_utf8_lossy(stderr_bytes);
    let tail: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .rev()
        .take(4)
        .collect();
    if tail.is_empty() {
        fallback.to_string()
    } else {
        tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
    }
}

pub fn run_privileged_apt_install(targets: &[String]) -> Result<(), CaramDesktopError> {
    if targets.is_empty() {
        return Ok(());
    }
    for t in targets {
        validate_apt_target(t)?;
    }

    // Chịu lỗi kho bên thứ ba tạm hỏng: ghi cảnh báo chứ không chặn cài đặt nếu cache vẫn cài được
    let update_out = Command::new("apt-get")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .args(["-o", "DPkg::Lock::Timeout=120", "update"])
        .output()
        .map_err(|e| CaramDesktopError::SpawnFailed {
            binary: "apt-get".to_string(),
            source: e,
        })?;
    if !update_out.status.success() {
        let hint = summarize_stderr(&update_out.stderr, "apt-get update gặp cảnh báo kho nguồn");
        eprintln!("[WARN] apt-get update: {}", hint);
    }

    let mut cmd = Command::new("apt-get");
    cmd.env("DEBIAN_FRONTEND", "noninteractive").args([
        "-o",
        "DPkg::Lock::Timeout=120",
        "install",
        "-y",
        "--no-install-recommends",
    ]);
    for t in targets {
        cmd.arg(t);
    }

    let output = cmd.output().map_err(|e| CaramDesktopError::SpawnFailed {
        binary: "apt-get".to_string(),
        source: e,
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CaramDesktopError::CommandFailed {
            action: "apt-get install".to_string(),
            detail: summarize_stderr(&output.stderr, "Cài đặt gói APT/.deb thất bại"),
        })
    }
}

pub fn run_privileged_apt_remove(packages: &[String]) -> Result<(), CaramDesktopError> {
    if packages.is_empty() {
        return Ok(());
    }
    let installed_now = snapshot_installed_debs();
    let mut existing: Vec<String> = Vec::new();
    for p in packages {
        if p.starts_with('/') {
            return Err(CaramDesktopError::InvalidTarget {
                arg: p.clone(),
                reason: "Không chấp nhận đường dẫn khi gỡ gói APT",
            });
        }
        validate_apt_target(p)?;
        if installed_now.contains(p) {
            existing.push(p.clone());
        }
    }

    if existing.is_empty() {
        return Ok(());
    }

    let mut cmd = Command::new("apt-get");
    cmd.env("DEBIAN_FRONTEND", "noninteractive").args([
        "-o",
        "DPkg::Lock::Timeout=120",
        "remove",
        "-y",
    ]);
    for p in &existing {
        cmd.arg(p);
    }

    let output = cmd.output().map_err(|e| CaramDesktopError::SpawnFailed {
        binary: "apt-get".to_string(),
        source: e,
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CaramDesktopError::CommandFailed {
            action: "apt-get remove".to_string(),
            detail: summarize_stderr(&output.stderr, "Gỡ gói APT thất bại"),
        })
    }
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

pub fn select_amd64_deb_url(assets_raw: &[(&str, &str)]) -> Option<String> {
    assets_raw
        .iter()
        .find(|(name, _)| {
            let lower = name.to_ascii_lowercase();
            lower.ends_with(".deb")
                && !lower.contains("dbgsym")
                && (lower.contains("amd64") || lower.contains("x86_64"))
        })
        .map(|(_, url)| (*url).to_string())
}

fn stream_download_deb(
    agent: &ureq::Agent,
    url: &str,
    app_id: &str,
    expected_sha256: Option<&str>,
) -> Result<PathBuf, CaramDesktopError> {
    let dir = private_download_dir()?;
    let safe_id: String = app_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let dst_path = dir.join(format!("caramos-{}.deb", safe_id));

    let resp = agent
        .get(url)
        .set("User-Agent", "caramos-app-center/2.0")
        .call()
        .map_err(|e| CaramDesktopError::Network {
            url: url.to_string(),
            reason: e.to_string(),
        })?;

    let mut reader = resp.into_reader();
    let mut tmp = NamedTempFile::new_in(&dir).map_err(|e| CaramDesktopError::Io {
        context: "Khởi tạo tệp đệm tải .deb".to_string(),
        source: e,
    })?;

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65_536];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| CaramDesktopError::Network {
                url: url.to_string(),
                reason: format!("Đứt luồng đọc dữ liệu: {}", e),
            })?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            hasher.update(chunk);
            tmp.write_all(chunk).map_err(|e| CaramDesktopError::Io {
                context: "Ghi dữ liệu .deb tạm".to_string(),
                source: e,
            })?;
        }
    }

    let actual_sha = hex::encode(hasher.finalize());
    if let Some(expected) = expected_sha256 {
        if actual_sha != expected.to_ascii_lowercase() {
            return Err(CaramDesktopError::ChecksumMismatch {
                file: dst_path.display().to_string(),
            });
        }
    }

    tmp.as_file()
        .sync_all()
        .map_err(|e| CaramDesktopError::Io {
            context: "Đồng bộ fsync tệp .deb".to_string(),
            source: e,
        })?;
    // Để `_apt` / `root` đọc được qua `pkexec`
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
        let _ = fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o644));
    }

    tmp.persist(&dst_path).map_err(|e| CaramDesktopError::Io {
        context: format!("Lưu tệp .deb tại '{}'", dst_path.display()),
        source: e.error,
    })?;
    Ok(dst_path)
}

pub fn download_github_deb_to_tmp(repo: &str, app_id: &str) -> Result<PathBuf, CaramDesktopError> {
    let api_url = format!("https://api.github.com/repos/{}/releases/latest", repo);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(300))
        .build();

    let release: GithubRelease = agent
        .get(&api_url)
        .set("User-Agent", "caramos-app-center/2.0")
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| CaramDesktopError::Network {
            url: api_url.clone(),
            reason: e.to_string(),
        })?
        .into_json()
        .map_err(|e| CaramDesktopError::Network {
            url: api_url.clone(),
            reason: format!("Dữ liệu GitHub Releases không hợp lệ: {}", e),
        })?;

    let asset = release
        .assets
        .iter()
        .find(|a| {
            let lower = a.name.to_ascii_lowercase();
            lower.ends_with(".deb")
                && !lower.contains("dbgsym")
                && (lower.contains("amd64") || lower.contains("x86_64"))
        })
        .ok_or_else(|| CaramDesktopError::Network {
            url: api_url,
            reason: format!(
                "Không tìm thấy gói .deb amd64 trong bản phát hành {}",
                release.tag_name
            ),
        })?;

    let expected_sha = asset
        .digest
        .as_deref()
        .and_then(|d| d.trim().strip_prefix("sha256:"));

    stream_download_deb(&agent, &asset.browser_download_url, app_id, expected_sha)
}

pub fn download_direct_deb_to_tmp(url: &str, app_id: &str) -> Result<PathBuf, CaramDesktopError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(300))
        .build();
    stream_download_deb(&agent, url, app_id, None)
}

pub fn ensure_flathub_remote() {
    let _ = Command::new("flatpak")
        .args([
            "remote-add",
            "--if-not-exists",
            "--user",
            "flathub",
            "https://dl.flathub.org/repo/flathub.flatpakrepo",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn install_flatpak_app(app_id: &str) -> Result<(), CaramDesktopError> {
    ensure_flathub_remote();
    let output = Command::new("flatpak")
        .args([
            "install",
            "-y",
            "--noninteractive",
            "--user",
            "flathub",
            app_id,
        ])
        .output()
        .map_err(|e| CaramDesktopError::SpawnFailed {
            binary: "flatpak".to_string(),
            source: e,
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CaramDesktopError::CommandFailed {
            action: format!("flatpak install {}", app_id),
            detail: summarize_stderr(&output.stderr, "Cài ứng dụng từ Flathub thất bại"),
        })
    }
}

pub fn remove_flatpak_app(app_id: &str) -> Result<(), CaramDesktopError> {
    let user_out = Command::new("flatpak")
        .args(["uninstall", "-y", "--noninteractive", "--user", app_id])
        .output()
        .map_err(|e| CaramDesktopError::SpawnFailed {
            binary: "flatpak".to_string(),
            source: e,
        })?;
    if user_out.status.success() {
        return Ok(());
    }

    let sys_out = Command::new("flatpak")
        .args(["uninstall", "-y", "--noninteractive", app_id])
        .output()
        .map_err(|e| CaramDesktopError::SpawnFailed {
            binary: "flatpak".to_string(),
            source: e,
        })?;
    if sys_out.status.success() {
        Ok(())
    } else {
        Err(CaramDesktopError::CommandFailed {
            action: format!("flatpak uninstall {}", app_id),
            detail: summarize_stderr(&sys_out.stderr, "Không thể gỡ ứng dụng Flatpak"),
        })
    }
}

pub fn invoke_self_pkexec(flag: &str, args: &[String]) -> Result<(), CaramDesktopError> {
    let exe = env::current_exe().map_err(|e| CaramDesktopError::Io {
        context: "Xác định đường dẫn tệp thực thi hiện tại".to_string(),
        source: e,
    })?;
    let mut cmd = Command::new("pkexec");
    cmd.arg(exe).arg(flag);
    for a in args {
        cmd.arg(a);
    }
    let output = cmd.output().map_err(|e| CaramDesktopError::SpawnFailed {
        binary: "pkexec".to_string(),
        source: e,
    })?;
    if output.status.success() {
        return Ok(());
    }
    // Mã thoát 126/127 của pkexec biểu thị người dùng bấm Hủy hoặc đóng hộp thoại mật khẩu
    if matches!(output.status.code(), Some(126) | Some(127)) {
        return Err(CaramDesktopError::AuthCancelled);
    }
    Err(CaramDesktopError::CommandFailed {
        action: format!("pkexec {}", flag),
        detail: summarize_stderr(&output.stderr, "Thao tác quản trị thất bại"),
    })
}

// ============================================================================
// 6. BỘ QUÉT PHẦN CỨNG & CHẨN ĐOÁN DRIVER CARAMOS (`caramos-driver-check`)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareCategory {
    Wifi,
    Ethernet,
    Graphics,
    Audio,
    Bluetooth,
    Webcam,
    Printer,
}

impl HardwareCategory {
    pub const ALL: [Self; 7] = [
        Self::Wifi,
        Self::Ethernet,
        Self::Graphics,
        Self::Audio,
        Self::Bluetooth,
        Self::Webcam,
        Self::Printer,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Wifi => "Wi-Fi",
            Self::Ethernet => "Mạng dây (Ethernet)",
            Self::Graphics => "Đồ họa (GPU)",
            Self::Audio => "Âm thanh",
            Self::Bluetooth => "Bluetooth",
            Self::Webcam => "Camera (Webcam)",
            Self::Printer => "Máy in",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverStatus {
    Working,
    MissingDriver,
    MissingFirmware,
    Blocked,
    NeedsReboot,
}

impl DriverStatus {
    pub fn status_text(self) -> &'static str {
        match self {
            Self::Working => "Đang hoạt động",
            Self::MissingDriver => "Chưa có driver",
            Self::MissingFirmware => "Thiếu firmware",
            Self::Blocked => "Bị chặn",
            Self::NeedsReboot => "Cần khởi động lại",
        }
    }

    pub fn is_issue(self) -> bool {
        !matches!(self, Self::Working)
    }
}

#[derive(Debug, Clone)]
pub struct HardwareFinding {
    pub category: HardwareCategory,
    pub device_name: String,
    pub driver_in_use: Option<String>,
    pub status: DriverStatus,
    pub explanation: String,
    pub recommended_packages: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DriverScanReport {
    pub secure_boot_enabled: bool,
    pub nomodeset_active: bool,
    pub root_free_mb: Option<u64>,
    pub findings: Vec<HardwareFinding>,
    pub firmware_errors: Vec<String>,
}

impl DriverScanReport {
    pub fn issue_count(&self) -> usize {
        self.findings.iter().filter(|f| f.status.is_issue()).count()
    }

    pub fn has_wifi_issue(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.category == HardwareCategory::Wifi && f.status.is_issue())
    }

    pub fn all_recommended_packages(&self) -> Vec<String> {
        let mut set = BTreeSet::new();
        for f in &self.findings {
            if f.status.is_issue() {
                for p in &f.recommended_packages {
                    set.insert(p.clone());
                }
            }
        }
        set.into_iter().collect()
    }

    pub fn requires_large_disk_space(&self) -> bool {
        self.all_recommended_packages()
            .iter()
            .any(|p| p == "nvidia-driver")
    }

    pub fn has_enough_disk_for_fix(&self) -> bool {
        let min_mb = if self.requires_large_disk_space() {
            2500
        } else {
            300
        };
        self.root_free_mb.map(|free| free >= min_mb).unwrap_or(true)
    }

    pub fn fingerprint(&self) -> String {
        let mut lines: Vec<String> = self
            .findings
            .iter()
            .map(|f| format!("{:?}|{}|{:?}", f.category, f.device_name, f.status))
            .collect();
        lines.sort();
        let joined = lines.join("\n");
        let digest = Sha256::digest(joined.as_bytes());
        hex::encode(digest)
    }

    pub fn format_text_report(&self) -> String {
        let mut out = String::from("=== CARAMOS 2.0 HARDWARE & DRIVER REPORT ===\n");
        out.push_str(&format!(
            "Secure Boot: {}\nTham số nomodeset: {}\nDung lượng trống '/': {}\nTổng vấn đề: {}\n\n",
            if self.secure_boot_enabled {
                "BẬT"
            } else {
                "TẮT / Không xác định"
            },
            if self.nomodeset_active {
                "CÓ"
            } else {
                "Không"
            },
            self.root_free_mb
                .map(|m| format!("{} MB", m))
                .unwrap_or_else(|| "Không xác định".to_string()),
            self.issue_count()
        ));
        for f in &self.findings {
            out.push_str(&format!(
                "[{:?}] {} | Trạng thái: {} | Driver: {}\n  Giải thích: {}\n",
                f.category,
                f.device_name,
                f.status.status_text(),
                f.driver_in_use.as_deref().unwrap_or("Chưa nạp"),
                f.explanation
            ));
            if !f.recommended_packages.is_empty() {
                out.push_str(&format!(
                    "  Gói đề xuất: {}\n",
                    f.recommended_packages.join(", ")
                ));
            }
        }
        if !self.firmware_errors.is_empty() {
            out.push_str("\n--- Cảnh báo Firmware từ Kernel ---\n");
            for err in &self.firmware_errors {
                out.push_str(err);
                out.push('\n');
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct PciDeviceInfo {
    pub slot: String,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u32,
    pub driver: Option<String>,
    pub human_name: String,
}

fn parse_hex_u32(text: &str) -> Option<u32> {
    let trimmed = text.trim();
    let hex_str = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    u32::from_str_radix(hex_str, 16).ok()
}

pub fn detect_secure_boot_enabled() -> bool {
    let dir = Path::new("/sys/firmware/efi/efivars");
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("SecureBoot-") {
            if let Ok(bytes) = fs::read(entry.path()) {
                return bytes.last().copied() == Some(1);
            }
        }
    }
    false
}

pub fn detect_nomodeset_in_cmdline(cmdline: &str) -> bool {
    cmdline.split_whitespace().any(|tok| tok == "nomodeset")
}

fn detect_root_free_mb() -> Option<u64> {
    let out = Command::new("df").args(["-PB1", "/"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let data_line = text.lines().nth(1)?;
    let avail_bytes: u64 = data_line.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_bytes / (1024 * 1024))
}

fn collect_blacklisted_modules() -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    let Ok(entries) = fs::read_dir("/etc/modprobe.d") else {
        return set;
    };
    for entry in entries.flatten() {
        if let Ok(text) = fs::read_to_string(entry.path()) {
            for line in text.lines() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix("blacklist ") {
                    if let Some(module) = rest.split_whitespace().next() {
                        set.insert(module.to_string());
                    }
                }
            }
        }
    }
    set
}

fn collect_kernel_firmware_errors() -> Vec<String> {
    let output = Command::new("journalctl")
        .args(["-k", "-b", "--no-pager", "-q"])
        .output()
        .or_else(|_| Command::new("dmesg").output());

    let Ok(out) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut errors = Vec::new();
    for line in text.lines() {
        if line.contains("Direct firmware load for") && line.contains("failed") {
            errors.push(line.trim().to_string());
            if errors.len() >= 10 {
                break;
            }
        }
    }
    errors
}

fn lspci_name_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(out) = Command::new("lspci").args(["-D", "-mm"]).output() else {
        return map;
    };
    if !out.status.success() {
        return map;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some((slot, rest)) = line.split_once(' ') {
            let quoted: Vec<&str> = rest.split('"').collect();
            let vendor = quoted.get(3).copied().unwrap_or("");
            let device = quoted.get(5).copied().unwrap_or("");
            let full = format!("{} {}", vendor, device).trim().to_string();
            if !full.is_empty() {
                map.insert(slot.to_string(), full);
            }
        }
    }
    map
}

fn read_sysfs_pci_devices() -> Vec<PciDeviceInfo> {
    let names = lspci_name_map();
    let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") else {
        return Vec::new();
    };

    let mut list = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let slot = entry.file_name().to_string_lossy().to_string();
        let Some(vendor) = fs::read_to_string(path.join("vendor"))
            .ok()
            .and_then(|s| parse_hex_u32(&s))
            .and_then(|v| u16::try_from(v).ok())
        else {
            continue;
        };
        let Some(device) = fs::read_to_string(path.join("device"))
            .ok()
            .and_then(|s| parse_hex_u32(&s))
            .and_then(|v| u16::try_from(v).ok())
        else {
            continue;
        };
        let Some(class_code) = fs::read_to_string(path.join("class"))
            .ok()
            .and_then(|s| parse_hex_u32(&s))
        else {
            continue;
        };
        let driver = fs::read_link(path.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));
        let human_name = names
            .get(&slot)
            .cloned()
            .unwrap_or_else(|| format!("Thiết bị PCI {:04x}:{:04x}", vendor, device));

        list.push(PciDeviceInfo {
            slot,
            vendor_id: vendor,
            device_id: device,
            class_code,
            driver,
            human_name,
        });
    }
    list
}

fn has_firmware_error_matching(firmware_errors: &[String], keywords: &[&str]) -> bool {
    firmware_errors.iter().any(|line| {
        let lower = line.to_ascii_lowercase();
        keywords.iter().any(|kw| lower.contains(kw))
    })
}

pub fn classify_pci_device(
    dev: &PciDeviceInfo,
    blacklisted: &BTreeSet<String>,
    firmware_errors: &[String],
    nomodeset_active: bool,
) -> Option<HardwareFinding> {
    let base_class = (dev.class_code >> 16) & 0xff;
    let sub_class = (dev.class_code >> 8) & 0xff;

    // 1. Wi-Fi (0x0280)
    if base_class == 0x02 && sub_class == 0x80 {
        let (pkgs, fw_keys, expected_mods): (Vec<&str>, &[&str], &[&str]) = match dev.vendor_id {
            0x8086 => (vec!["firmware-iwlwifi"], &["iwlwifi"], &["iwlwifi"]),
            0x10ec => (
                vec!["firmware-realtek"],
                &["rtw", "rtlwifi", "rtl_nic"],
                &["rtw88_pci", "rtw89_pci", "r8169"],
            ),
            0x14e4 => (
                vec!["firmware-brcm80211", "broadcom-sta-dkms"],
                &["brcm"],
                &["wl", "brcmfmac", "b43"],
            ),
            0x168c | 0x17cb => (
                vec!["firmware-atheros"],
                &["ath"],
                &["ath9k", "ath10k_pci", "ath11k_pci"],
            ),
            0x14c3 => (
                vec!["firmware-misc-nonfree"],
                &["mediatek", "mt79"],
                &["mt7921e"],
            ),
            _ => (vec!["firmware-misc-nonfree"], &["wifi", "wlan"], &[]),
        };

        let (status, explanation, rec) =
            if expected_mods.iter().any(|m| blacklisted.contains(*m)) && dev.driver.is_none() {
                (
                    DriverStatus::Blocked,
                    "Trình điều khiển Wi-Fi đang bị chặn trong /etc/modprobe.d.".to_string(),
                    Vec::new(),
                )
            } else if has_firmware_error_matching(firmware_errors, fw_keys) {
                (
                    DriverStatus::MissingFirmware,
                    "Nhân Linux báo thiếu tệp firmware cho card Wi-Fi này.".to_string(),
                    pkgs.into_iter().map(ToString::to_string).collect(),
                )
            } else if dev.driver.is_some() {
                (
                    DriverStatus::Working,
                    "Card Wi-Fi đã nhận trình điều khiển ổn định.".to_string(),
                    Vec::new(),
                )
            } else {
                (
                    DriverStatus::MissingDriver,
                    "Card Wi-Fi chưa có trình điều khiển hoạt động.".to_string(),
                    pkgs.into_iter().map(ToString::to_string).collect(),
                )
            };

        return Some(HardwareFinding {
            category: HardwareCategory::Wifi,
            device_name: dev.human_name.clone(),
            driver_in_use: dev.driver.clone(),
            status,
            explanation,
            recommended_packages: rec,
        });
    }

    // 2. Ethernet (0x0200)
    if base_class == 0x02 && sub_class == 0x00 {
        let (status, explanation, rec) = if dev.driver.is_some() {
            (
                DriverStatus::Working,
                "Cổng mạng dây đã sẵn sàng hoạt động.".to_string(),
                Vec::new(),
            )
        } else {
            let pkgs = match dev.vendor_id {
                0x10ec => vec!["firmware-realtek".to_string()],
                0x14e4 => vec!["firmware-bnx2".to_string()],
                _ => vec!["firmware-misc-nonfree".to_string()],
            };
            (
                DriverStatus::MissingDriver,
                "Cổng mạng dây chưa nạp được trình điều khiển.".to_string(),
                pkgs,
            )
        };

        return Some(HardwareFinding {
            category: HardwareCategory::Ethernet,
            device_name: dev.human_name.clone(),
            driver_in_use: dev.driver.clone(),
            status,
            explanation,
            recommended_packages: rec,
        });
    }

    // 3. Graphics / GPU (0x03xx)
    if base_class == 0x03 {
        let drv = dev.driver.as_deref().unwrap_or("");
        if nomodeset_active && !matches!(drv, "nvidia" | "amdgpu" | "i915" | "xe" | "radeon") {
            return Some(HardwareFinding {
                category: HardwareCategory::Graphics,
                device_name: dev.human_name.clone(),
                driver_in_use: dev.driver.clone(),
                status: DriverStatus::Blocked,
                explanation:
                    "Hệ thống đang khởi động với tham số 'nomodeset', tắt tăng tốc đồ họa phần cứng."
                        .to_string(),
                recommended_packages: Vec::new(),
            });
        }

        let (status, explanation, rec) = match dev.vendor_id {
            0x10de => {
                if drv == "nvidia" {
                    (
                        DriverStatus::Working,
                        "Card đồ họa NVIDIA đang dùng trình điều khiển chính hãng.".to_string(),
                        Vec::new(),
                    )
                } else {
                    (
                        DriverStatus::MissingDriver,
                        "Đang chạy đồ họa cơ bản; cài driver NVIDIA chính hãng để mượt video, xuất HDMI và tiết kiệm điện."
                            .to_string(),
                        vec!["nvidia-driver".to_string(), "firmware-misc-nonfree".to_string()],
                    )
                }
            }
            0x1002 => {
                if matches!(drv, "amdgpu" | "radeon")
                    && !has_firmware_error_matching(firmware_errors, &["amdgpu", "radeon"])
                {
                    (
                        DriverStatus::Working,
                        "Card đồ họa AMD đang chạy tăng tốc phần cứng bình thường.".to_string(),
                        Vec::new(),
                    )
                } else {
                    (
                        DriverStatus::MissingFirmware,
                        "Cần gói firmware-amd-graphics cho card đồ họa AMD.".to_string(),
                        vec!["firmware-amd-graphics".to_string()],
                    )
                }
            }
            0x8086 => {
                if matches!(drv, "i915" | "xe")
                    && !has_firmware_error_matching(firmware_errors, &["i915", "xe/"])
                {
                    (
                        DriverStatus::Working,
                        "Đồ họa tích hợp Intel đang hoạt động bình thường.".to_string(),
                        Vec::new(),
                    )
                } else {
                    (
                        DriverStatus::MissingFirmware,
                        "Thiếu firmware đồ họa Intel (GuC/HuC).".to_string(),
                        vec!["firmware-intel-graphics".to_string()],
                    )
                }
            }
            _ => (
                if dev.driver.is_some() {
                    DriverStatus::Working
                } else {
                    DriverStatus::MissingDriver
                },
                if dev.driver.is_some() {
                    "Thiết bị hiển thị đã nhận trình điều khiển.".to_string()
                } else {
                    "Thiết bị hiển thị chưa có trình điều khiển riêng.".to_string()
                },
                Vec::new(),
            ),
        };

        return Some(HardwareFinding {
            category: HardwareCategory::Graphics,
            device_name: dev.human_name.clone(),
            driver_in_use: dev.driver.clone(),
            status,
            explanation,
            recommended_packages: rec,
        });
    }

    // 4. Audio (0x0403 / 0x0401)
    if base_class == 0x04 && matches!(sub_class, 0x01 | 0x03) {
        let sof_missing = has_firmware_error_matching(firmware_errors, &["sof-", "intel/dsp_fw"]);
        let (status, explanation, rec) = if sof_missing || dev.driver.is_none() {
            (
                DriverStatus::MissingFirmware,
                "Thiếu firmware âm thanh (Intel SOF/DSP) cho loa hoặc micro tích hợp.".to_string(),
                vec!["firmware-sof-signed".to_string()],
            )
        } else {
            (
                DriverStatus::Working,
                "Bộ điều khiển âm thanh đang hoạt động bình thường.".to_string(),
                Vec::new(),
            )
        };

        return Some(HardwareFinding {
            category: HardwareCategory::Audio,
            device_name: dev.human_name.clone(),
            driver_in_use: dev.driver.clone(),
            status,
            explanation,
            recommended_packages: rec,
        });
    }

    None
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn has_any_entry_starting_with(dir: &str, prefix: &str) -> bool {
    fs::read_dir(dir)
        .ok()
        .map(|rd| {
            rd.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(prefix))
        })
        .unwrap_or(false)
}

fn canon_capt_model_name(product_id: &str) -> Option<&'static str> {
    match product_id {
        "2676" => Some("Canon LBP2900 (CAPT)"),
        "266a" => Some("Canon LBP3000 (CAPT)"),
        "26da" => Some("Canon LBP3010 / LBP3018 / LBP3050 (CAPT)"),
        "26db" => Some("Canon LBP3100 / LBP3108 / LBP3150 (CAPT)"),
        "262b" => Some("Canon LBP1120 (CAPT)"),
        "2677" => Some("Canon LBP3200 (CAPT)"),
        "271a" => Some("Canon LBP6000 / LBP6018 (CAPT)"),
        "2771" => Some("Canon LBP6020 (CAPT)"),
        _ => None,
    }
}

fn detect_usb_peripherals(
    installed_debs: &HashSet<String>,
    firmware_errors: &[String],
) -> Vec<HardwareFinding> {
    let mut findings = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/bus/usb/devices") else {
        return findings;
    };

    let mut bt_found = false;
    let mut cam_found = false;
    let mut seen_printers = HashSet::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains(':') || name.starts_with("usb") {
            continue;
        }
        let vendor = read_trimmed(&path.join("idVendor")).unwrap_or_default();
        let product = read_trimmed(&path.join("idProduct")).unwrap_or_default();
        let dev_class = read_trimmed(&path.join("bDeviceClass")).unwrap_or_default();
        let product_label = read_trimmed(&path.join("product"))
            .unwrap_or_else(|| format!("Thiết bị USB {}:{}", vendor, product));

        // Máy in Canon CAPT (LBP2900 v.v.)
        if vendor.eq_ignore_ascii_case("04a9") {
            let pid_lower = product.to_ascii_lowercase();
            if let Some(model) = canon_capt_model_name(&pid_lower) {
                if seen_printers.insert(pid_lower) {
                    let capt_ready = installed_debs.contains("printer-driver-captdriver")
                        || installed_debs.contains("cndrvcups-capt");
                    let (status, explanation, rec) = if capt_ready {
                        (
                            DriverStatus::Working,
                            "Đã có gói trình điều khiển Canon CAPT trên hệ thống.".to_string(),
                            Vec::new(),
                        )
                    } else {
                        (
                            DriverStatus::MissingDriver,
                            "Máy in Canon dùng giao thức CAPT riêng. Cần gói captdriver hoặc driver Canon CAPT để in ổn định."
                                .to_string(),
                            Vec::new(),
                        )
                    };
                    findings.push(HardwareFinding {
                        category: HardwareCategory::Printer,
                        device_name: model.to_string(),
                        driver_in_use: if capt_ready {
                            Some("captdriver".to_string())
                        } else {
                            None
                        },
                        status,
                        explanation,
                        recommended_packages: rec,
                    });
                }
            }
        }

        // Bluetooth USB (class e0)
        if !bt_found && dev_class.eq_ignore_ascii_case("e0") {
            bt_found = true;
            let hci_ok = has_any_entry_starting_with("/sys/class/bluetooth", "hci");
            let bt_fw_err =
                has_firmware_error_matching(firmware_errors, &["bluetooth", "rtl_bt", "intel/ibt"]);
            let (status, explanation, rec) = if hci_ok && !bt_fw_err {
                (
                    DriverStatus::Working,
                    "Bộ thu phát Bluetooth đang hoạt động bình thường.".to_string(),
                    Vec::new(),
                )
            } else {
                (
                    DriverStatus::MissingFirmware,
                    "Bluetooth phát hiện lỗi hoặc thiếu firmware.".to_string(),
                    vec![
                        "firmware-realtek".to_string(),
                        "firmware-iwlwifi".to_string(),
                        "firmware-atheros".to_string(),
                    ],
                )
            };
            findings.push(HardwareFinding {
                category: HardwareCategory::Bluetooth,
                device_name: product_label.clone(),
                driver_in_use: if hci_ok {
                    Some("btusb".to_string())
                } else {
                    None
                },
                status,
                explanation,
                recommended_packages: rec,
            });
        }

        // Webcam USB (kiểm tra interface class 0e)
        if !cam_found {
            let is_video = fs::read_dir(&path)
                .ok()
                .map(|sub| {
                    sub.flatten().any(|iface| {
                        read_trimmed(&iface.path().join("bInterfaceClass"))
                            .map(|c| c.eq_ignore_ascii_case("0e"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            if is_video {
                cam_found = true;
                let video_dev_ok = has_any_entry_starting_with("/dev", "video");
                findings.push(HardwareFinding {
                    category: HardwareCategory::Webcam,
                    device_name: product_label,
                    driver_in_use: if video_dev_ok {
                        Some("uvcvideo".to_string())
                    } else {
                        None
                    },
                    status: if video_dev_ok {
                        DriverStatus::Working
                    } else {
                        DriverStatus::MissingDriver
                    },
                    explanation: if video_dev_ok {
                        "Camera chuẩn UVC đã sẵn sàng cho họp trực tuyến.".to_string()
                    } else {
                        "Không tìm thấy thiết bị /dev/video cho camera này.".to_string()
                    },
                    recommended_packages: Vec::new(),
                });
            }
        }
    }

    findings
}

pub fn run_full_hardware_scan() -> DriverScanReport {
    let secure_boot_enabled = detect_secure_boot_enabled();
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let nomodeset_active = detect_nomodeset_in_cmdline(&cmdline);
    let root_free_mb = detect_root_free_mb();
    let blacklisted = collect_blacklisted_modules();
    let firmware_errors = collect_kernel_firmware_errors();
    let installed_debs = snapshot_installed_debs();

    let mut findings = Vec::new();
    for dev in read_sysfs_pci_devices() {
        if let Some(finding) =
            classify_pci_device(&dev, &blacklisted, &firmware_errors, nomodeset_active)
        {
            findings.push(finding);
        }
    }
    findings.extend(detect_usb_peripherals(&installed_debs, &firmware_errors));

    DriverScanReport {
        secure_boot_enabled,
        nomodeset_active,
        root_free_mb,
        findings,
        firmware_errors,
    }
}

fn fingerprint_state_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("caramos").join("driver-fingerprint.sha256"))
}

pub fn load_saved_fingerprint() -> Option<String> {
    let path = fingerprint_state_path()?;
    read_trimmed(&path)
}

pub fn save_fingerprint(fp: &str) -> Result<(), CaramDesktopError> {
    let Some(path) = fingerprint_state_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| CaramDesktopError::Io {
            context: format!("Tạo thư mục '{}'", parent.display()),
            source: e,
        })?;
        let mut tmp = NamedTempFile::new_in(parent).map_err(|e| CaramDesktopError::Io {
            context: "Tạo tệp tạm lưu dấu vân tay phần cứng".to_string(),
            source: e,
        })?;
        writeln!(tmp, "{}", fp).map_err(|e| CaramDesktopError::Io {
            context: "Ghi dấu vân tay phần cứng".to_string(),
            source: e,
        })?;
        tmp.as_file()
            .sync_all()
            .map_err(|e| CaramDesktopError::Io {
                context: "fsync dấu vân tay phần cứng".to_string(),
                source: e,
            })?;
        tmp.persist(&path).map_err(|e| CaramDesktopError::Io {
            context: format!("Lưu '{}'", path.display()),
            source: e.error,
        })?;
    }
    Ok(())
}

pub fn open_external_url(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn parse_install_link_accepts_valid_flathub_id_and_strips_query() {
        let id = parse_install_link("caramos-app://install/org.videolan.VLC?from=caramos").unwrap();
        assert_eq!(id, "org.videolan.VLC");
    }

    #[test]
    fn parse_install_link_rejects_malicious_inputs() {
        assert!(parse_install_link("caramos-app://install/org.bad;rm -rf /").is_err());
        assert!(parse_install_link("caramos-app://install/notenough").is_err());
        assert!(parse_install_link("https://flathub.org/org.videolan.VLC").is_err());
    }

    #[test]
    fn validate_apt_target_rejects_traversal_and_shell_chars() {
        assert!(validate_apt_target("libreoffice-l10n-vi").is_ok());
        assert!(validate_apt_target("g++").is_ok());
        assert!(validate_apt_target("vlc;rm -rf /").is_err());
        assert!(validate_apt_target("/etc/passwd").is_err());
        assert!(validate_apt_target("/tmp/caramos-../evil.deb").is_err());
    }

    #[test]
    fn select_amd64_deb_skips_dbgsym() {
        let assets = [
            ("zalo-dbgsym_1.0_amd64.deb", "https://ex/dbg.deb"),
            ("zalo_1.0_arm64.deb", "https://ex/arm.deb"),
            ("zalo_1.0_amd64.deb", "https://ex/amd64.deb"),
        ];
        assert_eq!(
            select_amd64_deb_url(&assets).as_deref(),
            Some("https://ex/amd64.deb")
        );
    }

    #[test]
    fn classify_pci_detects_nvidia_and_nomodeset() {
        let dev = PciDeviceInfo {
            slot: "0000:01:00.0".to_string(),
            vendor_id: 0x10de,
            device_id: 0x2504,
            class_code: 0x030000,
            driver: Some("nouveau".to_string()),
            human_name: "NVIDIA GeForce RTX 3060".to_string(),
        };
        let finding = classify_pci_device(&dev, &BTreeSet::new(), &[], false).unwrap();
        assert_eq!(finding.status, DriverStatus::MissingDriver);
        assert!(finding
            .recommended_packages
            .contains(&"nvidia-driver".to_string()));

        let blocked = classify_pci_device(&dev, &BTreeSet::new(), &[], true).unwrap();
        assert_eq!(blocked.status, DriverStatus::Blocked);
    }

    #[test]
    fn catalog_has_unique_ids_and_all_six_categories() {
        let mut ids = HashSet::new();
        let mut cats = HashSet::new();
        for entry in CARAMOS_APP_CATALOG {
            assert!(ids.insert(entry.id), "Trùng ID ứng dụng: {}", entry.id);
            assert!(!entry.pros.is_empty());
            cats.insert(entry.category);
        }
        assert_eq!(cats.len(), AppCategory::ALL.len());
    }
}

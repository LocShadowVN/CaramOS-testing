# CaramOS 2.0 — Unified Rust Workspace Makefile (Debian 13 + Calamares + GTK4 Rust Apps)
CARGO ?= cargo
CTL_BIN := target/release/caramos-ctl
UI_BIN := target/release/caramos-ui
APP_CENTER_BIN := target/release/caramos-app-center
DRIVER_CHECK_BIN := target/release/caramos-driver-check
STAGE_BINS := --stage-bin $(CTL_BIN) --stage-bin $(UI_BIN) --stage-bin $(APP_CENTER_BIN) --stage-bin $(DRIVER_CHECK_BIN)
WORK_DIR ?= ./build
UI_OVERLAY_DIR := $(WORK_DIR)/ui_overlay
WALLPAPER_DIR ?= ./config/includes.chroot/usr/share/backgrounds
ASSETS_DIR ?= ./assets

.PHONY: all check test build-bin stage-ui prune dry-run build release clean distclean

all: check test build-bin

check:
	$(CARGO) clippy --workspace --all-targets -- -D warnings -D clippy::unwrap_used -D clippy::expect_used -D clippy::panic

test:
	$(CARGO) test --workspace

build-bin:
	$(CARGO) build --workspace --release

stage-ui: build-bin
	$(UI_BIN) branding \
		--output-root $(UI_OVERLAY_DIR) \
		--wallpaper-search-dir $(WALLPAPER_DIR) \
		--assets-dir $(ASSETS_DIR)

prune: build-bin
	$(CTL_BIN) prune --repo-root . --execute

dry-run: stage-ui
	$(CTL_BIN) build \
		--dry-run \
		--work-dir $(WORK_DIR)_dryrun \
		--extra-overlay $(UI_OVERLAY_DIR) \
		$(STAGE_BINS)

build: stage-ui
	sudo env "GITHUB_TOKEN=$${GITHUB_TOKEN:-}" $(CTL_BIN) build \
		--work-dir $(WORK_DIR) \
		--extra-overlay $(UI_OVERLAY_DIR) \
		$(STAGE_BINS)

release: stage-ui
	sudo env "GITHUB_TOKEN=$${GITHUB_TOKEN:-}" $(CTL_BIN) build \
		--release \
		--work-dir $(WORK_DIR) \
		--extra-overlay $(UI_OVERLAY_DIR) \
		$(STAGE_BINS)

clean:
	sudo rm -rf $(WORK_DIR) $(WORK_DIR)_dryrun

distclean: clean
	$(CARGO) clean

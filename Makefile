# Summer Makefile (cross-platform)
#
# `make` builds (debug); `make dev` launches the live `summer` CLI (needs
# ANTHROPIC_API_KEY); `make check` runs the local gate the plan calls "the
# checks" (run, not scheduled: each is a file anyone can run); `make help`
# lists everything.
#
# `install` builds first, so a single command suffices.
#
# Linux install (system-wide, via a .deb; needs root):
#     make install
# Linux install (current user only, no root needed; ~/.local must be on PATH):
#     make install-files PREFIX=$(HOME)/.local
# `install` always packages/installs system-wide under /usr via dpkg, so it
# does not accept PREFIX; only `install-files` does.
#
# Windows install (per-user; needs Inno Setup 6, https://jrsoftware.org/isdl.php):
#     make install
# Builds target\installer\summer-setup-<version>.exe and runs it silently:
# installs summer.exe and adds its folder to the user PATH. Override the
# location with WINPREFIX=...
#
# Windows install without Inno Setup (per-user, no admin; needs PowerShell):
#     make install-files
# Same effect as `make install` (exe + PATH) via a plain script instead of a
# packaged installer, so it does not appear in Add/Remove Programs.
# `make uninstall` reverses either one.
#
# Other targets: installer, uninstall, clean

CARGO ?= cargo
# Extra arguments passed to the running CLI, e.g. `make dev ARGS=--sandbox`.
ARGS ?=

.DEFAULT_GOAL := build

# The task set `make bench` scores. Override to run another: `make bench SET=...`.
SET ?= crates/bench/tasks/starter.json
# Runs per task. Five separates "always fails" from "always passes"; it does
# not separate 60% from 80%, which is what the interval beside every rate is for.
RUNS ?= 5

.PHONY: build build-release dev bench ablate check check-platforms \
        install install-files installer stage deb uninstall clean help

build: ## Build the whole workspace (debug)
	$(CARGO) build --workspace

# What the installer stages/packages: the release `summer` binary at
# target/release/summer.
build-release: ## Build the whole workspace (release; what the installer packages)
	$(CARGO) build --workspace --release

dev: ## Launch the live summer CLI (needs ANTHROPIC_API_KEY); pass ARGS=...
	$(CARGO) run -p summer-code -- $(ARGS)

bench: ## Score a task set against the live model (needs ANTHROPIC_API_KEY); SET=, RUNS=
	$(CARGO) run --release -p summer-bench -- $(SET) --runs $(RUNS) $(ARGS)

# The ablation matrix: the all-on control plus one arm per quality mechanism
# disabled, over the same task set, rendered side by side. Needs a provider
# credential in the environment (see `bench --help`), and costs as much as five
# scored runs of the set.
ablate: ## Run the ablation matrix (needs a provider credential); SET=, RUNS=
	$(CARGO) run --release -p summer-bench -- ablate $(SET) --runs $(RUNS) $(ARGS)

check: ## The local gate: format, lint, doc, test, contract-crate assertion
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
# Doc warnings are what nothing else catches: a broken intra-doc link compiles,
# tests clean, and lints clean, then renders as plain text on docs.rs. Denied
# here so a link to an item that was renamed or made private fails the gate.
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --no-deps
	$(CARGO) test --workspace
	./scripts/check-contract-deps.sh

check-platforms: ## Compile-check against Windows and macOS (needs `rustup target add`)
	./scripts/check-platforms.sh

clean:
	$(CARGO) clean
	rm -rf target/deb target/installer

ifeq ($(OS),Windows_NT)
# ============================ Windows =================================
# Install logic lives in scripts/windows-*.ps1 so a single recipe survives
# whichever shell mingw32-make picks (cmd.exe or sh).

WINPREFIX ?= $(LOCALAPPDATA)\Programs\Summer
PS        := powershell -NoProfile -ExecutionPolicy Bypass

# Builds the packaged installer and runs it silently, so summer ends up
# with a real Add/Remove Programs entry. Requires Inno Setup 6 (ISCC.exe on
# PATH, or installed to the default location): https://jrsoftware.org/isdl.php
install: build-release
	$(PS) -File scripts/windows-installer.ps1 -Run

# The plain script-driven install: same effect as `make install` (exe + PATH)
# without needing Inno Setup, but it won't show up in Add/Remove Programs.
install-files: build-release
	$(PS) -File scripts/windows-setup.ps1 -Action install -Prefix "$(WINPREFIX)"

# Reverses whichever of `install` / `install-files` was used: if the packaged
# installer's uninstaller is present it's used (matching how Add/Remove
# Programs would uninstall), otherwise this falls back to the plain script.
uninstall:
	$(PS) -File scripts/windows-uninstall.ps1 -Prefix "$(WINPREFIX)"

# Just builds target\installer\summer-setup-<version>.exe without running
# it, e.g. for distributing to users who don't have Rust or a repo checkout.
installer: build-release
	$(PS) -File scripts/windows-installer.ps1

else
# ============================= Linux =================================

PREFIX     ?= /usr/local
DESTDIR    ?=
BINDIR     := $(DESTDIR)$(PREFIX)/bin
BIN        := target/release/summer

# Prepend sudo unless running as root, or unless the install destination is
# writable without it (e.g. PREFIX=$(HOME)/.local). Override with SUDO= to
# disable outright (needed when sudo itself can't escalate, e.g. containers
# run with --security-opt=no-new-privileges).
UID_ := $(shell id -u)
SUDO := $(if $(filter 0,$(UID_)),,$(if $(filter $(HOME)/%,$(DESTDIR)$(PREFIX)),,sudo))

# --- Debian package (.deb) ---------------------------------------------------
# `make install` builds a .deb and installs it with `dpkg -i`, so the package
# manager owns the files and `dpkg -r summer` removes them cleanly. The
# deb follows FHS layout under /usr (not /usr/local). For a non-package,
# prefix-overridable file copy, use `make install-files`.

STAGE      := target/deb/root
DEBDIR     := target/deb
VERSION    := $(shell grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
ARCH       := $(shell dpkg --print-architecture 2>/dev/null || echo amd64)
DESC       := $(shell grep -m1 '^description' crates/cli/Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
M_NAME     := $(shell git config user.name 2>/dev/null || echo Summer)
M_MAIL     := $(shell git config user.email 2>/dev/null || echo summer@localhost)
DEB        := $(DEBDIR)/summer_$(VERSION)_$(ARCH).deb

# Stage the install tree into $(STAGE) under prefix /usr (FHS for packages).
# No sudo: everything lands under the user-owned target/ dir.
stage: build-release
	rm -rf $(STAGE) 2>/dev/null || sudo rm -rf $(STAGE)
	install -Dm755 $(BIN) $(STAGE)/usr/bin/summer
	@mkdir -p $(STAGE)/DEBIAN
	@printf 'Package: summer\nVersion: %s\nSection: devel\nPriority: optional\nArchitecture: %s\nMaintainer: %s <%s>\nDescription: %s\n An agentic coding system: kernel-enforced sandboxing, an event-sourced\n journal, a policy engine, batched tool dispatch, and a Ratatui TUI REPL.\n' \
		"$(VERSION)" "$(ARCH)" "$(M_NAME)" "$(M_MAIL)" "$(DESC)" \
		> $(STAGE)/DEBIAN/control

# Build the .deb from the staged tree. --root-owner-group sets root:root on
# every file so the package installs with correct ownership.
deb: stage
	@mkdir -p $(DEBDIR)
	dpkg-deb --build --root-owner-group $(STAGE) $(DEB)
	@echo "built $(DEB)"

# Install by building the .deb and handing it to dpkg.
install: deb
	$(SUDO) dpkg -i $(DEB)
	@echo "installed summer $(VERSION) via $(DEB)"

# Exact reverse of `make install`: hand removal to dpkg, which owns every
# file the deb laid down. `apt remove summer` does the same thing. The
# manual sweep is a fallback for the `install-files` layout (which dpkg
# doesn't track) and a no-op when the package was just removed.
uninstall:
	@if dpkg -s summer >/dev/null 2>&1; then \
		$(SUDO) dpkg -r summer; \
	else \
		echo "summer not installed as a package; removing files manually"; \
		$(foreach d,/usr /usr/local,$(SUDO) rm -f $(d)/bin/summer;) \
	fi
	@echo "uninstalled summer"

# --- Non-package install (plain file copy, honours PREFIX/DESTDIR) ---------

install-files: build-release
	$(SUDO) install -Dm755 $(BIN) $(BINDIR)/summer
	@echo "installed summer to $(PREFIX)"

endif

help: ## List the targets
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "} {printf "  \033[36m%-9s\033[0m %s\n", $$1, $$2}'

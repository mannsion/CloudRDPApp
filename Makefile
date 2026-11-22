# Build and install CloudRDPApp for the current user (no root needed).
#
#   make build       release build
#   make install     install into ~/.local (binary, desktop file, icons)
#   make uninstall   remove what install put there
#   make test        unit and integration tests
#   make check       clippy (-D warnings) and tests
#
# PREFIX and DESTDIR can be overridden, e.g.:
#   make install PREFIX=$HOME/opt/cloudrdpapp
#   make install PREFIX=/usr DESTDIR=/tmp/pkgroot

PREFIX  ?= $(HOME)/.local
DESTDIR ?=
BINDIR  := $(PREFIX)/bin
DATADIR := $(PREFIX)/share
APP_ID  := io.github.mannsion.CloudRDPApp
BIN     := cloudrdpapp
CARGO   ?= cargo

.PHONY: build install uninstall test check

build:
	$(CARGO) build --release --locked

# The desktop file says plain "cloudrdpapp"; install points it at the
# installed binary in case $(BINDIR) is not on the session's PATH.
# --register-url-handler writes the hidden ms-appx-web: handler entry
# (Exec=<binary> --auth-callback %u) and makes it the default with xdg-mime,
# so browser sign-ins can return to the app; it logs a handler it replaces.
# When DESTDIR is set (distro packaging in a staging root), per-user URL
# handler registration and desktop database updates are skipped.
# No icon-theme.cache is written: a stale one in ~/.local would hide icons
# that other programs install later; touching the directory is enough.
install: build
	install -Dm755 target/release/$(BIN) $(DESTDIR)$(BINDIR)/$(BIN)
	install -Dm644 data/$(APP_ID).desktop $(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop
	install -Dm644 data/icons/hicolor/scalable/apps/$(APP_ID).svg \
		$(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	install -Dm644 data/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg \
		$(DESTDIR)$(DATADIR)/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg
	sed -i "s|^Exec=.*|Exec=$(BINDIR)/$(BIN)|" $(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop
ifeq ($(strip $(DESTDIR)),)
	$(BINDIR)/$(BIN) --register-url-handler
	-update-desktop-database -q $(DATADIR)/applications 2>/dev/null
endif
	-touch -c $(DESTDIR)$(DATADIR)/icons/hicolor
	@echo "Installed. Start it from the application menu or run $(BINDIR)/$(BIN)"

uninstall:
ifeq ($(strip $(DESTDIR)),)
	-$(BINDIR)/$(BIN) --unregister-url-handler
endif
	rm -f $(DESTDIR)$(BINDIR)/$(BIN)
	rm -f $(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop
	rm -f $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	rm -f $(DESTDIR)$(DATADIR)/icons/hicolor/symbolic/apps/$(APP_ID)-symbolic.svg
ifeq ($(strip $(DESTDIR)),)
	-update-desktop-database -q $(DATADIR)/applications 2>/dev/null
endif
	-touch -c $(DESTDIR)$(DATADIR)/icons/hicolor
	@echo "Removed. Settings (~/.config/cloudrdpapp), cache and logs are kept;"
	@echo "sign out in the app first to remove the saved sign-in from the keyring."

test:
	$(CARGO) test

check:
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) test

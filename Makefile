PREFIX := /usr
BINDIR := $(PREFIX)/bin
PKG_CONFIG := pkg-config
DBUS_INTERFACES_DIR := $(shell $(PKG_CONFIG) dbus-1 --variable=interfaces_dir)
DBUS_SESSION_BUS_SERVICES_DIR := $(shell $(PKG_CONFIG) dbus-1 --variable=session_bus_services_dir)
UDEV_DIR := $(shell $(PKG_CONFIG) udev --variable=udev_dir)
UDEV_RULES_DIR := $(UDEV_DIR)/rules.d
SYSTEMD_USER_UNIT_DIR := $(shell $(PKG_CONFIG) systemd --variable=systemd_user_unit_dir)

INTERFACES := CecDevice1 Config1 Daemon1 MessageHandler1

all: build

.PHONY: build clean test install

target/release/cecd: build

target/release/cectool: build

build:
	@cargo $(CARGOFLAGS) build -r --target-dir target

clean:
	@cargo $(CARGOFLAGS) clean

test:
	@cargo $(CARGOFLAGS) test

install: target/release/cecd target/release/cectool
	install -d -m 755 "$(DESTDIR)$(UDEV_RULES_DIR)"
	install -d -m 755 "$(DESTDIR)$(BINDIR)"
	install -d -m 755 "$(DESTDIR)$(SYSTEMD_USER_UNIT_DIR)"
	install -d -m 755 "$(DESTDIR)$(DBUS_INTERFACES_DIR)"
	install -d -m 755 "$(DESTDIR)$(DBUS_SESSION_BUS_SERVICES_DIR)"
	install -m 644 linux-cec/data/udev-rules.d/60-cec-uaccess.rules "$(DESTDIR)$(UDEV_RULES_DIR)"
	install -m 644 cecd/data/udev-rules.d/60-cecd-uinput.rules "$(DESTDIR)$(UDEV_RULES_DIR)"
	install -m 755 target/release/cecd "$(DESTDIR)$(BINDIR)/cecd"
	install -m 755 target/release/cectool "$(DESTDIR)$(BINDIR)/cectool"
	install -m 755 target/release/cec-attach-tty "$(DESTDIR)$(BINDIR)/cec-attach-tty"
	install -m 644 cecd/data/cecd.service "$(DESTDIR)$(SYSTEMD_USER_UNIT_DIR)"
	install -m 644 cecd/data/com.steampowered.CecDaemon1.service "$(DESTDIR)$(DBUS_SESSION_BUS_SERVICES_DIR)"
	install -m 644 $(patsubst %,cecd/data/dbus-interfaces/com.steampowered.CecDaemon1.%.xml,$(INTERFACES)) "$(DESTDIR)$(DBUS_INTERFACES_DIR)"

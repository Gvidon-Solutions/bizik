# bizik — build, install and deploy.
#
# Everything a server runs is the same binary you run locally, so a deploy is a
# build plus a copy. The default target prints what is available.
#
# Servers are not hardcoded here. Put yours in an untracked `hosts.mk`:
#
#     HOSTS = back=168.119.201.8 front=example.com
#
# and `make setup` becomes one command. Without it, pass HOSTS on the
# command line, or register hosts once with `bzk host add`.

TARGET  ?= x86_64-unknown-linux-musl
PREFIX  ?= $(HOME)/.local/bin
BIN     := target/$(TARGET)/release/bzk
HOSTS   ?=

-include hosts.mk

.PHONY: help setup build dev install push hooks hosts deploy run doctor \
        check test lint fmt fmt-check clean uninstall

help:
	@echo "bizik"
	@echo
	@echo "  make setup      first run: build, install, register hosts, deploy, check"
	@echo "  make install    build a static binary and put it in $(PREFIX)"
	@echo "  make deploy     install here, copy to every host, install hooks"
	@echo "  make run        open the dashboard"
	@echo "  make doctor     check the local setup and every host"
	@echo
	@echo "  make check      fmt, clippy and tests — everything CI would run"
	@echo "  make test       tests only"
	@echo "  make dev        fast native debug build"
	@echo
	@echo "  make uninstall  remove the local binary and configuration"
	@echo
	@echo "hosts: $(if $(HOSTS),$(HOSTS),none set — see the top of this Makefile)"

# --- building -------------------------------------------------------------
#
# The musl target is not optional for anything that gets copied to a server:
# a default build links against this machine's glibc, which is routinely newer
# than the server's, and the copy then refuses to start.

$(TARGET)-installed:
	@rustup target list --installed 2>/dev/null | grep -qx '$(TARGET)' \
	  || rustup target add $(TARGET)

build: $(TARGET)-installed
	cargo build --release --target $(TARGET)

dev:
	cargo build

install: build
	@mkdir -p $(PREFIX)
	install -m755 $(BIN) $(PREFIX)/bzk
	@echo "installed $(PREFIX)/bzk ($$($(PREFIX)/bzk --version))"
	@case ":$$PATH:" in *":$(PREFIX):"*) ;; \
	  *) echo "note: $(PREFIX) is not on your PATH";; esac

# --- deploying ------------------------------------------------------------

hosts:
	@test -n "$(HOSTS)" || { \
	  echo "no hosts. Either create hosts.mk with:"; \
	  echo "    HOSTS = back=1.2.3.4 front=example.com"; \
	  echo "or run: make hosts HOSTS='back=1.2.3.4'"; exit 1; }
	@for h in $(HOSTS); do \
	  name=$${h%%=*}; target=$${h#*=}; \
	  $(PREFIX)/bzk host add "$$name" "$$target"; \
	done

# Copies the binary to every registered host. Needed after every rebuild —
# `make doctor` says so when a host is left behind on an older one.
push:
	$(PREFIX)/bzk install

# Edits ~/.claude/settings.json on each host, merging rather than replacing.
# Without these, an idle session cannot be told apart from a blocked one.
hooks:
	@names=""; for h in $(HOSTS); do names="$$names $${h%%=*}"; done; \
	$(PREFIX)/bzk hooks install $$names

deploy: install push hooks doctor

setup: install hosts push hooks doctor
	@echo
	@echo "ready — run: make run"

run:
	$(PREFIX)/bzk

doctor:
	@$(PREFIX)/bzk doctor

# --- quality --------------------------------------------------------------

check: fmt-check lint test

test:
	cargo test

lint:
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

clean:
	cargo clean

# Leaves anything installed on the servers alone: remove that with
# `bzk hooks uninstall <hosts>` while the binary is still around.
uninstall:
	rm -f $(PREFIX)/bzk
	rm -rf $(HOME)/.config/bizik $(HOME)/.cache/bizik
	@echo "removed the local binary and configuration; servers untouched"

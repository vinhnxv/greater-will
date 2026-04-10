.PHONY: help install reinstall build dev check test fmt lint clean \
        daemon-start daemon-start-fg daemon-stop daemon-status daemon-restart \
        daemon-install daemon-uninstall daemon-logs ps

# Default target — list available commands
help:
	@echo "Greater-Will Makefile"
	@echo ""
	@echo "Build & install:"
	@echo "  make install          Install gw to ~/.cargo/bin (cargo install --path .)"
	@echo "  make reinstall        Force re-install (overwrites existing binary)"
	@echo "  make build            Release build (target/release/gw)"
	@echo "  make dev              Debug build (target/debug/gw)"
	@echo ""
	@echo "Quality gates:"
	@echo "  make check            cargo check --all-targets (fast ward check)"
	@echo "  make test             Run all tests"
	@echo "  make fmt              Format code with rustfmt"
	@echo "  make lint             Run clippy with warnings as errors"
	@echo ""
	@echo "Daemon lifecycle (requires 'make install' first):"
	@echo "  make daemon-start     Start daemon in background"
	@echo "  make daemon-start-fg  Start daemon in foreground (Ctrl+C to stop)"
	@echo "  make daemon-stop      Stop running daemon"
	@echo "  make daemon-status    Show daemon status"
	@echo "  make daemon-restart   Restart daemon"
	@echo "  make daemon-logs      Tail daemon log file"
	@echo "  make ps               List active runs"
	@echo ""
	@echo "System service (launchd on macOS):"
	@echo "  make daemon-install   Install as always-on launchd service"
	@echo "  make daemon-uninstall Uninstall launchd service"
	@echo ""
	@echo "Cleanup:"
	@echo "  make clean            cargo clean"

# ── Build & install ───────────────────────────────────────────────────
install:
	cargo install --path .

reinstall:
	cargo install --path . --force

build:
	cargo build --release

dev:
	cargo build

# ── Quality gates ─────────────────────────────────────────────────────
check:
	cargo check --all-targets

test:
	cargo test

fmt:
	cargo fmt

lint:
	cargo clippy --all-targets -- -D warnings

# ── Daemon lifecycle ──────────────────────────────────────────────────
daemon-start:
	gw daemon start

daemon-start-fg:
	gw daemon start --foreground

daemon-stop:
	gw daemon stop

daemon-status:
	gw daemon status

daemon-restart:
	gw daemon restart

daemon-logs:
	@tail -f ~/.gw/daemon.log 2>/dev/null || \
		(echo "No daemon log found at ~/.gw/daemon.log — is the daemon running?" && exit 1)

ps:
	gw ps

# ── System service (launchd) ──────────────────────────────────────────
daemon-install:
	gw daemon install

daemon-uninstall:
	gw daemon uninstall

# ── Cleanup ───────────────────────────────────────────────────────────
clean:
	cargo clean

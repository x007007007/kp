# kp 开发辅助
BIN_DIR   ?= target/debug
KP_BIN    := $(CURDIR)/$(BIN_DIR)/kp
DEV_DIR   := $(CURDIR)/.dev
USER_SHELL ?= $(shell echo $${SHELL:-/bin/zsh})
# 沙箱环境：kp（含它拉起的 tmux）的一切状态都落在 .dev/ 内，与系统级使用完全隔离
DEV_ENV    := PATH="$(CURDIR)/$(BIN_DIR):$$PATH" XDG_STATE_HOME="$(DEV_DIR)/state" XDG_RUNTIME_DIR="$(DEV_DIR)/runtime" XDG_CONFIG_HOME="$(DEV_DIR)/config" KP_DEV=1

.PHONY: help build test lint dev-env dev-clean

help:
	@echo "make build      编译（debug）"
	@echo "make test       单元测试"
	@echo "make lint       cargo fmt --check + clippy -D warnings"
	@echo "make dev-env    进入开发环境：kp = 开发中构建，状态沙箱于 .dev/"
	@echo "make dev-clean  清理沙箱（停止会话、杀 dev daemon、删 .dev）"

build:
	cargo build

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

dev-env: build
	@command -v tmux >/dev/null 2>&1 || { echo "kp 需要 tmux >= 3.0（macOS: brew install tmux）"; exit 1; }
	@mkdir -p "$(DEV_DIR)/state" "$(DEV_DIR)/runtime" "$(DEV_DIR)/config"
	@chmod 700 "$(DEV_DIR)/runtime"
	@echo "── kp dev-env ─────────────────────────────────"
	@echo " kp    → $(KP_BIN)"
	@echo "         $$($(KP_BIN) --version 2>/dev/null) · tmux $$($$(command -v tmux) -V)"
	@echo " 状态  → .dev/{state,runtime,config}（与系统 kp 完全隔离）"
	@echo " 试试  → kp sleep 300 &   kp ls   ·   kp logs -f <name>"
	@echo " 退出  → exit（自动停止沙箱内会话与 dev daemon）"
	@echo "───────────────────────────────────────────────"
	@$(DEV_ENV) "$(USER_SHELL)"; \
	$(DEV_ENV) "$(KP_BIN)" stop --all >/dev/null 2>&1; \
	pkill -f "$(KP_BIN) --daemon" 2>/dev/null; \
	echo "[kp dev-env] 已退出，沙箱会话与 daemon 已清理"

dev-clean:
	@-pkill -f "$(KP_BIN) --daemon" 2>/dev/null
	@rm -rf "$(DEV_DIR)"
	@echo "✓ 沙箱已删除（$(DEV_DIR)）"

CARGO ?= cargo
CARGO_TARGET_DIR ?= target
BINDIR ?= $(HOME)/.local/bin

.PHONY: install
install:
	$(CARGO) build --release --locked --bin fex --target-dir "$(CARGO_TARGET_DIR)"
	install -d "$(BINDIR)"
	install -m 755 "$(CARGO_TARGET_DIR)/release/fex" "$(BINDIR)/fex"

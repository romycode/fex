CARGO ?= cargo
RUSTC ?= rustc
CARGO_TARGET_DIR ?= target
BINDIR ?= $(HOME)/.local/bin

.PHONY: install
install:
	@set -eu; \
	os=$$(uname -s); \
	case "$$os" in \
		Linux|Darwin) ;; \
		*) printf 'Unsupported platform: %s\n' "$$os" >&2; exit 1 ;; \
	esac; \
	target=$$($(RUSTC) --print host-tuple); \
	case "$$os:$$target" in \
		Linux:*-linux-*|Darwin:aarch64-apple-darwin) ;; \
		Darwin:*) printf '%s\n' 'macOS requires a native arm64 Rust toolchain.' >&2; exit 1 ;; \
		*) printf 'Unsupported Rust host: %s\n' "$$target" >&2; exit 1 ;; \
	esac; \
	if [ "$${CARGO_ENCODED_RUSTFLAGS+x}" ]; then \
		separator=$$(printf '\037'); \
		export CARGO_ENCODED_RUSTFLAGS="$${CARGO_ENCODED_RUSTFLAGS:+$$CARGO_ENCODED_RUSTFLAGS$$separator}-Ctarget-cpu=native"; \
	else \
		export RUSTFLAGS="$${RUSTFLAGS:+$$RUSTFLAGS }-Ctarget-cpu=native"; \
	fi; \
	$(CARGO) build --release --locked --bin fex --target "$$target" \
		--target-dir "$(CARGO_TARGET_DIR)" --config profile.release.codegen-units=1; \
	install -d "$(BINDIR)"; \
	install -m 755 "$(CARGO_TARGET_DIR)/$$target/release/fex" "$(BINDIR)/fex"

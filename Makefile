INSTALL_DIR ?= $(HOME)/.local/bin

.PHONY: install build test

build:
	cargo build --release

install: build
	install -m 755 target/release/subswap $(INSTALL_DIR)/subswap

test:
	cargo test

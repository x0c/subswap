INSTALL_DIR ?= $(HOME)/.local/bin
PI_EXT_DIR = $(HOME)/.pi/agent/extensions
PI_EXT_LINK = $(PI_EXT_DIR)/subswap-pi

.PHONY: install build test install-pi doctor

build:
	cargo build --release

install: build
	install -m 755 target/release/subswap $(INSTALL_DIR)/subswap

# Pi 扩展默认安装链（显式目标，不合入默认 `install`）：
# link 缺失才建、指向别处永不覆盖只警告；node_modules 缺失才装依赖；最后跑 check。
install-pi:
	mkdir -p "$(PI_EXT_DIR)"
	@if [ -L "$(PI_EXT_LINK)" ]; then \
		target=$$(readlink "$(PI_EXT_LINK)"); \
		if [ "$$target" != "$(CURDIR)/integrations/pi" ]; then \
			echo "Pi extension link points elsewhere ($$target); never overwriting. Remove it manually to reinstall." >&2; \
			exit 1; \
		fi; \
	elif [ -e "$(PI_EXT_LINK)" ]; then \
		echo "Pi extension path exists but is not a symlink; never overwriting." >&2; \
		exit 1; \
	else \
		ln -s "$(CURDIR)/integrations/pi" "$(PI_EXT_LINK)"; \
	fi
	cd integrations/pi && npm install --ignore-scripts
	cd integrations/pi && npm run check
	@echo "Pi extension installed. Run 'subswap doctor' to verify [pi], then /reload in Pi."

doctor:
	cargo run -p subswap-cli -- doctor

test:
	cargo test

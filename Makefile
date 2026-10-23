.PHONY: help app run test check clean

help:
	@echo "MacGate"
	@echo "  make app     build/macrdp.app (SwiftUI app + Rust helper, signed with MACRDP_SIGN_ID,"
	@echo "               the 'macrdp-dev' identity if present, else ad-hoc)"
	@echo "  make run     run the server from the terminal (port from config, default 3389)"
	@echo "  make test    all Rust tests, including the patched IronRDP crates"
	@echo "  make check   build everything without running"
	@echo "  make clean   remove build output"

APP_OUT := build/macrdp.app
APP_DERIVED := build/DerivedData
# Stable identity keeps TCC grants and Keychain "Always Allow" across rebuilds; ad-hoc otherwise.
MACRDP_SIGN_ID ?= $(or $(shell security find-identity -p codesigning 2>/dev/null | awk '/"macrdp-dev"/{print $$2; exit}'),-)

# Hardened runtime stays off: the helper reaches private APIs through dlsym.
app:
	cargo build --release -p macrdp-server
	cd macrdp-app && xcodegen generate --quiet
	xcodebuild -project macrdp-app/macrdp.xcodeproj -scheme macrdp -configuration Release -destination generic/platform=macOS \
		-derivedDataPath $(APP_DERIVED) CODE_SIGNING_ALLOWED=NO -quiet build
	rm -rf $(APP_OUT)
	ditto $(APP_DERIVED)/Build/Products/Release/macrdp.app $(APP_OUT)
	cp target/release/macrdp-server $(APP_OUT)/Contents/MacOS/macrdp-server
	codesign --force --timestamp=none --identifier com.macrdp.server --sign "$(MACRDP_SIGN_ID)" $(APP_OUT)/Contents/MacOS/macrdp-server
	codesign --force --timestamp=none --sign "$(MACRDP_SIGN_ID)" $(APP_OUT)
	codesign --verify --strict --deep $(APP_OUT)
	@echo "Built $(APP_OUT)"

run:
	cargo run --release -p macrdp-server

# The patched IronRDP crates are [patch] path crates, not workspace members.
test:
	cargo test --workspace
	cargo test -p ironrdp-acceptor
	cargo test -p ironrdp-server --lib

check:
	cargo build --workspace
	cd macrdp-app/MacRDPKit && swift build

clean:
	cargo clean
	rm -rf build macrdp-app/macrdp.xcodeproj

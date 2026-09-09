VERSION      := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
MAC_TARGETS   := aarch64-apple-darwin x86_64-apple-darwin
LINUX_TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl
WIN_TARGETS   := x86_64-pc-windows-gnu aarch64-pc-windows-gnullvm
DIST          := dist

.PHONY: check fmt clippy test shellcheck version-check build dist release clean

check: fmt clippy test shellcheck version-check

fmt:
	cargo fmt --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

shellcheck:
	shellcheck plugin/scripts/*.sh

version-check:
	@PLUGIN=$$(python3 -c "import json;print(json.load(open('plugin/.claude-plugin/plugin.json'))['version'])"); \
	echo "workspace=$(VERSION) plugin=$$PLUGIN"; \
	test "$(VERSION)" = "$$PLUGIN" || { echo "version mismatch"; exit 1; }

build:
	cargo build --workspace

# Cross-compiled release tarballs for all six targets into dist/.
# macOS targets build natively; Linux musl and Windows GNU targets link
# through zig (brew install zig cargo-zigbuild) — cross/Docker doesn't work
# from an Apple Silicon host (needs a non-host rustup toolchain + qemu).
dist: $(addprefix dist-,$(MAC_TARGETS) $(LINUX_TARGETS) $(WIN_TARGETS))

dist-aarch64-apple-darwin dist-x86_64-apple-darwin: dist-%:
	rustup target add $*
	cargo build --release --target $* -p autofork -p autofork-daemon
	$(MAKE) package TARGET=$*

dist-x86_64-unknown-linux-musl dist-aarch64-unknown-linux-musl: dist-%:
	@command -v cargo-zigbuild >/dev/null || { echo "cargo-zigbuild not found: brew install zig cargo-zigbuild"; exit 1; }
	rustup target add $*
	cargo zigbuild --release --target $* -p autofork -p autofork-daemon
	$(MAKE) package TARGET=$*

dist-x86_64-pc-windows-gnu dist-aarch64-pc-windows-gnullvm: dist-%:
	@command -v cargo-zigbuild >/dev/null || { echo "cargo-zigbuild not found: brew install zig cargo-zigbuild"; exit 1; }
	rustup target add $*
	cargo zigbuild --release --target $* -p autofork -p autofork-daemon
	$(MAKE) package TARGET=$* EXE=.exe

# EXE is the binary suffix (".exe" for Windows targets); the tarball keeps
# it, and plugin/scripts/bootstrap.sh installs by the same name.
package:
	@test -n "$(TARGET)" || { echo "package: TARGET not set"; exit 1; }
	$(eval ASSET := autofork-v$(VERSION)-$(TARGET))
	rm -rf $(DIST)/$(TARGET)
	mkdir -p $(DIST)/$(TARGET)/bin
	cp target/$(TARGET)/release/autofork$(EXE) target/$(TARGET)/release/autofork-daemon$(EXE) $(DIST)/$(TARGET)/bin/
	-strip $(DIST)/$(TARGET)/bin/autofork$(EXE) $(DIST)/$(TARGET)/bin/autofork-daemon$(EXE) 2>/dev/null
	tar -czf $(DIST)/$(ASSET).tar.gz -C $(DIST)/$(TARGET) bin
	shasum -a 256 $(DIST)/$(ASSET).tar.gz | sed 's|$(DIST)/||' > $(DIST)/$(ASSET).tar.gz.sha256

# Full manual release: checks, builds all targets, tags v<version>,
# publishes a GitHub release with the twelve assets.
release: check dist
	@test -z "$$(git status --porcelain)" || { echo "working tree not clean"; exit 1; }
	@COUNT=$$(ls $(DIST)/*.tar.gz $(DIST)/*.sha256 | wc -l); \
	test "$$COUNT" -eq 12 || { echo "expected 12 assets, got $$COUNT"; exit 1; }
	git tag "v$(VERSION)"
	git push origin main "v$(VERSION)"
	gh release create "v$(VERSION)" $(DIST)/*.tar.gz $(DIST)/*.sha256 \
		--title "autofork v$(VERSION)" \
		--generate-notes

clean:
	rm -rf $(DIST)
	cargo clean

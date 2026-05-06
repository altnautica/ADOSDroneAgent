# Local release flow for the lite Rust agent.
#
# Run from the repo root via:
#
#     make -C agents/lite-rs release VERSION=0.1.4
#
# Targets are designed to be re-runnable. Artifacts land under ../../dist/
# (relative to this Makefile, i.e. <repo>/dist/). Run individual targets
# during development; run `release` for a full publish.

# The sign target uses bash process substitution; force bash so the
# Makefile works on systems where /bin/sh is dash or similar.
SHELL := /bin/bash

.PHONY: help build build-luckfox build-aarch64-glibc build-aarch64-musl build-armv7-musl \
        image image-luckfox manifest sign clean release pre-flight

REPO_ROOT := $(abspath ../..)
DIST := $(REPO_ROOT)/dist
LITE_DIR := $(REPO_ROOT)/agents/lite-rs

# Resolve VERSION from the workspace Cargo.toml when not passed in.
WS_VERSION := $(shell grep -m1 '^version' $(LITE_DIR)/Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
VERSION ?= $(WS_VERSION)
TAG := lite-v$(VERSION)

CROSS_TARGETS := \
        armv7-unknown-linux-musleabihf \
        aarch64-unknown-linux-gnu \
        aarch64-unknown-linux-musl \
        x86_64-unknown-linux-musl

help:
	@printf 'Lite agent local release targets\n'
	@printf '  make build                       Cross-compile binaries for all targets\n'
	@printf '  make build-luckfox               Cross-compile armv7-musl only\n'
	@printf '  make image                       Build the Luckfox flashable image\n'
	@printf '  make manifest                    Emit dist/ados-agent-manifest.json\n'
	@printf '  make sign                        Sign every artifact in dist/ with minisign\n'
	@printf '  make release VERSION=x.y.z       Full local release: build + image + manifest + sign + gh release\n'
	@printf '  make clean                       Remove dist/\n'
	@printf '\n'
	@printf 'Required tools: cross (cargo install cross), minisign, gh (logged in), python3, docker\n'

pre-flight:
	@command -v cross >/dev/null || { echo "cross not installed. Run: cargo install cross"; exit 1; }
	@command -v minisign >/dev/null || { echo "minisign not installed. brew install minisign"; exit 1; }
	@command -v gh >/dev/null || { echo "gh not installed. brew install gh"; exit 1; }
	@command -v python3 >/dev/null || { echo "python3 not on PATH"; exit 1; }
	@command -v docker >/dev/null || { echo "docker not installed (cross + Buildroot need it)"; exit 1; }
	@docker info >/dev/null 2>&1 || { echo "Docker daemon is not running. Start Docker Desktop."; exit 1; }
	@[ -n "$$LITE_AGENT_MINISIGN_KEY" ] && [ -n "$$LITE_AGENT_MINISIGN_PASSWORD" ] || { \
		echo "Set LITE_AGENT_MINISIGN_KEY and LITE_AGENT_MINISIGN_PASSWORD in your shell"; exit 1; }
	@mkdir -p $(DIST)

build: pre-flight $(addprefix build-,$(CROSS_TARGETS))
build-armv7-unknown-linux-musleabihf: build-armv7-musl
build-aarch64-unknown-linux-gnu: build-aarch64-glibc
build-aarch64-unknown-linux-musl: build-aarch64-musl
build-x86_64-unknown-linux-musl:
	cd $(LITE_DIR) && cross build --release --target x86_64-unknown-linux-musl -p ados-agent-lite

build-armv7-musl:
	cd $(LITE_DIR) && cross build --release --target armv7-unknown-linux-musleabihf -p ados-agent-lite

build-aarch64-glibc:
	cd $(LITE_DIR) && cross build --release --target aarch64-unknown-linux-gnu -p ados-agent-lite

build-aarch64-musl:
	cd $(LITE_DIR) && cross build --release --target aarch64-unknown-linux-musl -p ados-agent-lite

build-luckfox: build-armv7-musl

image: image-luckfox

image-luckfox: pre-flight
	@mkdir -p $(DIST)
	IMGBUILD_VERSION=$(VERSION) IMGBUILD_OUTPUT=$(DIST) \
		bash $(LITE_DIR)/imagebuilder/lib/build-driver.sh luckfox-pico-zero
	@set -e; \
	src=$$(ls $(DIST)/luckfox-pico-zero/ados-luckfox-pico-zero-*.img.gz 2>/dev/null | head -n1); \
	if [ -z "$$src" ]; then \
		echo "expected $(DIST)/luckfox-pico-zero/ados-luckfox-pico-zero-*.img.gz to exist; imagebuilder did not produce it" >&2; \
		exit 1; \
	fi; \
	cp "$$src" $(DIST)/

manifest: pre-flight
	python3 $(REPO_ROOT)/scripts/emit_ados_agent_manifest.py \
		$(DIST)/ados-agent-manifest.json --release-tag $(TAG)
	@install -m 0755 $(REPO_ROOT)/scripts/install-lite.sh $(DIST)/install-lite.sh
	@install -m 0755 $(REPO_ROOT)/scripts/install.sh      $(DIST)/install.sh

sign: pre-flight
	@echo "Signing artifacts under $(DIST)/"
	@cd $(DIST) && for artifact in $$(find . -maxdepth 2 \( -name '*.tar.gz' -o -name '*.img.gz' \) -type f); do \
		[ -f "$$artifact.minisig" ] && continue; \
		echo "  -> $$artifact"; \
		echo "$$LITE_AGENT_MINISIGN_PASSWORD" | minisign -S \
			-s <(printf '%s' "$$LITE_AGENT_MINISIGN_KEY") \
			-m "$$artifact"; \
		sha256sum "$$artifact" > "$$artifact.sha256"; \
	done

release: pre-flight build image manifest sign
	@echo "Publishing $(TAG) to altnautica/ADOSDroneAgent..."
	cd $(REPO_ROOT) && gh release create $(TAG) \
		--repo altnautica/ADOSDroneAgent \
		--title "Lightweight agent $(VERSION)" \
		--notes-file $(LITE_DIR)/CHANGELOG.md \
		$(DIST)/ados-agent-manifest.json \
		$(DIST)/luckfox-pico-zero/*.img.gz \
		$(DIST)/luckfox-pico-zero/*.img.gz.sha256 \
		$(DIST)/luckfox-pico-zero/*.img.gz.minisig

clean:
	rm -rf $(DIST)

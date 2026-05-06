################################################################################
#
# ados-rkmpi-wrapper
#
# Builds the subprocess wrapper that bridges the lite Rust agent to the
# Rockchip RKMPI hardware video encoder (RV1106 / RV1106G3). The wrapper
# C source plus its own Makefile live at
#   agents/lite-rs/boards/luckfox-pico-zero/rkmpi-wrapper/
# and are pulled in via SITE_METHOD=local.
#
# The wrapper's Makefile pins its own uclibc toolchain off SDK_ROOT
# (arm-rockchip830-linux-uclibcgnueabihf-gcc from the Luckfox SDK tree).
# That is by design — RKMPI ships as a vendor binary linked against
# that exact uclibc and trying to relink it against the Buildroot
# TARGET_CROSS toolchain would break ABI compatibility. So this recipe
# exports SDK_ROOT and lets the inner Makefile pick its own CC.
#
# SDK_ROOT must be set in the environment (CI exports it from the SDK
# clone step). A missing SDK_ROOT fails the build with a clear error
# from the inner Makefile rather than silently producing an unbuildable
# host-toolchain link.
#
# License file note: the canonical LICENSE lives at the repo root
# (ADOSDroneAgent/LICENSE). Buildroot resolves LICENSE_FILES relative
# to the BUILD_DIR copy of the package, not the original site, so we
# cannot reference the repo-root LICENSE via .. here. The package-level
# LICENSE declaration is sufficient; the legal-info warning is
# harmless.
#
################################################################################

ADOS_RKMPI_WRAPPER_VERSION = local
ADOS_RKMPI_WRAPPER_SITE = $(BR2_EXTERNAL_ADOS_PATH)/../boards/luckfox-pico-zero/rkmpi-wrapper
ADOS_RKMPI_WRAPPER_SITE_METHOD = local
ADOS_RKMPI_WRAPPER_LICENSE = GPL-3.0-or-later

# The wrapper Makefile constructs its own CC from SDK_ROOT and ignores
# CROSS_COMPILE / TARGET_CROSS, so we forward only SDK_ROOT. Falling
# back to the env var means a local developer building outside CI can
# `export SDK_ROOT=/opt/luckfox-pico` and run `make` exactly the same
# way CI does.
ADOS_RKMPI_WRAPPER_SDK_ROOT ?= $(SDK_ROOT)

define ADOS_RKMPI_WRAPPER_BUILD_CMDS
	@if [ -z "$(ADOS_RKMPI_WRAPPER_SDK_ROOT)" ]; then \
		echo "ERROR: SDK_ROOT must be set to a Luckfox SDK clone for ados-rkmpi-wrapper"; \
		exit 1; \
	fi
	$(MAKE) -C $(@D) \
		SDK_ROOT=$(ADOS_RKMPI_WRAPPER_SDK_ROOT) \
		all
endef

define ADOS_RKMPI_WRAPPER_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/rkmpi-wrapper \
		$(TARGET_DIR)/usr/lib/ados/rkmpi-wrapper
endef

$(eval $(generic-package))

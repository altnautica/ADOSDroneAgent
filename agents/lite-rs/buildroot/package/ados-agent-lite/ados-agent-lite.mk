################################################################################
#
# ados-agent-lite
#
# Pulls the prebuilt, signed Rust binary from GitHub Releases and installs it
# alongside a busybox sysv-rc init script and a minimal default agent.yaml.
#
# Version handling:
#   ADOS_AGENT_LITE_VERSION = "lite-agent-main"   tracks rolling main builds.
#                                                 Override at build time with
#                                                 ADOS_AGENT_LITE_VERSION=lite-vX.Y.Z
#                                                 to pin a stable release.
#
# Source layout assumption:
#   The release tarball at <SITE>/<SOURCE> contains the binary at the archive
#   root (file name `ados-agent-lite`). The Rust release workflow at
#   .github/workflows/lite-agent-release.yml produces archives in this shape.
#
# Hash verification:
#   ados-agent-lite.hash carries the sha256 of the tarball PLUS the sha256 of
#   the LICENSE file. Operators bumping ADOS_AGENT_LITE_VERSION must also
#   regenerate the .hash entries (Buildroot fails the build on mismatch).
#
################################################################################

ADOS_AGENT_LITE_VERSION = lite-agent-main
ADOS_AGENT_LITE_SITE = https://github.com/altnautica/ADOSDroneAgent/releases/download/$(ADOS_AGENT_LITE_VERSION)
ADOS_AGENT_LITE_SOURCE = ados-agent-lite-$(ADOS_AGENT_LITE_VERSION)-armv7-unknown-linux-musleabihf.tar.gz
ADOS_AGENT_LITE_LICENSE = GPL-3.0-or-later
ADOS_AGENT_LITE_LICENSE_FILES = LICENSE

# Pre-extracted binary lives at the archive root (the release workflow stages
# files explicitly without the leading "./"); copy directly into the rootfs.
define ADOS_AGENT_LITE_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/ados-agent-lite \
		$(TARGET_DIR)/usr/local/bin/ados-agent-lite
	$(INSTALL) -D -m 0755 $(BR2_EXTERNAL_ADOS_PATH)/package/ados-agent-lite/S99ados-agent-lite \
		$(TARGET_DIR)/etc/init.d/S99ados-agent-lite
	$(INSTALL) -D -m 0755 $(BR2_EXTERNAL_ADOS_PATH)/package/ados-agent-lite/S98ados-first-boot \
		$(TARGET_DIR)/etc/init.d/S98ados-first-boot
	$(INSTALL) -D -m 0755 $(BR2_EXTERNAL_ADOS_PATH)/package/ados-agent-lite/first-boot.sh \
		$(TARGET_DIR)/usr/local/sbin/ados-first-boot
	$(INSTALL) -D -m 0644 $(BR2_EXTERNAL_ADOS_PATH)/package/ados-agent-lite/agent.yaml \
		$(TARGET_DIR)/etc/ados/agent.yaml
	chmod 0640 $(TARGET_DIR)/etc/ados/agent.yaml
endef

$(eval $(generic-package))

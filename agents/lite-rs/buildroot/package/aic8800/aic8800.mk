################################################################################
#
# aic8800
#
# Out-of-tree Aicsemi AIC8800 family USB Wi-Fi kernel module (Radxa fork).
# Provides Wi-Fi 6 support on the Luckfox Pico Zero target. Firmware blobs
# under src/*/aic8800_fdrv/firmware/ are non-redistributable proprietary
# binaries shipped by the upstream repo and copied verbatim into
# /lib/firmware/aic8800DC/ on the target rootfs.
#
################################################################################

AIC8800_VERSION = 7f42b22913b462ab6c658dfc075bae1dbfe9a71a
AIC8800_SITE = $(call github,radxa-pkg,aic8800,$(AIC8800_VERSION))
AIC8800_LICENSE = GPL-3.0
AIC8800_LICENSE_FILES = debian/copyright
AIC8800_DEPENDENCIES = linux

AIC8800_MODULE_SUBDIRS = src/PCIE/aic8800_fdrv \
                         src/USB/aic8800_fdrv \
                         src/SDIO/aic8800_fdrv

AIC8800_MODULE_MAKE_OPTS = \
    KERNEL_DIR=$(LINUX_DIR) \
    CONFIG_AIC_LOAD_FW_FROM_USERSPACE=y

define AIC8800_INSTALL_FIRMWARE
    mkdir -p $(TARGET_DIR)/lib/firmware/aic8800DC
    cp -a $(@D)/src/PCIE/aic8800_fdrv/firmware/aic8800DC/* \
          $(TARGET_DIR)/lib/firmware/aic8800DC/
endef
AIC8800_POST_INSTALL_TARGET_HOOKS += AIC8800_INSTALL_FIRMWARE

$(eval $(kernel-module))
$(eval $(generic-package))

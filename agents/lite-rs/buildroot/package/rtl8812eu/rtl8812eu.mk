################################################################################
#
# rtl8812eu
#
# Out-of-tree Realtek RTL8812AU/EU USB Wi-Fi kernel module. Used by the lite
# agent's WFB-ng broadcast on the air side. Pulled from the aircrack-ng fork
# at a pinned SHA so monitor mode and frame injection behaviour are stable.
#
################################################################################

RTL8812EU_VERSION = 734485506a30d6237c2deaad666a19f8ca5379f2
RTL8812EU_SITE = $(call github,aircrack-ng,rtl8812au,$(RTL8812EU_VERSION))
RTL8812EU_LICENSE = GPL-2.0
RTL8812EU_LICENSE_FILES = LICENSE
RTL8812EU_DEPENDENCIES = linux

RTL8812EU_MODULE_MAKE_OPTS = \
    CONFIG_RTL8812AU=m \
    USER_EXTRA_CFLAGS="-DCONFIG_PLATFORM_ARM_RK3506" \
    KVER=$(LINUX_VERSION_PROBED) \
    KSRC=$(LINUX_DIR)

$(eval $(kernel-module))
$(eval $(generic-package))

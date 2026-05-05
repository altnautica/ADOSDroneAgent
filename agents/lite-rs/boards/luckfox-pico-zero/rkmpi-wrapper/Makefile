# Build the RKMPI wrapper subprocess for the lite agent.
#
# Usage:
#   make SDK_ROOT=/opt/luckfox-pico
#
# The output is a uclibc-linked executable that the agent installer
# drops at /usr/lib/ados/rkmpi-wrapper. The Buildroot recipe at
# agents/lite-rs/buildroot/package/ados-rkmpi-wrapper/ wraps this
# Makefile so a `make` in the Buildroot tree builds the wrapper
# alongside the rest of the rootfs.

# ---- toolchain --------------------------------------------------------

# Path to the unpacked Luckfox SDK. Override on the command line:
#   make SDK_ROOT=/opt/luckfox-pico
SDK_ROOT ?= /opt/luckfox-pico

# uclibc cross-compile triplet for RV1106 / RV1106G3.
TRIPLET ?= arm-rockchip830-linux-uclibcgnueabihf
TOOLCHAIN_BIN ?= $(SDK_ROOT)/tools/linux/toolchain/$(TRIPLET)/bin
CC := $(TOOLCHAIN_BIN)/$(TRIPLET)-gcc
STRIP := $(TOOLCHAIN_BIN)/$(TRIPLET)-strip

# ---- vendor headers + library ----------------------------------------

# Vendor RKMPI headers ship under media/mpp/release_*/include/rockchip
# in the SDK tree. The release variant must match the target ABI.
RKMPI_INCLUDE ?= $(SDK_ROOT)/media/mpp/release_mpp_rv1106_$(TRIPLET)/include/rockchip
RKMPI_LIB ?= $(SDK_ROOT)/media/mpp/release_mpp_rv1106_$(TRIPLET)/lib

# ---- build flags -----------------------------------------------------

CFLAGS := -O2 -Wall -Wextra -Werror -std=gnu99 \
          -ffunction-sections -fdata-sections \
          -I$(RKMPI_INCLUDE)
LDFLAGS := -Wl,--gc-sections \
           -L$(RKMPI_LIB) -Wl,-rpath-link,$(RKMPI_LIB)
# rockchip_mpp is the vendor library that ships RK_MPI_VENC_*; pthread
# is required by the vendor lib's internal threads; m is for the
# bitrate / GOP arithmetic in the encoder attribute setup.
LDLIBS := -lrockchip_mpp -lpthread -lm

# Length-prefixed msgpack framing is implemented inline in main.c so
# the wrapper has no msgpack-c dependency. The body shapes are small
# enough to encode by hand against the rmp-serde wire layout.

OBJ := main.o

# ---- targets ---------------------------------------------------------

.PHONY: all clean install
all: rkmpi-wrapper

rkmpi-wrapper: $(OBJ)
	$(CC) -o $@ $^ $(LDFLAGS) $(LDLIBS)
	$(STRIP) --strip-unneeded $@

%.o: %.c
	$(CC) $(CFLAGS) -c -o $@ $<

# Install target for the Buildroot recipe wrapper. DESTDIR + PREFIX
# follow the standard convention so a vendor packaging script can drop
# the binary into the rootfs.
PREFIX ?= /usr
INSTALL_DIR ?= $(PREFIX)/lib/ados
install: rkmpi-wrapper
	install -D -m 0755 rkmpi-wrapper $(DESTDIR)$(INSTALL_DIR)/rkmpi-wrapper

clean:
	rm -f $(OBJ) rkmpi-wrapper

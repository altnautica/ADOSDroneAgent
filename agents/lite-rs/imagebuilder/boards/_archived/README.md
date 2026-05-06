# Archived board recipes

Board recipes here are preserved for reference but are no longer wired into
the imagebuilder CI matrix or the local release flow.

## pi-zero-2w

The Pi Zero 2 W lite-agent install path is the curl one-liner running on
stock Raspberry Pi OS Lite, not a custom flashable image. The mainline
CYW43436 Wi-Fi driver, libcamera, systemd, glibc, and apt are all present
on stock Pi OS, so there is nothing the agent needs that a userspace
installer cannot put in place. The Mission Control Flash Tool surfaces the
curl one-liner directly when the user picks `Pi Zero 2 W`.

The recipe is preserved in case a pre-baked image is ever wanted (zero
terminal interaction for Pi users). Restoring it is one PR: move the
folder back to `boards/pi-zero-2w/` and re-add it to the release matrix.

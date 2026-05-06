# AIC8800 Wi-Fi driver and firmware blobs

The `aic8800` Buildroot package ships two distinct artifacts that have
distinct licence terms.

## Driver source (kernel modules)

- **Source:** `radxa-pkg/aic8800` GitHub fork at the SHA pinned in
  `aic8800.mk` (`AIC8800_VERSION`).
- **Licence:** GPL-3.0-or-later. License file in the upstream tarball:
  `debian/copyright`.
- **Built artifact:** `.ko` kernel modules installed to
  `/lib/modules/<kver>/extra/`. Kernel module licence string is `"GPL"`
  per the in-tree `MODULE_LICENSE()` macro.
- **Distribution:** unrestricted under the GPL. No additional terms
  apply when shipping the modules in the Buildroot image.

## Firmware blobs

- **Source:** the same upstream fork carries the firmware binaries under
  `src/PCIE/aic8800_fdrv/firmware/aic8800DC/` (and parallel paths for
  USB / SDIO variants).
- **Licence:** **proprietary, non-redistributable as written.** The
  upstream `debian/copyright` file describes the firmware as vendor-
  shipped binaries originating from Aicsemi. The Radxa fork distributes
  them under the same terms Aicsemi released them, which permit use on
  the device but do not grant a redistribution right to third parties
  in writing.
- **Built artifact:** binary blobs copied to `/lib/firmware/aic8800DC/`
  in the Buildroot rootfs by the `AIC8800_INSTALL_FIRMWARE` post-install
  hook in `aic8800.mk`. The driver's `request_firmware()` calls load
  them at runtime.

## Distribution implications

Shipping the AIC8800DC firmware blobs inside an image artifact (e.g.
`ados-luckfox-pico-zero-X.Y.Z.img.gz`) means the image is a derivative
work that contains those blobs. Confirm with counsel before publishing
the image to a wide audience. Two pragmatic mitigations:

1. **Vendor-licence-tracker pattern (default).** Note in the OEM flash
   guide that the image includes vendor firmware shipped under the
   upstream licence. Operators who redistribute the image accept the
   same terms that apply to the upstream Radxa fork. This is the
   standard practice across embedded Linux distributions.

2. **User-supplied firmware path (alternative).** Strip the firmware
   from the image and require operators to drop the blobs into
   `/lib/firmware/aic8800DC/` themselves on first boot. The setup
   webapp can surface a "Wi-Fi driver firmware missing" error and link
   to a download. This keeps the image redistributable but breaks the
   zero-touch flash UX.

The ADOS lite image ships the blobs by default (mitigation #1). A
follow-up release MAY switch to mitigation #2 if redistribution terms
tighten or if the firmware grows past the size budget for the image.

## Verifying the licence file in a fresh build

```
make aic8800-source
tar -xzOf "$(BUILDROOT_OUTPUT)/dl/aic8800/aic8800-<sha>.tar.gz" \
    "aic8800-<sha>/debian/copyright" | head -200
```

If the upstream `debian/copyright` text changes between pinned SHAs,
update the corresponding hash in `aic8800.hash` AND re-read the file
to confirm the licence terms above still hold.

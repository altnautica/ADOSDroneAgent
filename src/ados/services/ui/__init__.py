"""Ground-station UI input services and helpers.

The panel is rendered by the native display service and the front-panel GPIO
buttons are read in-process by the native input arbiter, which rebuilds its
mapping on SIGHUP. This package keeps the Python-side configuration helpers
those rely on:

* ``display_conf`` reads and writes the SPI LCD rotation config.
* ``touch`` holds the touch-calibration session, affine transform, and
  recent-event ring that the display REST routes share with the panel.
"""

"""Ground-station UI input services and helpers.

The panel is rendered by the native display service; this package keeps
the Python-side input and configuration helpers it relies on:

* ``button_service`` reads the front-panel GPIO buttons and publishes
  ``ButtonEvent`` on the bus defined in ``events``. It runs under
  systemd via ``python -m ados.services.ui.button_service``.
* ``display_conf`` reads and writes the SPI LCD rotation config.
* ``reload_signal`` SIGHUPs the panel services when the GCS edits the
  display or button settings.
* ``touch`` holds the touch-calibration session, affine transform, and
  recent-event ring that the display REST routes share with the panel.
"""

"""Touch input subsystem for the SPI LCD.

Reads raw events from the ADS7846 evdev node, applies the saved
calibration matrix and panel rotation, and publishes high-level
gestures (tap, long press, swipe, drag) on a fanout bus that pages
subscribe to.

The split is:

* :mod:`events` — dataclasses + bus shapes.
* :mod:`transform` — affine math + calibration persistence.
* :mod:`kinetic` — drag-scroll decay state machine.
* :mod:`calibrate` — 5-point wizard renderer + math.
* :mod:`bridge` — evdev reader, classifier, and bus publisher.
"""

from __future__ import annotations

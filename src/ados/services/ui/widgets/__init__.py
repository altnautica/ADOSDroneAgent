"""Reusable widget primitives for LCD pages.

Widgets are split between two flavors:

* Functional draws — :func:`draw_list_row`, :func:`draw_reboot_banner`.
  Painted inline by a page renderer at a specified origin.
* Modal pages — :class:`EnumPickerModal`, :class:`SliderModal`,
  :class:`KeyboardModal`, :class:`ConfirmDialog`. Pushed onto the
  navigator's modal stack by a page that wants to capture a value.
"""

from __future__ import annotations

from ados.services.ui.widgets.camera_chip import draw_camera_chip
from ados.services.ui.widgets.confirm_dialog import ConfirmDialog
from ados.services.ui.widgets.enum_picker import EnumPickerModal, options_from_strings
from ados.services.ui.widgets.list_row import ROW_H, draw_list_row
from ados.services.ui.widgets.onscreen_keyboard import KeyboardModal
from ados.services.ui.widgets.reboot_banner import BANNER_H, draw_reboot_banner
from ados.services.ui.widgets.rec_button import draw_rec_button
from ados.services.ui.widgets.slider import SliderModal
from ados.services.ui.widgets.video_compositor import VideoCompositor

__all__ = [
    "BANNER_H",
    "ConfirmDialog",
    "EnumPickerModal",
    "KeyboardModal",
    "ROW_H",
    "SliderModal",
    "VideoCompositor",
    "draw_camera_chip",
    "draw_list_row",
    "draw_rec_button",
    "draw_reboot_banner",
    "options_from_strings",
]

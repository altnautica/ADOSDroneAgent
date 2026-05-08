"""LCD chrome — top status bar and bottom tab bar.

These render the persistent shell that frames every page in the LCD
UI. The framing is intentionally thin (32 px on top, 44 px on the
bottom) so each page gets the bulk of the 480x320 panel.

The chrome modules are pure paint functions: callers pass in the live
state dict and palette, the bar paints in place onto an existing PIL
Image. No I/O, no global state.
"""

from __future__ import annotations

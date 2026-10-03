"""Integer limits of the native readers' field types.

The Rust daemons parse a config section into fixed-width integers, and a
section that fails to parse falls back to defaults wholesale. Every Python
config integer is therefore bounded by the type its native reader declares,
so the generated schema (and with it the config write route) refuses a value
the reader would choke on instead of persisting it.
"""

from __future__ import annotations

U8_MAX = 0xFF
U16_MAX = 0xFFFF
U32_MAX = 0xFFFF_FFFF
I8_MIN = -0x80
I8_MAX = 0x7F
I64_MIN = -(2**63)
I64_MAX = 2**63 - 1

__all__ = ["I8_MAX", "I8_MIN", "I64_MAX", "I64_MIN", "U8_MAX", "U16_MAX", "U32_MAX"]

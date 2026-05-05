"""Reusable PIL render primitives for the SPI LCD dashboards.

Each module here exposes a small set of pure functions that paint a
single visual element onto a PIL ImageDraw surface. The dashboard
composer (``groundnode_landscape``) lays these out into a 480x320
canvas. Keeping the primitives narrow makes the layout easy to
modify without re-deriving math, and gives us a clean target for
unit tests when we add a render-snapshot suite.
"""

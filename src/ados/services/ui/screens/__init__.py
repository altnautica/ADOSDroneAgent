"""OLED screen renderers (MSN-025 Wave B).

Each module exposes `render(draw, width, height, state)` that draws
onto a PIL ImageDraw canvas handed in by `luma.core.render.canvas`.

Screens are pure. They read from `state` (the polled ground-station
status dict) and never mutate. Unknown fields render as `--`.
"""

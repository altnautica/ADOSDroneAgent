"""Interactive renderer for ``ados update``.

Mirrors the look of the installer's progress UI (rounded box, braille spinner,
green/red + cyan accent, ASCII fallback) but in Python, since the OTA self-update
is a client-driven flow against the running agent rather than the Rust installer.

No new dependency: a small ANSI "sticky block" drawn to stderr, render-only
(only ever writes, never reads input), so it is safe over SSH and degrades to
plain line output when stderr is not a terminal. The agent already tracks the
update phase (``GET /api/ota`` ``state``) and a live download fraction
(``download`` block), so the install POST runs in a background thread while the
main thread polls + renders.
"""

from __future__ import annotations

import os
import sys
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any

import click

# Ordered phases the operator sees, mapped from the agent's UpdateState.
PHASES = ("Download", "Verify", "Install", "Restart")
# UpdateState (GET /api/ota `state`) → the phase index it lights up.
_STATE_PHASE = {
    "downloading": 0,
    "verifying": 1,
    "installing": 2,
    "restarting": 3,
}
_SPIN_UNICODE = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
_SPIN_ASCII = "-\\|/"


@dataclass
class _Theme:
    color: bool
    ascii: bool

    def paint(self, s: str, code: str) -> str:
        return f"\x1b[{code}m{s}\x1b[0m" if self.color else s

    def ok(self, s: str) -> str:
        return self.paint(s, "32")  # green

    def fail(self, s: str) -> str:
        return self.paint(s, "31")  # red

    def accent(self, s: str) -> str:
        return self.paint(s, "36")  # cyan

    def dim(self, s: str) -> str:
        return self.paint(s, "90")  # bright-black / grey

    def spinner(self, frame: int) -> str:
        seq = _SPIN_ASCII if self.ascii else _SPIN_UNICODE
        return seq[frame % len(seq)]

    def glyph_ok(self) -> str:
        return "+" if self.ascii else "✓"

    def glyph_fail(self) -> str:
        return "x" if self.ascii else "✗"

    def glyph_pending(self) -> str:
        return "." if self.ascii else "•"

    def box(self) -> tuple[str, str, str, str, str, str]:
        if self.ascii:
            return ("+", "+", "+", "+", "-", "|")
        return ("╭", "╮", "╰", "╯", "─", "│")


def _locale_is_utf8() -> bool:
    for key in ("LC_ALL", "LC_CTYPE", "LANG"):
        val = os.environ.get(key)
        if val:
            return "utf-8" in val.lower() or "utf8" in val.lower()
    return True


def _detect_theme() -> _Theme:
    return _Theme(color="NO_COLOR" not in os.environ, ascii=not _locale_is_utf8())


@dataclass
class _Model:
    new_version: str
    current_version: str
    active: int = -1  # index into PHASES, -1 = nothing started
    done_through: int = -1  # highest completed phase index
    failed: bool = False
    download: dict[str, Any] = field(default_factory=dict)

    def apply_status(self, status: dict[str, Any]) -> None:
        state = str(status.get("state", "")).lower()
        if state == "failed":
            self.failed = True
            return
        self.download = status.get("download", {}) or {}
        idx = _STATE_PHASE.get(state)
        if idx is not None:
            self.active = idx
            self.done_through = max(self.done_through, idx - 1)


def _term_width() -> int:
    try:
        return max(40, min(os.get_terminal_size(sys.stderr.fileno()).columns - 2, 64))
    except OSError:
        return 58


def _bar(theme: _Theme, percent: float) -> str:
    cells = 8
    filled = max(0, min(cells, round(percent / 100 * cells)))
    if theme.ascii:
        return "[" + "#" * filled + "." * (cells - filled) + "]"
    return "▕" + "█" * filled + "░" * (cells - filled) + "▏"


def _phase_detail(theme: _Theme, model: _Model, idx: int) -> str:
    """Right-aligned detail for a phase row (plain text; width is measured)."""
    if idx == 0 and model.active == 0 and model.download:
        pct = float(model.download.get("percent", 0) or 0)
        speed = float(model.download.get("speed_bps", 0) or 0)
        mbps = f"  {speed / 1e6:.1f} MB/s" if speed > 0 else ""
        return f"{_bar(theme, pct)} {pct:>3.0f}%{mbps}"
    return ""


def _block_lines(theme: _Theme, model: _Model, spinner: int, width: int) -> list[str]:
    tl, tr, bl, br, h, v = theme.box()
    content_w = width - 2
    body_w = content_w - 2

    title = f"ADOS Drone Agent · updating to {model.new_version}"[: max(0, content_w - 4)]
    lead = f"{h} {title} "
    dashes = max(1, content_w - _vlen(lead))
    top = theme.accent(f"{tl}{lead}{h * dashes}{tr}")

    rows: list[str] = []
    for i, name in enumerate(PHASES):
        if model.failed and i == model.active:
            glyph, gcolor = theme.glyph_fail(), theme.fail
        elif i <= model.done_through:
            glyph, gcolor = theme.glyph_ok(), theme.ok
        elif i == model.active:
            glyph, gcolor = theme.spinner(spinner), theme.accent
        else:
            glyph, gcolor = theme.glyph_pending(), theme.dim
        detail = _phase_detail(theme, model, i)
        label = name[: max(0, body_w - 2 - len(detail) - 1)]
        pad = max(0, body_w - 2 - len(label) - len(detail))
        body = f"{gcolor(glyph)} {label}{' ' * pad}{theme.accent(detail)}"
        rows.append(f"{theme.accent(v)} {body} {theme.accent(v)}")

    bottom = theme.accent(f"{bl}{h * content_w}{br}")
    return [top, *rows, bottom]


def _vlen(s: str) -> int:
    """Visible length: chars excluding ANSI escapes (our plain inputs have none)."""
    return len(s)


class _Sticky:
    """A render-only ANSI sticky block on stderr."""

    def __init__(self) -> None:
        self.height = 0
        self.out = sys.stderr

    def hide_cursor(self) -> None:
        self.out.write("\x1b[?25l")
        self.out.flush()

    def show_cursor(self) -> None:
        self.out.write("\x1b[?25h")
        self.out.flush()

    def draw(self, lines: list[str]) -> None:
        buf = []
        if self.height:
            buf.append(f"\x1b[{self.height}F")
        buf.append("\x1b[J")
        for line in lines:
            buf.append(line + "\n")
        self.out.write("".join(buf))
        self.out.flush()
        self.height = len(lines)

    def erase(self) -> None:
        if self.height:
            self.out.write(f"\x1b[{self.height}F\x1b[J")
            self.out.flush()
            self.height = 0


def _print_card(theme: _Theme, ok: bool, lines: list[str]) -> None:
    tl, tr, bl, br, h, v = theme.box()
    paint = theme.ok if ok else theme.fail
    width = _term_width()
    content_w = width - 2
    body_w = content_w - 2
    title = lines[0]
    lead = f"{h} {title[: content_w - 4]} "
    dashes = max(1, content_w - _vlen(lead))
    sys.stderr.write(paint(f"{tl}{lead}{h * dashes}{tr}") + "\n")
    for body in lines[1:]:
        clipped = body[:body_w]
        sys.stderr.write(f"{paint(v)} {clipped}{' ' * (body_w - len(clipped))} {paint(v)}\n")
    sys.stderr.write(paint(f"{bl}{h * content_w}{br}") + "\n")
    sys.stderr.flush()


def run(
    request: Callable[..., dict[str, Any]],
    current_version: str,
    new_version: str,
    interactive: bool,
) -> None:
    """Download + install the pending update, then restart.

    Interactive: a live phase checklist + download bar + closing card. Plain
    (non-tty / piped): the original line output, unchanged.
    """
    if not interactive:
        _run_plain(request, current_version, new_version)
        return

    theme = _detect_theme()
    model = _Model(new_version=new_version, current_version=current_version)
    sticky = _Sticky()
    sticky.hide_cursor()

    holder: dict[str, Any] = {}

    def _install() -> None:
        try:
            holder["resp"] = request("POST", "/api/ota/install", timeout=600.0)
        except Exception as exc:  # surfaced after the poll loop joins
            holder["err"] = exc

    worker = threading.Thread(target=_install, daemon=True)
    worker.start()

    spinner = 0
    model.active = 0  # the POST starts with the download
    try:
        while worker.is_alive():
            try:
                model.apply_status(request("GET", "/api/ota", timeout=5.0))
            except Exception:
                pass  # transient; keep animating
            sticky.draw(_block_lines(theme, model, spinner, _term_width()))
            spinner += 1
            time.sleep(0.25)
        worker.join()
    finally:
        pass

    err = holder.get("err")
    resp = holder.get("resp") or {}
    if err is not None or resp.get("status") == "error":
        model.failed = True
        sticky.draw(_block_lines(theme, model, spinner, _term_width()))
        sticky.erase()
        sticky.show_cursor()
        msg = str(err) if err is not None else str(resp.get("message", "update failed"))
        _print_card(
            theme,
            ok=False,
            lines=[
                f"{theme.glyph_fail()} Update failed",
                msg[:120],
                "full log: journalctl -t ados-agent",
            ],
        )
        raise click.ClickException("update failed")

    # Install succeeded: mark download/verify/install done, then restart.
    model.done_through = 2
    model.active = 3
    sticky.draw(_block_lines(theme, model, spinner, _term_width()))
    try:
        request("POST", "/api/ota/restart", timeout=30.0)
    except Exception:
        pass  # the service is restarting; the connection drop is expected
    model.done_through = 3
    sticky.draw(_block_lines(theme, model, spinner, _term_width()))
    sticky.erase()
    sticky.show_cursor()

    _print_card(
        theme,
        ok=True,
        lines=[
            f"{theme.glyph_ok()} Updated to {new_version}",
            f"was {current_version} · the agent is restarting",
            "",
            "Next: ados status",
        ],
    )


def _run_plain(
    request: Callable[..., dict[str, Any]], current_version: str, new_version: str
) -> None:
    """The non-interactive path (piped / CI): plain lines, unchanged behavior."""
    click.echo("Downloading and installing...")
    result = request("POST", "/api/ota/install", timeout=600.0)
    if result.get("status") == "error":
        raise click.ClickException(str(result.get("message", "Update failed")))
    click.echo("Restarting agent service...")
    try:
        restart = request("POST", "/api/ota/restart", timeout=30.0)
        click.echo(str(restart.get("message", "Restart requested.")))
    except click.ClickException:
        click.echo("Restart requested. The service may already be restarting.")

"""Shared ANSI rendering primitives for the ADOS CLI.

A dependency-free house style — rounded boxes, a braille spinner, a green/red
palette with a cyan accent, and ASCII + ``NO_COLOR`` fallbacks — used across the
operator-facing ``ados`` surfaces: the plain status one-pager, ``ados help``,
``ados pair`` / ``ados unpair``, and the ``ados uninstall`` progress checklist.
The interactive CLI commands share the same ``Theme`` / ``Sticky`` / ``bar`` /
``print_card`` primitives, re-imported from here.

Render-only: content goes to stdout, the sticky progress block goes to stderr,
and nothing ever reads input, so every surface is safe over SSH and degrades to
plain line output when the stream is not a terminal.
"""

from __future__ import annotations

import os
import sys
import threading
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from typing import TextIO

SPIN_UNICODE = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
SPIN_ASCII = "-\\|/"


@dataclass
class Theme:
    """Color + glyph vocabulary, gated on ``NO_COLOR`` and the locale's UTF-8-ness."""

    color: bool
    ascii: bool

    def paint(self, s: str, code: str) -> str:
        return f"\x1b[{code}m{s}\x1b[0m" if self.color else s

    def ok(self, s: str) -> str:
        return self.paint(s, "32")  # green

    def fail(self, s: str) -> str:
        return self.paint(s, "31")  # red

    def warn(self, s: str) -> str:
        return self.paint(s, "33")  # yellow

    def accent(self, s: str) -> str:
        return self.paint(s, "36")  # cyan

    def dim(self, s: str) -> str:
        return self.paint(s, "90")  # bright-black / grey

    def bold(self, s: str) -> str:
        return self.paint(s, "1")

    def spinner(self, frame: int) -> str:
        seq = SPIN_ASCII if self.ascii else SPIN_UNICODE
        return seq[frame % len(seq)]

    def glyph_ok(self) -> str:
        return "+" if self.ascii else "✓"

    def glyph_fail(self) -> str:
        return "x" if self.ascii else "✗"

    def glyph_pending(self) -> str:
        return "." if self.ascii else "•"

    def glyph_arrow(self) -> str:
        return "->" if self.ascii else "➜"

    def glyph_marker(self) -> str:
        return "|" if self.ascii else "▌"

    def box(self) -> tuple[str, str, str, str, str, str]:
        if self.ascii:
            return ("+", "+", "+", "+", "-", "|")
        return ("╭", "╮", "╰", "╯", "─", "│")


def locale_is_utf8() -> bool:
    for key in ("LC_ALL", "LC_CTYPE", "LANG"):
        val = os.environ.get(key)
        if val:
            return "utf-8" in val.lower() or "utf8" in val.lower()
    return True


def detect_theme() -> Theme:
    return Theme(color="NO_COLOR" not in os.environ, ascii=not locale_is_utf8())


def ssh_session() -> bool:
    """True when this shell is a remote SSH session (localhost is then useless)."""
    return bool(os.environ.get("SSH_CONNECTION") or os.environ.get("SSH_TTY"))


def term_width(stream: TextIO | None = None) -> int:
    out = stream or sys.stderr
    try:
        return max(40, min(os.get_terminal_size(out.fileno()).columns - 2, 64))
    except OSError:
        return 58


def vlen(s: str) -> int:
    """Visible length: chars excluding ANSI escapes (our plain inputs have none)."""
    return len(s)


def bar(theme: Theme, percent: float, cells: int = 8) -> str:
    filled = max(0, min(cells, round(percent / 100 * cells)))
    if theme.ascii:
        return "[" + "#" * filled + "." * (cells - filled) + "]"
    return "▕" + "█" * filled + "░" * (cells - filled) + "▏"


# ── Section header + inline glyphs ──────────────────────────────────────────


def marker(theme: Theme, label: str) -> str:
    """A section header: an accent bar + a bold label (the ``▌ ADOS`` word-mark)."""
    return f"{theme.accent(theme.glyph_marker())} {theme.bold(label)}"


_DOT_UNICODE = {"ok": "●", "warn": "●", "fail": "●", "pending": "○", "active": "◐"}
_DOT_ASCII = {"ok": "*", "warn": "!", "fail": "x", "pending": ".", "active": "o"}


def dot(theme: Theme, state: str) -> str:
    """A state dot, color co-encoded with a glyph (readable on a mono terminal)."""
    glyphs = _DOT_ASCII if theme.ascii else _DOT_UNICODE
    paint = {
        "ok": theme.ok,
        "warn": theme.warn,
        "fail": theme.fail,
        "pending": theme.dim,
        "active": theme.accent,
    }[state]
    return paint(glyphs[state])


def kv(theme: Theme, label: str, value: str, label_width: int = 12) -> str:
    """An aligned ``label   value`` row: dim label, bright value."""
    return f"  {theme.dim(label.ljust(label_width))}  {value}"


# ── Reachable-URL block (SSH-aware; localhost demoted) ──────────────────────


def is_localhost(url: str) -> bool:
    return "localhost" in url or "127.0.0.1" in url or "[::1]" in url


def is_mdns(url: str) -> bool:
    return ".local" in url and not is_localhost(url)


def order_reach_urls(urls: Sequence[str]) -> list[str]:
    """Order browser URLs so a remote operator sees a reachable one first:
    the mDNS ``.local`` host, then LAN addresses, then ``localhost`` last."""
    uniq: list[str] = []
    seen: set[str] = set()
    for u in urls:
        if u and u not in seen:
            seen.add(u)
            uniq.append(u)
    mdns = [u for u in uniq if is_mdns(u)]
    local = [u for u in uniq if is_localhost(u)]
    rest = [u for u in uniq if u not in mdns and u not in local]
    return mdns + rest + local


def reach_block(
    theme: Theme,
    urls: Sequence[str],
    *,
    title: str | None = "Reach this agent",
) -> list[str]:
    """Lines for a reachable-address block, ``.local`` / LAN IP first.

    ``localhost`` is shown (dimmed, marked on-box) ONLY as a last resort when no
    LAN/mDNS address is available. A remote operator cannot reach it, so when a
    routable address exists localhost is dropped as noise. Pass ``title=None`` to
    omit the section header.
    """
    ordered = order_reach_urls(urls)
    non_local = [u for u in ordered if not is_localhost(u)]
    show = non_local or ordered
    lines = [marker(theme, title)] if title else []
    arrow = theme.accent(theme.glyph_arrow())
    for u in show:
        if is_localhost(u):
            lines.append(f"  {arrow}  {theme.dim(u + '   (on-box only)')}")
        else:
            lines.append(f"  {arrow}  {u}")
    if not show:
        lines.append(f"  {theme.dim('(no reachable address found)')}")
    return lines


# ── Sticky progress block + closing card ────────────────────────────────────


class Sticky:
    """A render-only ANSI sticky block (redraws in place). Writes to stderr."""

    def __init__(self, out: TextIO | None = None) -> None:
        self.height = 0
        self.out = out or sys.stderr

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

    def leave(self) -> None:
        """Keep the last-drawn block on screen; stop tracking it."""
        self.height = 0


def print_card(theme: Theme, ok: bool, lines: list[str], out: TextIO | None = None) -> None:
    """A closing rounded card: ``lines[0]`` is the title, the rest is the body."""
    stream = out or sys.stderr
    tl, tr, bl, br, h, v = theme.box()
    paint = theme.ok if ok else theme.fail
    width = term_width(stream)
    content_w = width - 2
    body_w = content_w - 2
    title = lines[0]
    lead = f"{h} {title[: content_w - 4]} "
    dashes = max(1, content_w - vlen(lead))
    stream.write(paint(f"{tl}{lead}{h * dashes}{tr}") + "\n")
    for body in lines[1:]:
        clipped = body[:body_w]
        stream.write(f"{paint(v)} {clipped}{' ' * (body_w - len(clipped))} {paint(v)}\n")
    stream.write(paint(f"{bl}{h * content_w}{br}") + "\n")
    stream.flush()


# ── Live step checklist (uninstall, pair-bind, …) ───────────────────────────


@dataclass
class StepResult:
    label: str
    ok: bool
    elapsed: float
    detail: str = ""


# A step is a label plus a callable that does the blocking work and either
# returns an optional detail string or raises to signal failure.
Step = tuple[str, Callable[[], "str | None"]]


@dataclass
class _StepState:
    label: str
    status: str = "pending"  # pending | active | done | failed
    elapsed: float = 0.0
    detail: str = ""


def fmt_dur(seconds: float) -> str:
    if seconds < 60:
        return f"{seconds:.1f}s"
    minutes, secs = divmod(int(seconds), 60)
    return f"{minutes}:{secs:02d}"


def _step_rows(theme: Theme, title: str, states: Sequence[_StepState], spinner: int) -> list[str]:
    rows = [marker(theme, title)]
    for st in states:
        if st.status == "pending":
            rows.append(f"  {dot(theme, 'pending')} {theme.dim(st.label)}")
        elif st.status == "active":
            spin = theme.accent(theme.spinner(spinner))
            rows.append(f"  {spin} {st.label}   {theme.dim(fmt_dur(st.elapsed))}")
        elif st.status == "done":
            glyph = theme.ok(theme.glyph_ok())
            rows.append(f"  {glyph} {st.label}   {theme.dim(fmt_dur(st.elapsed))}")
        else:  # failed
            glyph = theme.fail(theme.glyph_fail())
            rows.append(
                f"  {glyph} {st.label}   {theme.fail('failed')} {theme.dim(fmt_dur(st.elapsed))}"
            )
            if st.detail:
                rows.append(f"      {theme.dim(st.detail)}")
    return rows


def run_steps(
    theme: Theme,
    steps: Sequence[Step],
    *,
    title: str,
    interactive: bool = True,
    out: TextIO | None = None,
) -> list[StepResult]:
    """Run steps in order with a live checklist, returning per-step results.

    Each step's blocking work runs on a worker thread so the spinner keeps
    animating during a slow call (a stubborn ``systemctl stop`` can take a
    minute). A step that raises is marked failed and the run CONTINUES to the
    next step; the caller decides what a failure means. Completed rows stay on
    screen as a record; the caller typically follows with a summary card.
    Non-interactive (piped / no TTY): one plain line per step, no animation.
    """
    if not interactive:
        return _run_steps_plain(steps, out=out)

    states = [_StepState(label=label) for label, _ in steps]
    results: list[StepResult] = []
    sticky = Sticky(out=out)
    sticky.hide_cursor()
    spinner = 0
    try:
        for i, (label, fn) in enumerate(steps):
            states[i].status = "active"
            holder: dict[str, object] = {}

            def _work(fn: Callable[[], str | None] = fn, holder: dict[str, object] = holder) -> None:
                try:
                    holder["detail"] = fn() or ""
                except Exception as exc:  # noqa: BLE001 — recorded, run continues
                    holder["err"] = exc

            worker = threading.Thread(target=_work, daemon=True)
            start = time.monotonic()
            worker.start()
            while worker.is_alive():
                states[i].elapsed = time.monotonic() - start
                sticky.draw(_step_rows(theme, title, states, spinner))
                spinner += 1
                time.sleep(0.15)
            worker.join()
            elapsed = time.monotonic() - start
            states[i].elapsed = elapsed
            if "err" in holder:
                detail = str(holder["err"])[:120]
                states[i].status = "failed"
                states[i].detail = detail
                results.append(StepResult(label, False, elapsed, detail))
            else:
                detail = str(holder.get("detail") or "")
                states[i].status = "done"
                results.append(StepResult(label, True, elapsed, detail))
            sticky.draw(_step_rows(theme, title, states, spinner))
    finally:
        sticky.leave()  # keep the finished checklist on screen as a record
        sticky.show_cursor()
    return results


def _run_steps_plain(steps: Sequence[Step], out: TextIO | None = None) -> list[StepResult]:
    stream = out or sys.stderr
    results: list[StepResult] = []
    for label, fn in steps:
        stream.write(f"... {label}\n")
        stream.flush()
        start = time.monotonic()
        try:
            detail = fn() or ""
            elapsed = time.monotonic() - start
            stream.write(f"    done ({fmt_dur(elapsed)})\n")
            results.append(StepResult(label, True, elapsed, str(detail)))
        except Exception as exc:  # noqa: BLE001 — recorded, run continues
            elapsed = time.monotonic() - start
            stream.write(f"    failed: {exc}\n")
            results.append(StepResult(label, False, elapsed, str(exc)[:120]))
        stream.flush()
    return results


__all__ = [
    "Theme",
    "detect_theme",
    "ssh_session",
    "term_width",
    "vlen",
    "bar",
    "marker",
    "dot",
    "kv",
    "reach_block",
    "order_reach_urls",
    "is_localhost",
    "is_mdns",
    "Sticky",
    "print_card",
    "run_steps",
    "StepResult",
    "Step",
    "fmt_dur",
]

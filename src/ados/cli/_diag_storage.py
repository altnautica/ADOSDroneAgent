"""Storage-report renderers for ``ados diag storage``: byte formatting, the
agent footprint against its budget, and the disk janitor's last pass."""

from __future__ import annotations

from typing import Any

import click

from ados.cli import _ansi


def _fmt_bytes(value: Any) -> str:
    """Human-readable size, or a dash when the number is genuinely absent."""
    if not isinstance(value, (int, float)):
        return "-"
    size = float(value)
    for unit in ("B", "KB", "MB", "GB"):
        if size < 1024:
            return f"{size:.0f} {unit}" if unit == "B" else f"{size:.1f} {unit}"
        size /= 1024
    return f"{size:.1f} TB"


# How each reclaim category is named on screen, in sweep order. A category the
# agent reports but this map does not know is still shown, under its raw key —
# a silently dropped row would be a category quietly deleting things with
# nothing on screen to say so.
_JANITOR_CATEGORIES = {
    # Reclaim categories (what a pass freed) and footprint categories (what is
    # occupied) overlap but are not identical: apt is one budget category and
    # two reclaim ones, and the live store occupies space no pass may reclaim.
    # Both sets are listed so neither renderer silently drops a row.
    "log_store": "logging store",
    "apt": "apt cache and index",
    "apt_archives": "downloaded packages",
    "apt_lists": "package index",
    "plugin_logs": "plugin logs",
    "audit_log": "audit trail",
    "recordings": "recordings",
    "journal": "journal",
    "quarantined_stores": "quarantined stores",
}


def _category_rows(block: Any) -> list[tuple[str, int]]:
    """Non-zero per-category byte figures from a janitor block, in sweep order."""
    if not isinstance(block, dict):
        return []
    rows: list[tuple[str, int]] = []
    for key, label in _JANITOR_CATEGORIES.items():
        value = block.get(key)
        if isinstance(value, int) and value > 0:
            rows.append((label, value))
    for key, value in block.items():
        if key not in _JANITOR_CATEGORIES and isinstance(value, int) and value > 0:
            rows.append((key, value))
    return rows


def _render_footprint(theme: Any, janitor: dict[str, Any]) -> None:
    """How much of the card the agent occupies, against the budget it is held to.

    This leads the report because space is what breaks these nodes. A card fills,
    a rewrite cannot get the scratch it needs, a write tears, the filesystem
    corrupts, and the box will not boot — that sequence is what caused four
    reflashes in eight days, not wear. The write rate below is the wear signal:
    real, but slower and far more forgiving.

    Every figure comes from the janitor's last pass. When it has not run, this
    says so and prints nothing rather than a total of zero, which would claim the
    agent occupies nothing at all.
    """
    total = janitor.get("footprint_bytes")
    budget = janitor.get("budget_bytes")
    click.echo(_ansi.marker(theme, "FOOTPRINT"))
    if not isinstance(total, int) or not isinstance(budget, int) or budget <= 0:
        why = janitor.get("reason") or "the janitor has not measured it yet"
        click.echo(_ansi.kv(theme, "total", theme.dim(f"- ({why})"), label_width=16))
        click.echo("")
        return

    pct = total * 100.0 / budget
    state = "fail" if total >= budget else ("warn" if pct >= 85.0 else "ok")
    click.echo(
        f"  {_ansi.dot(theme, state)} "
        f"{theme.bold(_fmt_bytes(total))} of {_fmt_bytes(budget)}  ({pct:.0f}%)"
    )

    caps = janitor.get("caps") if isinstance(janitor.get("caps"), dict) else {}
    over = janitor.get("over_cap") if isinstance(janitor.get("over_cap"), dict) else {}
    footprint = janitor.get("footprint") if isinstance(janitor.get("footprint"), dict) else {}
    for key, label in _JANITOR_CATEGORIES.items():
        used = footprint.get(key)
        if not isinstance(used, int) or used <= 0:
            continue
        cap = caps.get(key)
        detail = _fmt_bytes(used)
        if isinstance(cap, int) and cap > 0:
            detail += f" / {_fmt_bytes(cap)}"
        if isinstance(over.get(key), int) and over[key] > 0:
            # A category a floor is holding above its cap. Named rather than
            # hidden: the janitor declined to fix it and somebody may need to.
            detail = theme.fail(f"{detail}  over by {_fmt_bytes(over[key])}")
        click.echo(_ansi.kv(theme, "", theme.dim(f"{label}: {detail}"), label_width=4))

    installed = janitor.get("installed_bytes")
    if isinstance(installed, int) and installed > 0:
        # Stated as excluded rather than left for the reader to infer from the
        # arithmetic not adding up.
        click.echo(
            _ansi.kv(
                theme,
                "",
                theme.dim(f"installed agent: {_fmt_bytes(installed)} (not counted)"),
                label_width=4,
            )
        )
    click.echo("")


def _render_janitor(theme: Any, janitor: dict[str, Any]) -> None:
    """The disk janitor's last pass, and what is left for it to reclaim.

    A box can be writing gently and still fill up — the card that filled was not
    writing quickly, it was holding downloaded packages nothing removed. So the
    wear figures above are only half the answer and this is the other half.

    Absent numbers stay absent. A box whose janitor has never run must not print
    zeroes, because "nothing to reclaim" and "nobody has looked" are different
    answers and only one of them means the card is fine.
    """
    if not janitor:
        return
    click.echo("")
    click.echo(_ansi.marker(theme, "RECLAIM"))

    if janitor.get("ran") is not True:
        why = janitor.get("reason") or "the janitor has not run"
        click.echo(_ansi.kv(theme, "last pass", theme.dim(f"- ({why})"), 16))
        return

    rung = janitor.get("rung")
    age = janitor.get("age_s")
    when = f"{int(age) // 60} min ago" if isinstance(age, int) else "at an unknown time"
    detail = f"{rung} · {when}" if isinstance(rung, str) and rung else when
    click.echo(_ansi.kv(theme, "last pass", detail, 16))

    freed = janitor.get("reclaimed_bytes")
    if isinstance(freed, int):
        click.echo(_ansi.kv(theme, "freed", _fmt_bytes(freed), 16))
        for label, value in _category_rows(janitor.get("reclaimed")):
            click.echo(
                _ansi.kv(theme, "", theme.dim(f"{label}: {_fmt_bytes(value)}"), label_width=4)
            )
    else:
        click.echo(_ansi.kv(theme, "freed", theme.dim("- (not reported)"), 16))

    left = janitor.get("reclaimable_bytes")
    if isinstance(left, int):
        click.echo(_ansi.kv(theme, "still to give", _fmt_bytes(left), 16))
        for label, value in _category_rows(janitor.get("reclaimable")):
            click.echo(
                _ansi.kv(theme, "", theme.dim(f"{label}: {_fmt_bytes(value)}"), label_width=4)
            )
    else:
        click.echo(_ansi.kv(theme, "still to give", theme.dim("- (not reported)"), 16))

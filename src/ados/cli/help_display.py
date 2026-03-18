"""Rich-formatted help cheatsheet for ADOS Drone Agent CLI."""

from __future__ import annotations

from rich.console import Console
from rich.text import Text

from ados import __version__


def show_help() -> None:
    """Render the ADOS CLI cheatsheet with Rich formatting."""
    console = Console()

    blue = "#3A82FF"
    lime = "#DFF140"
    dim = "#A0A0A0"

    # ASCII art logo
    logo = Text()
    logo.append(
        "    _    ____   ___  ____\n"
        "   / \\  |  _ \\ / _ \\/ ___|\n"
        "  / _ \\ | | | | | | \\___ \\\n"
        " / ___ \\| |_| | |_| |___) |\n"
        "/_/   \\_\\____/ \\___/|____/\n",
        style=f"bold {blue}",
    )
    console.print(logo, highlight=False)

    subtitle = Text()
    subtitle.append("  D R O N E   A G E N T", style=f"bold {blue}")
    subtitle.append(f"   v{__version__}\n", style=f"{dim}")
    console.print(subtitle, highlight=False)

    def _header(label: str) -> Text:
        t = Text()
        t.append(f"  {label}", style=f"bold {lime}")
        return t

    def _cmd(name: str, desc: str, pad: int = 14) -> Text:
        t = Text()
        t.append(f"    {name:<{pad}}", style="bold white")
        t.append(desc, style=dim)
        return t

    # Row 1: INFO + FLIGHT
    console.print(
        Text.assemble(
            ("  INFO", f"bold {lime}"),
            (" " * 28, ""),
            ("FLIGHT", f"bold {lime}"),
        ),
        highlight=False,
    )
    pairs = [
        ("status", "Agent overview", "mavlink", "FC connection"),
        ("health", "CPU, RAM, disk", "link", "Video link stats"),
        ("diag", "Full diagnostics", "video", "Pipeline status"),
        ("version", "Print version", "snap", "Capture snapshot"),
    ]
    for left_cmd, left_desc, right_cmd, right_desc in pairs:
        t = Text()
        t.append(f"    {left_cmd:<14}", style="bold white")
        t.append(f"{left_desc:<20}", style=dim)
        t.append(f"  {right_cmd:<14}", style="bold white")
        t.append(right_desc, style=dim)
        console.print(t, highlight=False)

    console.print()

    # Row 2: SCRIPTING + CONFIG
    console.print(
        Text.assemble(
            ("  SCRIPTING", f"bold {lime}"),
            (" " * 23, ""),
            ("CONFIG", f"bold {lime}"),
        ),
        highlight=False,
    )
    pairs2 = [
        ("scripts", "List running", "config", "Show config"),
        ("run <p>", "Run Python script", "config <k>", "Get value"),
        ("send <c>", "Send text command", "set <k> <v>", "Set value"),
    ]
    for left_cmd, left_desc, right_cmd, right_desc in pairs2:
        t = Text()
        t.append(f"    {left_cmd:<14}", style="bold white")
        t.append(f"{left_desc:<20}", style=dim)
        t.append(f"  {right_cmd:<14}", style="bold white")
        t.append(right_desc, style=dim)
        console.print(t, highlight=False)

    console.print()

    # Row 3: TOOLS + PAIRING
    console.print(
        Text.assemble(
            ("  TOOLS", f"bold {lime}"),
            (" " * 27, ""),
            ("PAIRING", f"bold {lime}"),
        ),
        highlight=False,
    )
    # Merge into TOOLS left + PAIRING right, then SYSTEM below right
    tools_pairing = [
        ("tui", "Launch dashboard", "pair", "Show status/code"),
        ("start", "Start agent", "unpair", "Reset pairing"),
        ("demo", "Demo mode", "", ""),
        ("update", "OTA status", "", ""),
        ("check", "Check updates", "", ""),
        ("help", "This screen", "", ""),
    ]
    # Print first two rows with pairing, then switch right column to SYSTEM
    for i, (lc, ld, rc, rd) in enumerate(tools_pairing):
        if i == 3:
            # Insert SYSTEM header on right
            t = Text()
            t.append(f"    {lc:<14}", style="bold white")
            t.append(f"{ld:<20}", style=dim)
            t.append("  ", style="")
            t.append("SYSTEM", style=f"bold {lime}")
            console.print(t, highlight=False)
            continue
        if i == 4:
            rc, rd = "logs", "View logs"
        if i == 5:
            rc, rd = "uninstall", "Remove agent"
        t = Text()
        t.append(f"    {lc:<14}", style="bold white")
        t.append(f"{ld:<20}", style=dim)
        if rc:
            t.append(f"  {rc:<14}", style="bold white")
            t.append(rd, style=dim)
        console.print(t, highlight=False)

    console.print()

    # Footer hints
    hints = [
        ("  Flags:", " --json on most commands for machine output"),
        ("  Logs: ", " ados logs -f -n 100 (follow last 100 lines)"),
        ("  Config:", " ados config mavlink.baud (dot-path lookup)"),
    ]
    for label, detail in hints:
        t = Text()
        t.append(label, style=f"bold {lime}")
        t.append(detail, style=dim)
        console.print(t, highlight=False)

    console.print()

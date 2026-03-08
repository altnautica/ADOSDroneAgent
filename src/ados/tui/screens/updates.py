"""OTA Updates TUI screen: version, update status, download progress."""

from __future__ import annotations

import httpx
import structlog
from textual.app import ComposeResult
from textual.containers import Vertical
from textual.screen import Screen
from textual.widgets import Static

log = structlog.get_logger("tui.updates")


class UpdatesScreen(Screen):
    """Displays OTA update status, active/standby slot info, and download progress."""

    def compose(self) -> ComposeResult:
        with Vertical():
            yield Static("[b]OTA Updates[/b]", classes="panel-title")
            yield Static("Loading...", id="version-panel")
            yield Static("[b]Partition Slots[/b]", classes="panel-title")
            yield Static("Loading...", id="slots-panel")
            yield Static("[b]Download Progress[/b]", classes="panel-title")
            yield Static("No active download", id="download-panel")

    def on_mount(self) -> None:
        self.set_interval(5.0, self._refresh)

    async def _refresh(self) -> None:
        api = self.app.api_url  # type: ignore[attr-defined]
        try:
            async with httpx.AsyncClient(timeout=3.0) as client:
                resp = await client.get(f"{api}/api/ota")
                data = resp.json()
        except httpx.ConnectError:
            self.query_one("#version-panel", Static).update("Agent not running")
            return
        except Exception as exc:
            log.warning("updates_refresh_failed", error=str(exc))
            self.query_one("#version-panel", Static).update("Error loading data")
            return

        # Version and state
        state = data.get("state", "unknown")
        version = data.get("current_version", "?")
        error = data.get("error", "")

        version_text = f"Version:  {version}\nState:    {state}"
        if error:
            version_text += f"\nError:    {error}"

        pending = data.get("pending_update")
        if pending:
            version_text += (
                f"\n\nUpdate Available: v{pending.get('version', '?')}"
                f"\nChannel: {pending.get('channel', '?')}"
                f"\nChangelog: {pending.get('changelog', '')[:120]}"
            )

        self.query_one("#version-panel", Static).update(version_text)

        # Slots
        slots = data.get("slots", {})
        active = slots.get("active_slot", {})
        standby = slots.get("standby_slot", {})

        slots_text = (
            f"Active:   slot-{active.get('slot_name', '?')}"
            f"  v{active.get('version', '?')}"
            f"  boots={active.get('boot_count', 0)}\n"
            f"Standby:  slot-{standby.get('slot_name', '?')}"
            f"  v{standby.get('version', '?')}"
            f"  status={standby.get('status', '?')}"
        )
        if slots.get("should_rollback"):
            slots_text += "\n[red]WARNING: Boot failures detected, rollback recommended[/red]"

        self.query_one("#slots-panel", Static).update(slots_text)

        # Download
        dl = data.get("download", {})
        dl_state = dl.get("state", "idle")
        if dl_state == "downloading":
            pct = dl.get("percent", 0)
            speed = dl.get("speed_bps", 0)
            eta = dl.get("eta_seconds", 0)
            downloaded = dl.get("bytes_downloaded", 0)
            total = dl.get("total_bytes", 0)

            speed_kb = speed / 1024
            bar_width = 30
            filled = int(bar_width * pct / 100)
            bar = "#" * filled + "-" * (bar_width - filled)

            dl_text = (
                f"[{bar}] {pct:.1f}%\n"
                f"Downloaded: {downloaded:,} / {total:,} bytes\n"
                f"Speed: {speed_kb:.1f} KB/s    ETA: {eta:.0f}s"
            )
        elif dl_state == "completed":
            dl_text = "Download complete"
        elif dl_state == "failed":
            dl_text = "Download failed"
        else:
            dl_text = "No active download"

        self.query_one("#download-panel", Static).update(dl_text)

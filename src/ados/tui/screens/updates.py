"""OTA Updates TUI screen: version, update status, download progress."""

from __future__ import annotations

import structlog
from textual.app import ComposeResult
from textual.containers import Vertical
from textual.screen import Screen
from textual.widgets import Static

from ados.tui.widgets import GaugeBar, InfoPanel, StatusDot

log = structlog.get_logger("tui.updates")


class UpdatesScreen(Screen):
    """Displays OTA update status, update info, and download progress."""

    def compose(self) -> ComposeResult:
        with Vertical():
            with InfoPanel("VERSION"):
                yield StatusDot("OTA", "unknown", id="ota-state-dot")
                yield Static("", id="version-detail")
            with InfoPanel("UPDATE INFO"):
                yield Static("", id="update-info-detail")
            with InfoPanel("DOWNLOAD"):
                yield GaugeBar(
                    label="DL",
                    value=0,
                    thresholds=(100.0, 100.0),  # always green for downloads
                    id="dl-gauge",
                )
                yield Static("No active download", id="download-detail")

    def on_mount(self) -> None:
        self.set_interval(5.0, self._refresh)

    async def _refresh(self) -> None:
        fetcher = self.app.fetcher  # type: ignore[attr-defined]
        data = await fetcher.get_ota()

        ota_dot = self.query_one("#ota-state-dot", StatusDot)

        if data is None:
            ota_dot.set_state("disconnected")
            return

        # Version and state
        state = data.get("state", "unknown")
        version = data.get("current_version", "?")
        error = data.get("error", "")

        # Map OTA state to dot state
        state_map = {
            "idle": "ok",
            "checking": "connecting",
            "downloading": "warning",
            "verifying": "connecting",
            "installing": "warning",
            "restarting": "warning",
            "completed": "ok",
            "failed": "error",
        }
        ota_dot.set_state(state_map.get(state, "unknown"))

        version_lines = [f"Version:  {version}", f"State:    {state}"]
        if error:
            version_lines.append(f"[red]Error:    {error}[/red]")

        pending = data.get("pending_update")
        if pending:
            version_lines.append("")
            ver = pending.get('version', '?')
            version_lines.append(
                f"[#dff140]Update Available: v{ver}[/#dff140]",
            )
            version_lines.append(f"Channel: {pending.get('channel', '?')}")
            changelog = pending.get("changelog", "")[:120]
            if changelog:
                version_lines.append(f"Notes:   {changelog}")

        self.query_one("#version-detail", Static).update("\n".join(version_lines))

        # Update info
        channel = data.get("channel", "?")
        repo = data.get("github_repo", "?")
        last_check = data.get("last_check", "") or "never"
        prev_version = data.get("previous_version", "") or "none"

        info_lines = [
            f"Channel:  {channel}",
            f"Repo:     {repo}",
            f"Last check: {last_check}",
            f"Previous: {prev_version}",
        ]
        self.query_one("#update-info-detail", Static).update("\n".join(info_lines))

        # Download
        dl = data.get("download", {})
        dl_state = dl.get("state", "idle")
        dl_gauge = self.query_one("#dl-gauge", GaugeBar)

        if dl_state == "downloading":
            pct = dl.get("percent", 0)
            speed = dl.get("speed_bps", 0)
            eta = dl.get("eta_seconds", 0)
            downloaded = dl.get("bytes_downloaded", 0)
            total = dl.get("total_bytes", 0)
            speed_kb = speed / 1024

            dl_gauge.update_value(pct)

            dl_text = (
                f"Downloaded: {downloaded:,} / {total:,} bytes\n"
                f"Speed: {speed_kb:.1f} KB/s    ETA: {eta:.0f}s"
            )
        elif dl_state == "completed":
            dl_gauge.update_value(100)
            dl_text = "[#22c55e]Download complete[/#22c55e]"
        elif dl_state == "failed":
            dl_gauge.update_value(0)
            dl_text = "[red]Download failed[/red]"
        else:
            dl_gauge.update_value(0)
            dl_text = "No active download"

        self.query_one("#download-detail", Static).update(dl_text)

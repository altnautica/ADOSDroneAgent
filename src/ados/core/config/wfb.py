"""WFB-ng radio link configuration."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, model_validator


class WfbConfig(BaseModel):
    interface: str = ""
    # Fleet this node belongs to. Every node that must hear every other
    # node shares it. 1..65535; 0 is reserved as "unprovisioned" and is
    # rejected at radio load time. Combined with fleet_slot it fills the
    # wfb-ng 24-bit link_id space exactly (link_id = fleet_id << 8 |
    # fleet_slot), so two fleets can share one channel with no
    # channel_id collision.
    fleet_id: int = 1
    # This node's slot within the fleet. 0 = ground station, 1..24 =
    # drones. Every drone's slot MUST be unique within a fleet: two
    # transmitters sharing a channel_id re-init each other's FEC decoder
    # about once a second, which presents as unexplained link loss.
    # Issued by the ground station's fleet registry at pair time and
    # written into this config; never negotiated at runtime.
    fleet_slot: int = 0
    # Home / rendezvous channel. Both drone and ground start here and
    # return here on link loss, so the two sides deterministically meet
    # before any hopping. 149 (U-NII-3) is non-DFS and enabled under
    # essentially every regulatory domain, unlike U-NII-1 (36-48) which
    # many domains disable for injection.
    channel: int = 149
    # Regulatory domain applied via ``iw reg set`` on both rigs BEFORE the
    # radio comes up in monitor mode, so the drone and ground enable the
    # same channel set at usable TX power. Defaults to a domain that
    # permits the home channel (149, U-NII-3 / 5745 MHz, non-DFS) at full
    # power; the kernel's startup domain (or a country IE pushed by an
    # associated management WiFi) can otherwise forbid 5745 and cap TX to
    # the -100 dBm "not permitted" sentinel. Operators in a region where
    # this channel needs a different code set their own (e.g.
    # ``video.wfb.reg_domain: 'DE'``); the kernel then maps whatever that
    # domain allows. A None/empty value falls back to the same default.
    reg_domain: str | None = "US"
    # TX power in dBm. RTL8812EU + USB host VBUS topology browns out
    # the dongle above ~18 dBm sustained. Default is the floor for
    # bench bring-up; raise via PUT /api/wfb/tx-power once the link is
    # validated. Hard ceiling is enforced at validation time.
    tx_power_dbm: int = 5
    tx_power_max_dbm: int = 15
    # MCS index passed to wfb_tx -M. Default 1 (low-bitrate, robust).
    # Distinct from tx_power_dbm — earlier code conflated the two.
    mcs_index: int = 1
    # Power-supply topology hint for the WFB radio. Drives the brownout
    # warning in GCS/LCD. host_vbus = USB-A VBUS straight to dongle
    # VDD5.0 (default; what most bench rigs do). powered_hub = external
    # 5 V hub between SBC and dongle. external_5v = dongle has its own
    # 5 V rail wired directly.
    topology: Literal["host_vbus", "powered_hub", "external_5v"] = "host_vbus"
    fec_k: int = 8
    fec_n: int = 12
    # Frequency-band whitelist used by ``select_quietest_channel`` when
    # ``auto_channel_enabled`` is true and for post-link hop candidates.
    # Default U-NII-3 (5745-5825, channels 149-165): non-DFS and enabled
    # under essentially every regulatory domain, so a drone and ground
    # with mismatched regdomains still share a usable channel set. U-NII-1
    # (36-48) is often quieter but many domains disable it for injection
    # (the kernel rejects the channel with -22), which strands a ground
    # station that cannot follow the drone there. ``all`` considers every
    # standard channel without a band filter.
    band: Literal["u-nii-1", "u-nii-3", "all"] = "u-nii-3"
    # When true, the agent scans the configured band on every fresh
    # bind and writes the quietest channel into the persisted config
    # before bringing wfb_tx / wfb_rx up. Default false: the rig stays
    # on the home ``channel`` so the drone and ground deterministically
    # rendezvous there before any hopping. Scanning-and-relocating at
    # bind is what let the two sides pick different channels and never
    # meet. The scan is an `iw scan` round-trip (~1-3 s), at bind time
    # only, never on the steady-state link health tick.
    auto_channel_enabled: bool = False
    # When true, the agent's auto_pair supervisor opens a local bind
    # window on first boot and pairs to whichever unpaired peer responds
    # first on the radio. Flips to false the moment a pair lands so the
    # rig does not silently re-bind to another device after an unpair.
    # Re-enabling requires explicit operator action (REST / CLI / GCS).
    auto_pair_enabled: bool = True
    # Peer device-id and pair timestamp persist on both profiles (drone
    # holds the GS device-id, GS holds the drone device-id). The
    # ground-station-side fields under ground_station.paired_drone_id
    # remain for backward compat with field rigs running older configs;
    # the canonical surface for fresh installs is here.
    paired_with_device_id: str | None = None
    paired_at: str | None = None  # iso timestamp
    # Inject H.264 SEI markers carrying time.time_ns() into the wfb-tee
    # output so the ground side can compute over-the-air video
    # latency. Adds ~30 bytes per VCL NAL (~900 B/s at 30 fps),
    # negligible vs a 4 Mbps stream. On by default so the LCD shows
    # camera→display latency out of the box and the GCS popover can
    # compute true end-to-end via the browser-side SEI parser.
    # To disable, set sei_latency: false in /etc/ados/config.yaml
    # under video.wfb and restart the agent.
    sei_latency: bool = True
    # Operator-facing radio link preset. The WfbManager reads this at
    # startup and overrides mcs_index / fec_k / fec_n with the preset
    # values. Lets a bench operator widen the link without remembering
    # the right K/N/MCS combinations.
    #
    #   conservative (default): MCS=1, FEC=8/12. Low TX power, noisy
    #     bench, 200m range. Safe under host_vbus topology.
    #   balanced: MCS=3, FEC=8/12. Good outdoor link, 500m+, headroom
    #     for RSSI swings. Recommended once topology is powered_hub.
    #   aggressive: MCS=5, FEC=8/10. Excellent SNR, close-in, max
    #     throughput. Will drop the link on a noisy channel.
    #
    # When the preset is left at the default "conservative", the
    # manager respects whatever values are explicitly set on
    # mcs_index / fec_k / fec_n above (so an existing rig with custom
    # values is unaffected by adding the preset field).
    wfb_link_preset: Literal[
        "conservative", "balanced", "aggressive"
    ] = "conservative"
    # Closed-loop adaptive bitrate + FEC + MCS controller. When true, a
    # 1 Hz background controller watches the link quality monitor and
    # drives two ladders: a four-tier bitrate/FEC ladder
    # (4 Mbps/8-12 -> 3 Mbps/8-14 -> 2 Mbps/8-16 -> 1.2 Mbps/4-12) on
    # packet loss + RSSI hysteresis, and a five-rung MCS ladder on
    # measured SNR. Each rung's bitrate is applied to the video encoder
    # as a ceiling, so a degrading link emits FEWER on-air bytes.
    # Changes go to the RUNNING wfb_tx over its wfb-ng 24.08 management
    # socket (bound with -C), so the routine case costs no pipeline
    # blackout; the historical kill-and-respawn (~1-2 s of blackout) is
    # only the fallback when that socket does not answer. The controller
    # still paces itself so the link can settle before the next decision.
    # Default on: a drone-only rig with no received-side stats holds
    # the rung (cold-start guard), so it is inert until a ground
    # station is present. Pin a manual tier via REST or GCS to disable.
    adaptive_bitrate_enabled: bool = True
    # Cap on the top rung the adaptive MCS ladder may select. The ladder
    # is SNR >= 30 dB -> MCS 5, >= 24 -> 4, >= 21 -> 3, >= 15 -> 2, else
    # MCS 1, holding ~10 dB of margin over each rung's required SNR.
    # Range 1..5; values outside are clamped at radio load time.
    #
    # Default 3, not the ladder's ceiling of 5, on purpose: the rung
    # table is arithmetic from standard 802.11n required-SNR figures,
    # not a measurement of the vendored rtl8812eu driver, and OpenIPC's
    # adaptive-link (the working production reference for this loop)
    # ships a profile table that tops out at MCS 2 in the field.
    # Anything above 3 is bench-only until the characterisation sweep in
    # product/specs/ados-agent-rust-hybrid/wfb-video-pipeline-runbook.md
    # measures which rungs this driver actually applies and holds.
    adaptive_mcs_max: int = 3
    # Periodic + reactive coordinated frequency hopping. Operator
    # picks a band (the existing `band` field above) and the agent
    # autonomously moves the WFB-ng link to the quietest channel
    # inside that band on a periodic timer or when the link
    # degrades. Drone-side broadcasts an authenticated
    # HopAnnounce on the reserved control port; GS-side listens
    # and flips synchronously. Self-gating: the drone only flips
    # after it sees a peer ACK, so a half-upgraded pair does not
    # silently lose its link. Default on — the operator does not
    # need to think about channel selection. Disable to pin the
    # link to a fixed channel.
    auto_hop_enabled: bool = True
    # Period in seconds between routine "is there a quieter
    # channel?" rescans. Tuned for the bench: 60 s feels invisible
    # to the operator (one ~300 ms freeze per minute of flight)
    # without sitting on a degraded channel for too long.
    hop_period_seconds: int = 60
    # Reactive hop thresholds. The supervisor triggers an
    # off-schedule migration when the live link quality sample
    # crosses either threshold AND the link has been stable on
    # the current channel for at least 30 s.
    hop_loss_threshold_percent: float = 10.0
    hop_rssi_threshold_dbm: float = -75.0

    @model_validator(mode="before")
    @classmethod
    def _migrate_legacy_tx_power(cls, values):
        """Bridge the old `tx_power` YAML field to `tx_power_dbm`.

        Earlier releases shipped `tx_power: 25` but fed the value to
        `wfb_tx -M`, which is the MCS index, not radio power. Real TX
        power was never set; the dongle ran at driver default (often
        17-20 dBm, the brownout band on host-VBUS topology). The legacy
        value is therefore meaningless and is dropped, not migrated.
        Operators get the new safe default unless they have already
        written `tx_power_dbm` explicitly.
        """
        if not isinstance(values, dict):
            return values
        if "tx_power" in values and "tx_power_dbm" not in values:
            values.pop("tx_power", None)
        elif "tx_power" in values:
            # Both present — drop the legacy alias, keep the new field.
            values.pop("tx_power", None)
        return values

    @model_validator(mode="after")
    def _clamp_tx_power(self):
        if self.tx_power_dbm > self.tx_power_max_dbm:
            self.tx_power_dbm = self.tx_power_max_dbm
        if self.tx_power_dbm < 1:
            self.tx_power_dbm = 1
        return self

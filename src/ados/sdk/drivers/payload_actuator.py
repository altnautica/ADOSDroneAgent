"""Payload actuator driver base class.

A payload actuator driver triggers a discrete or continuous mechanism
attached to the airframe: sprayer pump, dropper servo, claw, sampler,
parachute, beacon. Actions are addressed by id and accept a free-form
argument bag so vendor-specific payloads can model their own command
surface.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class PayloadCandidate:
    """A payload actuator a driver claims it can open."""

    driver_id: str
    device_id: str
    label: str
    bus: str
    vid_pid: tuple[int, int] | None = None
    metadata: dict[str, Any] | None = None


@dataclass(frozen=True)
class PayloadCapabilities:
    """Static capabilities of an open payload actuator session.

    ``actions`` is the menu of action ids the driver accepts. The host
    uses this to populate the GCS payload panel and to validate
    :meth:`PayloadActuatorDriver.actuate` calls before they reach the
    driver subprocess.
    """

    actions: tuple[str, ...]
    has_position_feedback: bool = False
    has_flow_feedback: bool = False
    metadata: dict[str, Any] | None = None


@dataclass(frozen=True)
class PayloadCommand:
    """A single actuation request.

    ``action_id`` is one of the ids declared in :class:`PayloadCapabilities`.
    ``args`` carries action-specific parameters (duration, volume, angle).
    """

    action_id: str
    args: dict[str, Any]


@dataclass(frozen=True)
class PayloadState:
    """A snapshot of payload state after actuation."""

    timestamp_ns: int
    last_action_id: str | None
    busy: bool
    metadata: dict[str, Any] | None = None


class PayloadSession:
    """Opaque per-open state held by a payload driver."""


class PayloadActuatorDriver(ABC):
    """Abstract base for payload actuator drivers."""

    @abstractmethod
    async def discover(self) -> list[PayloadCandidate]:
        """Scan for payload actuators this driver can open."""

    @abstractmethod
    async def open(
        self, candidate: PayloadCandidate, config: dict[str, Any]
    ) -> PayloadSession:
        """Open a session against a payload actuator."""

    @abstractmethod
    async def close(self, session: PayloadSession) -> None:
        """Release resources held by a session."""

    @abstractmethod
    def capabilities(self, session: PayloadSession) -> PayloadCapabilities:
        """Return the static capabilities of an open session."""

    @abstractmethod
    async def actuate(
        self, session: PayloadSession, command: PayloadCommand
    ) -> None:
        """Execute one payload action.

        Raises :class:`ValueError` if ``command.action_id`` is not in
        :attr:`PayloadCapabilities.actions` so the host can reject the
        request before it reaches hardware.
        """

    @abstractmethod
    def get_state(self, session: PayloadSession) -> PayloadState:
        """Return the most recent payload state snapshot."""

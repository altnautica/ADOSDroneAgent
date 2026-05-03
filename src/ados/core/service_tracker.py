"""In-process service state tracking shared by legacy and API runtimes."""

from __future__ import annotations

import time
from enum import StrEnum

from ados.core.logging import get_logger

log = get_logger("service_tracker")


class ServiceState(StrEnum):
    """Lifecycle states for managed services."""

    STOPPED = "stopped"
    STARTING = "starting"
    RUNNING = "running"
    DEGRADED = "degraded"
    FAILED = "failed"


class ServiceTracker:
    """Tracks state and transitions for all agent services."""

    def __init__(self) -> None:
        self._states: dict[str, ServiceState] = {}
        self._transitions: dict[str, list[tuple[float, ServiceState]]] = {}

    def set_state(self, name: str, state: ServiceState) -> None:
        """Transition a service to a new state, recording the timestamp."""
        prev = self._states.get(name)
        self._states[name] = state

        if name not in self._transitions:
            self._transitions[name] = []
        self._transitions[name].append((time.monotonic(), state))

        if prev != state:
            log.info(
                "service_state_change",
                service=name,
                from_state=str(prev),
                to_state=state.value,
            )

    def get_state(self, name: str) -> ServiceState:
        """Get the current state of a service."""
        return self._states.get(name, ServiceState.STOPPED)

    def get_all(self) -> dict[str, ServiceState]:
        """Return a copy of all service states."""
        return dict(self._states)

    def get_transitions(self, name: str) -> list[tuple[float, ServiceState]]:
        """Return recorded state transitions for a given service."""
        return list(self._transitions.get(name, []))

    def to_dict(self) -> dict[str, dict]:
        """Serialize all service states for the REST API."""
        result: dict[str, dict] = {}
        for name, state in self._states.items():
            transitions = self._transitions.get(name, [])
            last_transition = transitions[-1][0] if transitions else 0
            result[name] = {
                "state": state.value,
                "last_transition": last_transition,
                "transition_count": len(transitions),
            }
        return result

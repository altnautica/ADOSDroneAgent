"""Pairing manager for ADOS Drone Agent."""

from __future__ import annotations

import contextlib
import fcntl
import json
import os
import secrets
import time
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

from ados.core.atomic import atomic_write_json
from ados.core.logging import get_logger
from ados.core.paths import PAIRING_JSON

log = get_logger("pairing")

# Safe charset: no ambiguous chars (0/O/1/I/L)
SAFE_CHARSET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789"
CODE_LENGTH = 6
# An unpaired agent regenerates its pair code every CODE_TTL seconds
# (the beacon loop calls get_or_create_code on each iteration and the
# stale-timestamp branch fires once the window passes). Keep this
# generous: the operator reads the code off `ados status`, the install
# banner, or the LCD, and we cannot assume they will walk over to
# Mission Control inside the next 15 minutes. 24 hours is long enough
# for a bench session that spans a workday, short enough that an
# abandoned-unpaired agent eventually rolls.
CODE_TTL = 24 * 60 * 60
PAIRING_STATE_PATH = str(PAIRING_JSON)
# How long a writer waits for another writer's lock before giving up. A holder
# keeps it for one read plus one atomic write.
WRITE_LOCK_TIMEOUT_S = 10.0
_LOCK_POLL_S = 0.02


def lock_path_for(state_path: Path) -> Path:
    """The lock file serialising writers of ``state_path``: the sibling
    ``<name>.lock``. The native control front takes the same lock, so a code
    regeneration here can never overwrite a claim made there."""
    return state_path.with_name(state_path.name + ".lock")


class PairingStateUnreadable(RuntimeError):
    """The pairing file exists but cannot be read; a writer refuses to replace
    it, since it may hold a live pairing."""


def _normalize_convex_site_url(url: str) -> str:
    """Normalize an operator-entered Convex URL toward the SITE (HTTP-actions)
    origin where ``/pairing/register`` is served.

    Convex serves client functions on the BACKEND origin and HTTP actions on the
    SITE origin. On the managed cloud these are different hostnames
    (``convex.altnautica.com`` vs ``convex-site.altnautica.com``); on a
    self-hosted deployment they are the same host on different ports — the
    backend on ``:3210`` and the site on ``:3211``. An operator who pastes the
    backend URL would 404 the register call, so map a known backend coordinate to
    its site sibling. A URL that is already a site origin (or one we cannot
    confidently rewrite) is returned unchanged so we never break a correct entry.
    """
    cleaned = (url or "").strip().rstrip("/")
    if not cleaned:
        return ""
    # Self-hosted: the backend runs on :3210, the HTTP-actions site on :3211.
    if ":3210" in cleaned:
        return cleaned.replace(":3210", ":3211")
    # Managed cloud: the backend host has a `-site` sibling for HTTP actions.
    # Only rewrite the exact managed backend host; leave anything else alone.
    if "://convex.altnautica.com" in cleaned:
        return cleaned.replace("://convex.altnautica.com", "://convex-site.altnautica.com")
    return cleaned


class PairingManager:
    """Manages pairing state, code generation, and API key validation.

    The agent runs three processes that each instantiate this class
    (ados-api, ados-cloud, ados-supervisor). Without cross-process
    coordination they would each carry their own in-memory snapshot
    of ``pairing.json`` and diverge from disk as soon as one of them
    rotated the pair code. The mtime-tracked reload below keeps all
    three convergent within one public-read cycle.
    """

    def __init__(self, state_path: str = PAIRING_STATE_PATH):
        self._state_path = Path(state_path)
        self._state: dict = {}
        self._last_loaded_mtime: float = 0.0
        self._load_state()

    def _load_state(self) -> None:
        if self._state_path.exists():
            try:
                self._state = json.loads(self._state_path.read_text())
                log.info("pairing_state_loaded", paired=self._state.get("paired", False))
            except (json.JSONDecodeError, OSError) as e:
                log.warning("pairing_state_load_failed", error=str(e))
                self._state = {}
        else:
            self._state = {}
        try:
            self._last_loaded_mtime = self._state_path.stat().st_mtime
        except OSError:
            self._last_loaded_mtime = 0.0

    def _maybe_reload(self) -> None:
        """Reload state when the on-disk pairing.json is newer than the
        copy we have in memory.

        Cheap: one stat() call per public read. The file is ~200 bytes,
        the reload itself only fires on mtime change. Without this the
        reading process serves stale state forever — the symptom that
        bit us when ados-cloud rotated the code and ados-api kept
        advertising the old one through /api/pairing/code, /api/pairing/info,
        and `ados status`.
        """
        try:
            mtime = self._state_path.stat().st_mtime
        except OSError:
            return
        if mtime > self._last_loaded_mtime:
            self._load_state()

    def _save_state(self) -> None:
        atomic_write_json(self._state_path, self._state, indent=2)
        try:
            self._last_loaded_mtime = self._state_path.stat().st_mtime
        except OSError:
            pass
        log.debug("pairing_state_saved")

    @contextlib.contextmanager
    def _write_lock(self) -> Iterator[None]:
        """Hold the cross-process pairing write lock (``flock`` on the sibling
        ``.lock`` file, bounded wait)."""
        lock_path = lock_path_for(self._state_path)
        lock_path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(lock_path, os.O_RDWR | os.O_CREAT, 0o600)
        try:
            deadline = time.monotonic() + WRITE_LOCK_TIMEOUT_S
            while True:
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    if time.monotonic() >= deadline:
                        raise TimeoutError(f"pairing lock {lock_path} held too long") from None
                    time.sleep(_LOCK_POLL_S)
            try:
                yield
            finally:
                fcntl.flock(fd, fcntl.LOCK_UN)
        finally:
            os.close(fd)

    def _mutate(self, change: Callable[[dict[str, Any]], bool]) -> None:
        """Read-modify-write ``pairing.json`` under the write lock.

        The state is re-read from disk inside the lock, never taken from this
        process's cached copy: another process (the native front's claim, a
        sibling Python process) may have written since. ``change`` edits the
        fresh state in place and returns whether anything needs saving. A file
        that exists but cannot be parsed is never overwritten.
        """
        with self._write_lock():
            if self._state_path.exists():
                try:
                    fresh = json.loads(self._state_path.read_text())
                except (json.JSONDecodeError, OSError) as e:
                    raise PairingStateUnreadable(str(e)) from e
                if not isinstance(fresh, dict):
                    raise PairingStateUnreadable("pairing state is not an object")
            else:
                fresh = {}
            self._state = fresh
            if change(self._state):
                self._save_state()

    @staticmethod
    def generate_code() -> str:
        """Generate a human-friendly pairing code."""
        return "".join(secrets.choice(SAFE_CHARSET) for _ in range(CODE_LENGTH))

    @staticmethod
    def generate_api_key() -> str:
        """Generate a secure API key with ados_ prefix."""
        return "ados_" + secrets.token_urlsafe(32)

    @property
    def is_paired(self) -> bool:
        self._maybe_reload()
        return self._state.get("paired", False)

    @property
    def api_key(self) -> str | None:
        self._maybe_reload()
        return self._state.get("api_key") if self._state.get("paired", False) else None

    @property
    def owner_id(self) -> str | None:
        self._maybe_reload()
        return self._state.get("owner_id") if self._state.get("paired", False) else None

    def get_or_create_code(self) -> str:
        """Get current pairing code, or generate a new one if expired.

        Read and written under the pairing write lock against the state on
        disk, so a sibling process's fresh code is returned verbatim and a
        claim that landed meanwhile is never undone. A paired node has no code:
        the empty string is returned and nothing is written.
        """
        self._maybe_reload()
        code = self._state.get("pairing_code")
        created_at = self._state.get("code_created_at", 0)
        if (
            not self._state.get("paired")
            and code
            and (time.time() - created_at) < CODE_TTL
            and self._state.get("pending_api_key")
        ):
            return code

        result: dict[str, str] = {}

        def change(state: dict[str, Any]) -> bool:
            if state.get("paired"):
                result["code"] = ""
                return False
            changed = False
            current = state.get("pairing_code")
            if not current or (time.time() - state.get("code_created_at", 0)) >= CODE_TTL:
                current = self.generate_code()
                state["pairing_code"] = current
                state["code_created_at"] = time.time()
                log.info("pairing_code_generated", code=current)
                changed = True
            # The code and the pending key always travel together (see
            # get_or_create_api_key).
            if not state.get("pending_api_key"):
                state["pending_api_key"] = self.generate_api_key()
                log.info("pairing_api_key_generated")
                changed = True
            result["code"] = current
            return changed

        self._mutate(change)
        return result["code"]

    def get_or_create_api_key(self) -> str:
        """Return a stable API key for the current pending pair attempt.

        The pair beacon calls this once per iteration. Without caching
        the agent posts a different key every 30 s; the cloud relay
        freezes whichever key happens to be in flight at claim time,
        and the agent's later transition to paired uses the very latest
        key — so cmd_drones.apiKey and pairing.json.api_key drift apart
        and every heartbeat after the claim 401s permanently.

        Persists the key on disk (under the pairing write lock) so a restart
        or sibling process picks up the same value; ``claim()`` or
        ``unpair()`` clears it.
        """
        self._maybe_reload()
        if self._state.get("paired"):
            return self._state.get("api_key", "")
        cached = self._state.get("pending_api_key")
        if cached:
            return cached

        result: dict[str, str] = {}

        def change(state: dict[str, Any]) -> bool:
            if state.get("paired"):
                result["key"] = state.get("api_key", "")
                return False
            existing = state.get("pending_api_key")
            if existing:
                result["key"] = existing
                return False
            state["pending_api_key"] = self.generate_api_key()
            result["key"] = state["pending_api_key"]
            log.info("pairing_api_key_generated")
            return True

        self._mutate(change)
        return result["key"]

    def claim(self, user_id: str, api_key: str | None = None) -> str:
        """Claim this agent for a user. Returns API key.

        Prefers the cached ``pending_api_key`` over generating a fresh
        one so the key the agent advertised through the beacon stays
        the same key the cloud-relay handler validates against. The
        already-paired check runs under the write lock against the state on
        disk, so two concurrent claims cannot both succeed.
        """
        result: dict[str, str] = {}

        def change(state: dict[str, Any]) -> bool:
            if state.get("paired"):
                raise ValueError("Already paired. Unpair first.")
            key = api_key or state.get("pending_api_key") or self.generate_api_key()
            state["paired"] = True
            state["api_key"] = key
            state["owner_id"] = user_id
            state["paired_at"] = time.time()
            state.pop("pairing_code", None)
            state.pop("code_created_at", None)
            state.pop("pending_api_key", None)
            result["key"] = key
            return True

        self._mutate(change)
        log.info("pairing_claimed", user_id=user_id)
        return result["key"]

    def unpair(self) -> None:
        """Clear pairing state. The cached pending key, code, paired flag and
        owner all drop."""
        previous: dict[str, object] = {}
        with self._write_lock():
            # Unpair is how an unreadable file is recovered, so it does not
            # read the old state strictly: it replaces it.
            try:
                previous["owner"] = json.loads(self._state_path.read_text()).get("owner_id")
            except (json.JSONDecodeError, OSError, AttributeError):
                previous["owner"] = None
            self._state = {}
            self._save_state()
        log.info("pairing_unpaired", previous_owner=previous.get("owner"))

    def validate_key(self, key: str) -> bool:
        """Check if a given API key matches the stored one."""
        self._maybe_reload()
        if not self._state.get("paired", False):
            return True  # When unpaired, all access is open
        return key == self._state.get("api_key")

    def get_info(self) -> dict:
        """Get pairing info for the /pairing/info endpoint."""
        self._maybe_reload()
        if self._state.get("paired", False):
            return {
                "paired": True,
                "owner_id": self._state.get("owner_id"),
                "paired_at": self._state.get("paired_at"),
            }
        return {
            "paired": False,
            "pairing_code": self.get_or_create_code(),
        }

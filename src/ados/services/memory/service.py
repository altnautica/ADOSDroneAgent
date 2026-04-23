"""World Model memory service — main module.

Opens the SQLite database, starts the ingest pipeline,
starts the entity merger, and registers REST routes.
"""

from __future__ import annotations

import asyncio
import os
import secrets
import signal
import socket
import time
from pathlib import Path

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging

from .capture_rules import RulesEngine
from .entity_merger import EntityMerger
from .ingest import IngestPipeline
from .schema import open_db, upsert_flight
from .writer import WriteBatcher

log = structlog.get_logger()


def _sd_notify(message: bytes) -> None:
    addr = os.environ.get("NOTIFY_SOCKET", "/run/systemd/notify")
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        s.sendto(message, addr)
        s.close()
    except OSError:
        pass


class MemoryService:
    """Manages the World Model database and ingest pipeline."""

    def __init__(self) -> None:
        self.config = load_config()
        self.conn = None
        self.batcher: WriteBatcher | None = None
        self.ingest: IngestPipeline | None = None
        self.merger: EntityMerger | None = None
        self.rules_engine = RulesEngine()
        self._flight_id: str = secrets.token_hex(8)
        self._flight_started = False

    def open(self) -> None:
        cfg = self.config.memory
        self.conn = open_db(cfg.db_path, create=True)
        self.batcher = WriteBatcher(self.conn)
        thumb_dir = Path(cfg.thumbs_dir)
        fullres_dir = Path(cfg.fullres_dir)
        thumb_dir.mkdir(parents=True, exist_ok=True)
        fullres_dir.mkdir(parents=True, exist_ok=True)

        self.ingest = IngestPipeline(
            batcher=self.batcher,
            rules_engine=self.rules_engine,
            flight_id=self._flight_id,
            thumb_dir=thumb_dir,
            fullres_dir=fullres_dir,
        )
        self.merger = EntityMerger(self.conn)

    def start_flight(self) -> str:
        self._flight_id = secrets.token_hex(8)
        self._flight_started = True
        upsert_flight(
            self.conn,
            self._flight_id,
            start_ts=time.time(),
            operator="agent",
        )
        if self.ingest:
            self.ingest.set_flight_id(self._flight_id)
        log.info("world_model_flight_started", flight_id=self._flight_id)
        return self._flight_id

    def end_flight(self) -> None:
        if not self._flight_started:
            return
        self._flight_started = False
        if self.conn:
            with self.conn:
                self.conn.execute(
                    "UPDATE flights SET end_ts = ? WHERE id = ?",
                    (time.time(), self._flight_id),
                )
            # Run a final entity merge pass
            if self.merger:
                self.merger.run_pass(flight_id=self._flight_id)
        log.info("world_model_flight_ended", flight_id=self._flight_id)

    async def run(self) -> None:
        configure_logging(self.config.logging.level)
        log.info("memory_service_starting")
        self.open()

        await self.batcher.start()
        await self.ingest.start()
        await self.merger.start()

        _sd_notify(b"READY=1")
        log.info("memory_service_ready", db=self.config.memory.db_path)

        shutdown = asyncio.Event()
        loop = asyncio.get_event_loop()
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, shutdown.set)

        await shutdown.wait()

        log.info("memory_service_shutting_down")
        await self.ingest.stop()
        await self.merger.stop()
        await self.batcher.stop()

        if self.conn:
            self.conn.close()

        log.info("memory_service_stopped")

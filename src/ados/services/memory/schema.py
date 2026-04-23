"""World Model database schema and migrations.

SQLite + sqlite-vec. Tables:
  flights           — mission run metadata
  frames            — captured keyframes with thumbnails
  observations      — object detections post capture-rules
  entities          — merged observations (canonical per object)
  places            — operator-saved named locations
  observation_tags  — many-to-many tags on observations
  snapshots         — point-in-time database exports

Virtual tables (sqlite-vec):
  frame_embeddings       — 768-dim CLIP float32 per frame
  observation_embeddings — 768-dim CLIP float32 per observation crop
  entity_canonicals      — canonical embedding per entity (running average)

The schema version lives in PRAGMA user_version.
Migrations are applied in order; each is idempotent.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path
from typing import Any

import structlog

log = structlog.get_logger()

CURRENT_VERSION = 1
EMBEDDING_DIM = 768


def _load_sqlite_vec(conn: sqlite3.Connection) -> None:
    """Load the sqlite-vec extension into the connection."""
    try:
        import sqlite_vec
        conn.enable_load_extension(True)
        sqlite_vec.load(conn)
        conn.enable_load_extension(False)
    except Exception as e:
        log.warning("sqlite_vec_load_failed", error=str(e), note="vector search will be disabled")


def _create_tables(conn: sqlite3.Connection) -> None:
    """Create all core tables (idempotent)."""
    conn.executescript(f"""
        CREATE TABLE IF NOT EXISTS flights (
            id TEXT PRIMARY KEY,
            start_ts REAL NOT NULL,
            end_ts REAL,
            operator TEXT NOT NULL DEFAULT '',
            vehicle_type TEXT NOT NULL DEFAULT 'copter',
            max_alt_m REAL NOT NULL DEFAULT 0,
            distance_m REAL NOT NULL DEFAULT 0,
            waypoint_count INTEGER NOT NULL DEFAULT 0,
            observation_count INTEGER NOT NULL DEFAULT 0,
            frame_count INTEGER NOT NULL DEFAULT 0,
            entity_count INTEGER NOT NULL DEFAULT 0,
            synced_at REAL
        );

        CREATE TABLE IF NOT EXISTS frames (
            id TEXT PRIMARY KEY,
            flight_id TEXT NOT NULL REFERENCES flights(id) ON DELETE CASCADE,
            ts REAL NOT NULL,
            pose_lat REAL,
            pose_lon REAL,
            pose_alt_m REAL,
            pose_heading REAL,
            camera_id TEXT NOT NULL DEFAULT 'main',
            thumb_path TEXT,
            fullres_path TEXT,
            caption TEXT,
            caption_source TEXT,
            caption_model TEXT,
            captured_by TEXT NOT NULL DEFAULT 'ingest'
        );

        CREATE TABLE IF NOT EXISTS observations (
            id TEXT PRIMARY KEY,
            flight_id TEXT NOT NULL REFERENCES flights(id) ON DELETE CASCADE,
            frame_id TEXT REFERENCES frames(id) ON DELETE SET NULL,
            entity_id TEXT REFERENCES entities(id) ON DELETE SET NULL,
            ts REAL NOT NULL,
            detect_class TEXT NOT NULL,
            confidence REAL NOT NULL DEFAULT 0,
            bbox_px TEXT,
            bbox_world TEXT,
            pose_lat REAL,
            pose_lon REAL,
            pose_alt_m REAL,
            target_lat REAL,
            target_lon REAL,
            target_alt_m REAL,
            source TEXT NOT NULL DEFAULT 'vision',
            model TEXT NOT NULL DEFAULT 'unknown'
        );

        CREATE TABLE IF NOT EXISTS entities (
            id TEXT PRIMARY KEY,
            first_seen_ts REAL NOT NULL,
            last_seen_ts REAL NOT NULL,
            last_lat REAL,
            last_lon REAL,
            detect_class TEXT NOT NULL,
            observation_count INTEGER NOT NULL DEFAULT 0,
            flight_ids TEXT NOT NULL DEFAULT '',
            name TEXT,
            tags TEXT NOT NULL DEFAULT '',
            merged_from TEXT NOT NULL DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS places (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            lat REAL NOT NULL,
            lon REAL NOT NULL,
            alt_m REAL,
            radius_m REAL NOT NULL DEFAULT 50.0,
            tags TEXT NOT NULL DEFAULT '',
            created_by TEXT NOT NULL DEFAULT 'operator',
            created_at REAL NOT NULL,
            last_visited_ts REAL
        );

        CREATE TABLE IF NOT EXISTS observation_tags (
            observation_id TEXT NOT NULL REFERENCES observations(id) ON DELETE CASCADE,
            tag TEXT NOT NULL,
            source TEXT NOT NULL DEFAULT 'rule',
            applied_at REAL NOT NULL,
            PRIMARY KEY (observation_id, tag)
        );

        CREATE TABLE IF NOT EXISTS snapshots (
            id TEXT PRIMARY KEY,
            flight_id TEXT REFERENCES flights(id) ON DELETE SET NULL,
            ts REAL NOT NULL,
            kind TEXT NOT NULL DEFAULT 'manual',
            payload_path TEXT,
            payload_bytes INTEGER NOT NULL DEFAULT 0,
            notes TEXT NOT NULL DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS entity_relationships (
            id TEXT PRIMARY KEY,
            source_id TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
            target_id TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
            rel_type TEXT NOT NULL,
            confidence REAL NOT NULL DEFAULT 0,
            applied_at REAL NOT NULL,
            source TEXT NOT NULL DEFAULT 'auto'
        );

        CREATE INDEX IF NOT EXISTS idx_frames_flight ON frames(flight_id, ts);
        CREATE INDEX IF NOT EXISTS idx_obs_flight ON observations(flight_id, ts);
        CREATE INDEX IF NOT EXISTS idx_obs_class_ts ON observations(detect_class, ts);
        CREATE INDEX IF NOT EXISTS idx_obs_entity ON observations(entity_id);
        CREATE INDEX IF NOT EXISTS idx_obs_pose ON observations(pose_lat, pose_lon);
        CREATE INDEX IF NOT EXISTS idx_frames_pose ON frames(pose_lat, pose_lon);
    """)


def _create_vector_tables(conn: sqlite3.Connection) -> None:
    """Create sqlite-vec virtual tables (idempotent, silently skip on error)."""
    dim = EMBEDDING_DIM
    try:
        conn.executescript(f"""
            CREATE VIRTUAL TABLE IF NOT EXISTS frame_embeddings
            USING vec0(embedding float[{dim}]);

            CREATE VIRTUAL TABLE IF NOT EXISTS observation_embeddings
            USING vec0(embedding float[{dim}]);

            CREATE VIRTUAL TABLE IF NOT EXISTS entity_canonicals
            USING vec0(embedding float[{dim}]);
        """)
    except Exception as e:
        log.warning("vec_table_create_failed", error=str(e))


def open_db(db_path: str | Path, *, create: bool = True) -> sqlite3.Connection:
    """Open (and optionally create) the World Model database.

    Returns a connection with sqlite-vec loaded, WAL mode enabled,
    foreign keys enabled, and all tables created/migrated.
    """
    path = Path(db_path)
    if create:
        path.parent.mkdir(parents=True, exist_ok=True)

    conn = sqlite3.connect(str(path), check_same_thread=False)
    conn.row_factory = sqlite3.Row

    # Performance settings
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    conn.execute("PRAGMA cache_size=-64000")  # 64 MB cache
    conn.execute("PRAGMA foreign_keys=ON")
    conn.execute("PRAGMA temp_store=MEMORY")

    _load_sqlite_vec(conn)
    _migrate(conn)
    return conn


def _migrate(conn: sqlite3.Connection) -> None:
    """Apply schema migrations."""
    version = conn.execute("PRAGMA user_version").fetchone()[0]
    if version >= CURRENT_VERSION:
        return

    log.info("world_model_migrating", from_version=version, to_version=CURRENT_VERSION)
    with conn:
        if version < 1:
            _create_tables(conn)
            _create_vector_tables(conn)
            conn.execute(f"PRAGMA user_version = {CURRENT_VERSION}")

    log.info("world_model_migrated", version=CURRENT_VERSION)


def insert_frame_embedding(conn: sqlite3.Connection, rowid: int, embedding: list[float]) -> None:
    """Insert or replace a frame embedding in the vector table."""
    try:
        import struct
        blob = struct.pack(f"{len(embedding)}f", *embedding)
        conn.execute(
            "INSERT OR REPLACE INTO frame_embeddings(rowid, embedding) VALUES (?, ?)",
            (rowid, blob),
        )
    except Exception as e:
        log.warning("frame_embedding_insert_failed", rowid=rowid, error=str(e))


def insert_observation_embedding(conn: sqlite3.Connection, rowid: int, embedding: list[float]) -> None:
    """Insert or replace an observation embedding."""
    try:
        import struct
        blob = struct.pack(f"{len(embedding)}f", *embedding)
        conn.execute(
            "INSERT OR REPLACE INTO observation_embeddings(rowid, embedding) VALUES (?, ?)",
            (rowid, blob),
        )
    except Exception as e:
        log.warning("obs_embedding_insert_failed", rowid=rowid, error=str(e))


def vector_search(
    conn: sqlite3.Connection,
    table: str,
    query_embedding: list[float],
    k: int = 10,
) -> list[int]:
    """Return up to k row IDs nearest to the query embedding."""
    try:
        import struct
        blob = struct.pack(f"{len(query_embedding)}f", *query_embedding)
        rows = conn.execute(
            f"SELECT rowid, distance FROM {table} WHERE embedding MATCH ? ORDER BY distance LIMIT ?",
            (blob, k),
        ).fetchall()
        return [r[0] for r in rows]
    except Exception as e:
        log.warning("vector_search_failed", table=table, error=str(e))
        return []

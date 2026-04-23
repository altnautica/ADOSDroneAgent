"""Query helpers for the World Model REST API.

Filter + paginate + vector search over the SQLite database.
All functions take a sqlite3.Connection and return dicts/lists
suitable for JSON serialization.
"""

from __future__ import annotations

import json
import sqlite3
from typing import Any

import structlog

from .schema import vector_search

log = structlog.get_logger()


def _row_to_dict(row: sqlite3.Row) -> dict:
    return dict(row)


# ── Flights ────────────────────────────────────────────────────────────────


def list_flights(conn: sqlite3.Connection, limit: int = 50, before_ts: float | None = None) -> list[dict]:
    q = "SELECT * FROM flights"
    params: list[Any] = []
    if before_ts is not None:
        q += " WHERE start_ts < ?"
        params.append(before_ts)
    q += " ORDER BY start_ts DESC LIMIT ?"
    params.append(limit)
    return [_row_to_dict(r) for r in conn.execute(q, params).fetchall()]


def get_flight(conn: sqlite3.Connection, flight_id: str) -> dict | None:
    row = conn.execute("SELECT * FROM flights WHERE id = ?", (flight_id,)).fetchone()
    return _row_to_dict(row) if row else None


def upsert_flight(conn: sqlite3.Connection, flight_id: str, start_ts: float, **kwargs: Any) -> None:
    fields = {"id": flight_id, "start_ts": start_ts, **kwargs}
    cols = ", ".join(fields.keys())
    placeholders = ", ".join("?" * len(fields))
    conn.execute(
        f"INSERT OR REPLACE INTO flights ({cols}) VALUES ({placeholders})",
        list(fields.values()),
    )


def update_flight_counts(conn: sqlite3.Connection, flight_id: str) -> None:
    conn.execute(
        """UPDATE flights SET
           frame_count = (SELECT count(*) FROM frames WHERE flight_id = ?),
           observation_count = (SELECT count(*) FROM observations WHERE flight_id = ?),
           entity_count = (SELECT count(DISTINCT entity_id) FROM observations WHERE flight_id = ? AND entity_id IS NOT NULL)
           WHERE id = ?""",
        (flight_id, flight_id, flight_id, flight_id),
    )


# ── Frames ─────────────────────────────────────────────────────────────────


def list_frames(
    conn: sqlite3.Connection,
    flight_id: str | None = None,
    limit: int = 100,
    offset: int = 0,
) -> list[dict]:
    q = "SELECT * FROM frames"
    params: list[Any] = []
    if flight_id:
        q += " WHERE flight_id = ?"
        params.append(flight_id)
    q += " ORDER BY ts DESC LIMIT ? OFFSET ?"
    params += [limit, offset]
    return [_row_to_dict(r) for r in conn.execute(q, params).fetchall()]


def get_frame(conn: sqlite3.Connection, frame_id: str) -> dict | None:
    row = conn.execute("SELECT * FROM frames WHERE id = ?", (frame_id,)).fetchone()
    return _row_to_dict(row) if row else None


# ── Observations ───────────────────────────────────────────────────────────


def list_observations(
    conn: sqlite3.Connection,
    flight_id: str | None = None,
    detect_class: str | None = None,
    entity_id: str | None = None,
    after_ts: float | None = None,
    limit: int = 100,
    offset: int = 0,
) -> list[dict]:
    q = "SELECT * FROM observations WHERE 1=1"
    params: list[Any] = []
    if flight_id:
        q += " AND flight_id = ?"
        params.append(flight_id)
    if detect_class:
        q += " AND detect_class = ?"
        params.append(detect_class)
    if entity_id:
        q += " AND entity_id = ?"
        params.append(entity_id)
    if after_ts is not None:
        q += " AND ts > ?"
        params.append(after_ts)
    q += " ORDER BY ts DESC LIMIT ? OFFSET ?"
    params += [limit, offset]
    return [_row_to_dict(r) for r in conn.execute(q, params).fetchall()]


def get_observation(conn: sqlite3.Connection, obs_id: str) -> dict | None:
    row = conn.execute("SELECT * FROM observations WHERE id = ?", (obs_id,)).fetchone()
    return _row_to_dict(row) if row else None


def add_observation_tag(conn: sqlite3.Connection, obs_id: str, tag: str, source: str = "operator") -> None:
    import time
    conn.execute(
        "INSERT OR IGNORE INTO observation_tags (observation_id, tag, source, applied_at) VALUES (?,?,?,?)",
        (obs_id, tag, source, time.time()),
    )


# ── Entities ───────────────────────────────────────────────────────────────


def list_entities(
    conn: sqlite3.Connection,
    detect_class: str | None = None,
    limit: int = 50,
    offset: int = 0,
) -> list[dict]:
    q = "SELECT * FROM entities WHERE 1=1"
    params: list[Any] = []
    if detect_class:
        q += " AND detect_class = ?"
        params.append(detect_class)
    q += " ORDER BY last_seen_ts DESC LIMIT ? OFFSET ?"
    params += [limit, offset]
    return [_row_to_dict(r) for r in conn.execute(q, params).fetchall()]


def get_entity(conn: sqlite3.Connection, entity_id: str) -> dict | None:
    row = conn.execute("SELECT * FROM entities WHERE id = ?", (entity_id,)).fetchone()
    return _row_to_dict(row) if row else None


def merge_entities(conn: sqlite3.Connection, entity_id: str, merge_into: str) -> bool:
    """Reassign all observations from entity_id → merge_into and delete entity_id."""
    if entity_id == merge_into:
        return False
    with conn:
        conn.execute(
            "UPDATE observations SET entity_id = ? WHERE entity_id = ?",
            (merge_into, entity_id),
        )
        conn.execute("DELETE FROM entities WHERE id = ?", (entity_id,))
        count = conn.execute(
            "SELECT count(*) FROM observations WHERE entity_id = ?", (merge_into,)
        ).fetchone()[0]
        conn.execute(
            "UPDATE entities SET observation_count = ? WHERE id = ?",
            (count, merge_into),
        )
    return True


def rename_entity(conn: sqlite3.Connection, entity_id: str, name: str) -> bool:
    with conn:
        conn.execute("UPDATE entities SET name = ? WHERE id = ?", (name, entity_id))
    return True


# ── Places ─────────────────────────────────────────────────────────────────


def list_places(conn: sqlite3.Connection) -> list[dict]:
    return [_row_to_dict(r) for r in conn.execute("SELECT * FROM places ORDER BY name").fetchall()]


def get_place(conn: sqlite3.Connection, place_id: str) -> dict | None:
    row = conn.execute("SELECT * FROM places WHERE id = ?", (place_id,)).fetchone()
    return _row_to_dict(row) if row else None


def create_place(
    conn: sqlite3.Connection,
    name: str,
    lat: float,
    lon: float,
    alt_m: float | None = None,
    radius_m: float = 50.0,
    tags: list[str] | None = None,
) -> dict:
    import secrets, time as _time
    place_id = secrets.token_hex(8)
    tags_str = ",".join(tags or [])
    with conn:
        conn.execute(
            """INSERT INTO places (id, name, lat, lon, alt_m, radius_m, tags, created_at)
               VALUES (?,?,?,?,?,?,?,?)""",
            (place_id, name, lat, lon, alt_m, radius_m, tags_str, _time.time()),
        )
    return {"id": place_id, "name": name, "lat": lat, "lon": lon, "radius_m": radius_m}


# ── Search ─────────────────────────────────────────────────────────────────


def search_by_embedding(
    conn: sqlite3.Connection,
    query_embedding: list[float],
    k: int = 10,
    table: str = "observation_embeddings",
) -> list[dict]:
    """Return top-k observations by embedding similarity."""
    rowids = vector_search(conn, table, query_embedding, k)
    if not rowids:
        return []
    placeholders = ", ".join("?" * len(rowids))
    rows = conn.execute(
        f"SELECT * FROM observations WHERE rowid IN ({placeholders})",
        rowids,
    ).fetchall()
    return [_row_to_dict(r) for r in rows]


# ── Diff ───────────────────────────────────────────────────────────────────


def flight_diff(conn: sqlite3.Connection, flight_id_a: str, flight_id_b: str) -> dict:
    """Return entities present in B but not A, and entities present in A but not B."""
    ids_a = set(
        r[0] for r in conn.execute(
            "SELECT DISTINCT entity_id FROM observations WHERE flight_id = ? AND entity_id IS NOT NULL",
            (flight_id_a,),
        ).fetchall()
    )
    ids_b = set(
        r[0] for r in conn.execute(
            "SELECT DISTINCT entity_id FROM observations WHERE flight_id = ? AND entity_id IS NOT NULL",
            (flight_id_b,),
        ).fetchall()
    )
    new_in_b = list(ids_b - ids_a)
    gone_in_b = list(ids_a - ids_b)

    def get_entities(ids: list[str]) -> list[dict]:
        if not ids:
            return []
        ph = ", ".join("?" * len(ids))
        return [_row_to_dict(r) for r in conn.execute(f"SELECT * FROM entities WHERE id IN ({ph})", ids).fetchall()]

    return {
        "new": get_entities(new_in_b),
        "gone": get_entities(gone_in_b),
        "flight_a": flight_id_a,
        "flight_b": flight_id_b,
    }

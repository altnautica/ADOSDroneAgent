"""World Model REST API routes.

All endpoints are under /api/memory/*.
The ados-memory service must be running for most endpoints to work.
If the database is not available, endpoints return 503.
"""

from __future__ import annotations

import json
import time
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException, Query
from pydantic import BaseModel

import structlog

log = structlog.get_logger()
router = APIRouter(prefix="/memory", tags=["memory"])


def _get_conn():
    """Get a read-only connection to the World Model database."""
    from ados.core.config import load_config
    cfg = load_config()
    db_path = Path(cfg.memory.db_path)
    if not db_path.exists():
        raise HTTPException(status_code=503, detail="World Model database not available")
    from ados.services.memory.schema import open_db
    return open_db(db_path, create=False)


# ── Flights ────────────────────────────────────────────────────────────────

@router.get("/flights")
async def list_flights(limit: int = Query(50, ge=1, le=200)):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import list_flights as _list
        return _list(conn, limit=limit)
    finally:
        conn.close()


@router.get("/flights/{flight_id}")
async def get_flight(flight_id: str):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import get_flight as _get
        result = _get(conn, flight_id)
        if not result:
            raise HTTPException(status_code=404, detail="Flight not found")
        return result
    finally:
        conn.close()


# ── Frames ─────────────────────────────────────────────────────────────────

@router.get("/frames")
async def list_frames(
    flight_id: str | None = None,
    limit: int = Query(100, ge=1, le=500),
    offset: int = Query(0, ge=0),
):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import list_frames as _list
        return _list(conn, flight_id=flight_id, limit=limit, offset=offset)
    finally:
        conn.close()


@router.get("/frames/{frame_id}")
async def get_frame(frame_id: str):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import get_frame as _get
        result = _get(conn, frame_id)
        if not result:
            raise HTTPException(status_code=404, detail="Frame not found")
        return result
    finally:
        conn.close()


# ── Observations ───────────────────────────────────────────────────────────

@router.get("/observations")
async def list_observations(
    flight_id: str | None = None,
    detect_class: str | None = None,
    entity_id: str | None = None,
    after_ts: float | None = None,
    limit: int = Query(100, ge=1, le=500),
    offset: int = Query(0, ge=0),
):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import list_observations as _list
        return _list(
            conn,
            flight_id=flight_id,
            detect_class=detect_class,
            entity_id=entity_id,
            after_ts=after_ts,
            limit=limit,
            offset=offset,
        )
    finally:
        conn.close()


@router.get("/observations/{obs_id}")
async def get_observation(obs_id: str):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import get_observation as _get
        result = _get(conn, obs_id)
        if not result:
            raise HTTPException(status_code=404, detail="Observation not found")
        return result
    finally:
        conn.close()


class TagBody(BaseModel):
    tag: str
    source: str = "operator"


@router.post("/observations/{obs_id}/tags")
async def add_tag(obs_id: str, body: TagBody):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import add_observation_tag
        with conn:
            add_observation_tag(conn, obs_id, body.tag, body.source)
        return {"ok": True}
    finally:
        conn.close()


# ── Entities ───────────────────────────────────────────────────────────────

@router.get("/entities")
async def list_entities(
    detect_class: str | None = None,
    limit: int = Query(50, ge=1, le=200),
    offset: int = Query(0, ge=0),
):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import list_entities as _list
        return _list(conn, detect_class=detect_class, limit=limit, offset=offset)
    finally:
        conn.close()


@router.get("/entities/{entity_id}")
async def get_entity(entity_id: str):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import get_entity as _get
        result = _get(conn, entity_id)
        if not result:
            raise HTTPException(status_code=404, detail="Entity not found")
        return result
    finally:
        conn.close()


class MergeBody(BaseModel):
    merge_into: str


@router.post("/entities/{entity_id}/merge")
async def merge_entity(entity_id: str, body: MergeBody):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import merge_entities
        with conn:
            ok = merge_entities(conn, entity_id, body.merge_into)
        if not ok:
            raise HTTPException(status_code=400, detail="Cannot merge entity into itself")
        return {"ok": True}
    finally:
        conn.close()


class RenameBody(BaseModel):
    name: str


@router.post("/entities/{entity_id}/rename")
async def rename_entity(entity_id: str, body: RenameBody):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import rename_entity as _rename
        with conn:
            _rename(conn, entity_id, body.name)
        return {"ok": True}
    finally:
        conn.close()


# ── Places ─────────────────────────────────────────────────────────────────

@router.get("/places")
async def list_places():
    conn = _get_conn()
    try:
        from ados.services.memory.queries import list_places as _list
        return _list(conn)
    finally:
        conn.close()


@router.get("/places/{place_id}")
async def get_place(place_id: str):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import get_place as _get
        result = _get(conn, place_id)
        if not result:
            raise HTTPException(status_code=404, detail="Place not found")
        return result
    finally:
        conn.close()


class PlaceBody(BaseModel):
    name: str
    lat: float
    lon: float
    alt_m: float | None = None
    radius_m: float = 50.0
    tags: list[str] = []


@router.post("/places")
async def create_place(body: PlaceBody):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import create_place as _create
        with conn:
            result = _create(
                conn,
                name=body.name,
                lat=body.lat,
                lon=body.lon,
                alt_m=body.alt_m,
                radius_m=body.radius_m,
                tags=body.tags,
            )
        return result
    finally:
        conn.close()


# ── Search ─────────────────────────────────────────────────────────────────

class TextSearchBody(BaseModel):
    query: str
    k: int = 10


@router.post("/search/by-text")
async def search_by_text(body: TextSearchBody):
    """Vector search observations by natural-language query.

    Calls the Vision Engine to get a CLIP text embedding, then
    searches the observation_embeddings vector table.
    """
    conn = _get_conn()
    try:
        # Get text embedding from Vision Engine
        embedding: list[float] | None = None
        try:
            import httpx
            resp = await httpx.AsyncClient(timeout=2.0).post(
                "http://127.0.0.1:8080/api/vision/embed-text",
                json={"text": body.query},
            )
            if resp.status_code == 200:
                embedding = resp.json().get("embedding")
        except Exception:
            pass

        if embedding:
            from ados.services.memory.queries import search_by_embedding
            return search_by_embedding(conn, embedding, k=body.k)

        # Fallback: text search on detect_class and caption
        rows = conn.execute(
            """SELECT * FROM observations
               WHERE detect_class LIKE ? OR rowid IN (
                   SELECT rowid FROM observations WHERE caption LIKE ? LIMIT ?
               )
               LIMIT ?""",
            (f"%{body.query}%", f"%{body.query}%", body.k, body.k),
        ).fetchall()
        return [dict(r) for r in rows]
    finally:
        conn.close()


# ── Diff ───────────────────────────────────────────────────────────────────

class DiffBody(BaseModel):
    flight_id_a: str
    flight_id_b: str


@router.post("/diff")
async def flight_diff(body: DiffBody):
    conn = _get_conn()
    try:
        from ados.services.memory.queries import flight_diff as _diff
        return _diff(conn, body.flight_id_a, body.flight_id_b)
    finally:
        conn.close()


# ── Health ─────────────────────────────────────────────────────────────────

@router.get("/health")
async def health():
    try:
        conn = _get_conn()
        count = conn.execute("SELECT count(*) FROM flights").fetchone()[0]
        conn.close()
        return {"status": "healthy", "flight_count": count}
    except HTTPException:
        return {"status": "unavailable"}

@router.get("/metrics")
async def metrics():
    try:
        conn = _get_conn()
        obs_count = conn.execute("SELECT count(*) FROM observations").fetchone()[0]
        entity_count = conn.execute("SELECT count(*) FROM entities").fetchone()[0]
        flight_count = conn.execute("SELECT count(*) FROM flights").fetchone()[0]
        conn.close()
        return {
            "observations": obs_count,
            "entities": entity_count,
            "flights": flight_count,
        }
    except Exception:
        return {"error": "unavailable"}

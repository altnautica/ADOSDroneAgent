"""Fleet/MeshNet stub routes."""

from __future__ import annotations

from fastapi import APIRouter

router = APIRouter()


@router.get("/fleet/enrollment")
async def get_enrollment():
    """Get MeshNet enrollment status."""
    return {"enrolled": False}


@router.get("/fleet/peers")
async def list_peers():
    """List fleet peers."""
    return []

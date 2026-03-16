"""Peripheral management stub routes."""

from __future__ import annotations

from fastapi import APIRouter

router = APIRouter()


@router.get("/peripherals")
async def list_peripherals():
    """List detected peripherals."""
    return []


@router.post("/peripherals/scan")
async def scan_peripherals():
    """Scan for connected peripherals."""
    return []

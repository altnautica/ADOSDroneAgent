"""Vision Engine REST routes.

v1.0 provides stubs for embedding endpoints so the World Model ingest
pipeline gets a clean 501 response instead of a 404 and can skip
gracefully. Real CLIP / VLM integration is a future update.
"""

from __future__ import annotations

from fastapi import APIRouter
from pydantic import BaseModel

router = APIRouter()


class EmbedCropBody(BaseModel):
    crop_base64: str


@router.post("/vision/embed")
async def embed_crop(body: EmbedCropBody):
    """Return an empty embedding with a note that Vision Engine is not yet wired.

    World Model ingest calls this during Stage 3 (Embedding). Returning
    an empty embedding lets the pipeline continue with the observation
    recorded but no semantic search vector. Real CLIP integration with
    the Vision Engine ships in a future update.
    """
    return {
        "embedding": [],
        "available": False,
        "reason": "Vision Engine CLIP endpoint not implemented in v1.0",
    }


class EmbedTextBody(BaseModel):
    text: str


@router.post("/vision/embed-text")
async def embed_text(body: EmbedTextBody):
    """Same treatment for text embedding."""
    return {
        "embedding": [],
        "available": False,
        "reason": "Vision Engine CLIP text embedding not implemented in v1.0",
    }


@router.get("/vision/embed/status")
async def embed_status():
    """GCS reads this to display 'semantic search disabled' message."""
    return {
        "available": False,
        "reason": "Vision Engine not yet wired. World Model falls back to text search.",
    }

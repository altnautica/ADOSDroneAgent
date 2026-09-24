"""Small constants shared by the residual ground-station routes."""

from __future__ import annotations


def _stock_confirm_token() -> str:
    """Confirmation token used when nothing is currently paired."""
    return "factory-reset-unpaired"


__all__ = ["_stock_confirm_token"]

"""Assist rules library.

Each rule module exports one or more Rule classes.
Rules are loaded at service start and run against the current
context window to generate Suggestion objects.
"""

from __future__ import annotations

from .base import Rule, Suggestion, load_all_rules

__all__ = ["Rule", "Suggestion", "load_all_rules"]

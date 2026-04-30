"""Built-in plugins shipped with the agent.

Each subdirectory is a self-contained plugin discovered via the
``ados.plugins`` entry-point group declared in ``pyproject.toml``.
Built-ins run inprocess (first-party signer carve-out) so they
incur no IPC cost; their code is the same shape third-party plugins
use, which keeps the SDK contract honest.
"""

from __future__ import annotations

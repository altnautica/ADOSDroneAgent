"""Static-asset package for the agent's browser dashboard.

The TypeScript source lives at ``ADOSDroneAgent/dashboard/`` (sibling of
``src/``). CI builds the SPA with ``npm run build`` and copies the
output into ``src/ados/dashboard/dist/`` immediately before
``uv build --wheel`` runs, so setuptools picks the built artifacts up
via the ``ados.dashboard`` package-data glob in ``pyproject.toml``.

The FastAPI server resolves the dist directory through
``importlib.resources`` so editable installs and wheel installs find
the same files.

A placeholder ``dist/index.html`` ships in the source tree so a fresh
checkout (without a CI build) still serves something rather than 404.
``scripts/build-dashboard.sh`` overwrites it on real builds.
"""

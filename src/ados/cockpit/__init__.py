"""Static-asset package for the agent-served ground-station cockpit.

The TypeScript source lives at ``ADOSDroneAgent/cockpit/`` (sibling of
``src/``). ``scripts/build-cockpit.sh`` builds the SPA with ``npm run
build`` and copies the output into ``src/ados/cockpit/static/`` before
``uv build --wheel`` runs, so setuptools picks the built artifacts up via
the ``ados.cockpit`` package-data glob in ``pyproject.toml``.

This is a separate bundle from the laptop dashboard (``ados.dashboard``):
a distinct route (``/cockpit``) and distinct built output for the on-box
HDMI panel. The staging directory is named ``static/`` rather than
``dist/`` so the top-level ``.gitignore`` rule that hides Vite / wheel
build output does not also hide the in-package files we ship.

The FastAPI server resolves the static directory through
``importlib.resources`` so editable installs and wheel installs find the
same files. A placeholder ``static/index.html`` ships in the source tree
so a fresh checkout (without a build) still serves something rather than
404; ``scripts/build-cockpit.sh`` overwrites it on real builds. The
served bundle is a committed artifact: any change to ``cockpit/`` must
rebuild and commit ``static/`` in the same push.
"""

"""Dashboard packaging contract.

The browser dashboard ships as a Vite-built SPA staged into
``src/ados/dashboard/dist/``. ``scripts/build-dashboard.sh`` copies the
``dashboard/dist/`` Vite output into the staging path before
``uv build --wheel`` runs, so the wheel always carries a built dist.
A placeholder ``dist/index.html`` ships in the source tree so a fresh
checkout (without a CI build) still serves something rather than 404.

This test enforces the contract that the staged dist exists and is
non-empty whenever the wheel is being built or tests run.
"""

from __future__ import annotations

from pathlib import Path

DASHBOARD_PKG = Path(__file__).resolve().parent.parent / "src" / "ados" / "dashboard"


def test_dashboard_package_marker_exists() -> None:
    """src/ados/dashboard/__init__.py must exist so setuptools registers
    the package and picks up the package-data glob.
    """
    init = DASHBOARD_PKG / "__init__.py"
    assert init.is_file(), f"missing dashboard package marker at {init}"


def test_dashboard_dist_directory_exists() -> None:
    """src/ados/dashboard/dist/ must exist with at least an index.html so
    the FastAPI StaticFiles mount can serve the dashboard root. CI builds
    overwrite this with the real Vite output; the placeholder ships in
    the source tree so editable installs are never broken.
    """
    dist = DASHBOARD_PKG / "dist"
    assert dist.is_dir(), f"missing dashboard dist directory at {dist}"
    index = dist / "index.html"
    assert index.is_file(), f"missing dashboard index.html at {index}"
    assert index.stat().st_size > 0, "dashboard index.html is empty"


def test_dashboard_resolves_via_importlib_resources() -> None:
    """The FastAPI mount uses importlib.resources.files() to locate the
    dist directory. This test mirrors that resolution path so a packaging
    regression fails here instead of at runtime on the agent.
    """
    from importlib.resources import files

    import ados.dashboard as pkg

    dist = Path(str(files(pkg))) / "dist"
    assert dist.is_dir(), f"importlib.resources can't reach dist at {dist}"
    assert (dist / "index.html").is_file()

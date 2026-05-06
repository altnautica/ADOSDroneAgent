"""Universal webapp packaging contract.

The agent's universal setup webapp is a History-API SPA with a single
``index.html`` shell, a single CSS file, a small bootstrap ``app.js``,
and per-component / per-view ES modules under ``components/`` and
``views/``. Anything that ships in the wheel must show up here so a
broken package layout fails the build, not the user.
"""

from __future__ import annotations

from pathlib import Path

import pytest

WEBAPP_DIR = Path(__file__).resolve().parent.parent / "web" / "setup"

REQUIRED_FILES = (
    "index.html",
    "app.js",
    "router.js",
    "state.js",
    "components.js",
    "dashboard.css",
)

REQUIRED_COMPONENTS = (
    "header.js",
    "bottom-dock.js",
    "command-palette.js",
    "context-menu.js",
    "gestures.js",
    "keyboard.js",
    "sheet.js",
    "theme.js",
    "toast.js",
)

REQUIRED_VIEWS = (
    "dashboard.js",
    "logs.js",
)

REQUIRED_SETTINGS_VIEWS = (
    "index.js",
    "profile.js",
    "cloud.js",
    "network.js",
    "display.js",
    "advanced.js",
)


def test_universal_webapp_dir_exists() -> None:
    assert WEBAPP_DIR.is_dir(), f"missing webapp directory: {WEBAPP_DIR}"


@pytest.mark.parametrize("path", REQUIRED_FILES)
def test_each_top_level_asset_present(path: str) -> None:
    target = WEBAPP_DIR / path
    assert target.is_file(), f"missing webapp asset: {target}"
    assert target.stat().st_size > 0, f"empty webapp asset: {target}"


@pytest.mark.parametrize("name", REQUIRED_COMPONENTS)
def test_each_component_module_present(name: str) -> None:
    target = WEBAPP_DIR / "components" / name
    assert target.is_file(), f"missing component module: {target}"
    assert target.stat().st_size > 0, f"empty component module: {target}"


@pytest.mark.parametrize("name", REQUIRED_VIEWS)
def test_each_top_level_view_present(name: str) -> None:
    target = WEBAPP_DIR / "views" / name
    assert target.is_file(), f"missing view module: {target}"
    assert target.stat().st_size > 0, f"empty view module: {target}"


@pytest.mark.parametrize("name", REQUIRED_SETTINGS_VIEWS)
def test_each_settings_view_present(name: str) -> None:
    target = WEBAPP_DIR / "views" / "settings" / name
    assert target.is_file(), f"missing settings view: {target}"
    assert target.stat().st_size > 0, f"empty settings view: {target}"


def test_index_references_bootstrap_assets() -> None:
    text = (WEBAPP_DIR / "index.html").read_text(encoding="utf-8")
    assert "/app.js" in text, "index.html does not reference /app.js"
    assert "/dashboard.css" in text, "index.html does not reference /dashboard.css"


def test_app_js_starts_router_and_polls_status() -> None:
    text = (WEBAPP_DIR / "app.js").read_text(encoding="utf-8")
    assert "Router" in text, "app.js must instantiate the History-API router"
    assert "/api/v1/setup/status" in text, (
        "app.js must seed or poll the setup status endpoint"
    )
    for view in ("renderDashboard", "renderSettings", "renderLogs"):
        assert view in text, f"app.js missing import for {view}"


def test_legacy_pages_are_gone() -> None:
    """The 8 legacy wizard HTML pages were collapsed into the SPA shell.
    A stray copy means the old surface is still reachable in the bundle."""
    legacy = (
        "setup.html",
        "mavlink.html",
        "video.html",
        "network.html",
        "remote.html",
        "ground.html",
        "system.html",
        "advanced.html",
    )
    for name in legacy:
        target = WEBAPP_DIR / name
        assert not target.exists(), f"legacy page still in webapp: {target}"


def test_no_legacy_static_dirs() -> None:
    """An earlier layout kept the webapp at src/ados/webapp/. That tree
    is retired; everything ships from the top-level web/setup/ package."""
    repo_root = WEBAPP_DIR.parent.parent
    legacy_root = repo_root / "src" / "ados" / "webapp"
    assert not legacy_root.exists(), "legacy src/ados/webapp/ should be removed"
    parent = WEBAPP_DIR.parent
    assert not (parent / "static").exists(), "no legacy web/static/ should exist"
    assert not (parent / "static-ground").exists(), (
        "no legacy web/static-ground/ should exist"
    )


def test_no_legacy_stylesheet() -> None:
    """The wizard-era ``style.css`` was replaced by ``dashboard.css``."""
    target = WEBAPP_DIR / "style.css"
    assert not target.exists(), f"legacy stylesheet still present: {target}"

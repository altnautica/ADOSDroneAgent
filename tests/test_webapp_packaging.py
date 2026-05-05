"""Universal webapp packaging contract.

The agent's universal setup webapp ships nine HTML page shells plus a single
ES module (``app.js``) and a single stylesheet (``style.css``). The shells all
reference ``/app.js`` to render. If the JS module is missing the webapp loads
the noscript fallback only, so we guard the contract here.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

WEBAPP_DIR = Path(__file__).resolve().parent.parent / "web" / "setup"

REQUIRED_PAGES = (
    "index.html",
    "setup.html",
    "mavlink.html",
    "video.html",
    "network.html",
    "remote.html",
    "ground.html",
    "system.html",
    "advanced.html",
)

REQUIRED_ASSETS = (
    "app.js",
    "style.css",
)


def test_universal_webapp_dir_exists() -> None:
    assert WEBAPP_DIR.is_dir(), f"missing webapp directory: {WEBAPP_DIR}"


@pytest.mark.parametrize("page", REQUIRED_PAGES)
def test_each_page_shell_present(page: str) -> None:
    target = WEBAPP_DIR / page
    assert target.is_file(), f"missing page shell: {target}"


@pytest.mark.parametrize("page", REQUIRED_PAGES)
def test_each_page_references_app_js(page: str) -> None:
    text = (WEBAPP_DIR / page).read_text(encoding="utf-8")
    assert "/app.js" in text, f"{page} does not reference /app.js"
    assert "/style.css" in text, f"{page} does not reference /style.css"


@pytest.mark.parametrize("page", REQUIRED_PAGES)
def test_each_page_sets_data_page(page: str) -> None:
    text = (WEBAPP_DIR / page).read_text(encoding="utf-8")
    match = re.search(r'data-page="([a-z-]+)"', text)
    assert match, f"{page} does not declare body[data-page]"


@pytest.mark.parametrize("asset", REQUIRED_ASSETS)
def test_each_asset_present(asset: str) -> None:
    target = WEBAPP_DIR / asset
    assert target.is_file(), f"missing webapp asset: {target}"
    assert target.stat().st_size > 0, f"empty webapp asset: {target}"


def test_no_legacy_static_dirs() -> None:
    """The legacy webapp tree under src/ados/webapp/ is retired; assets
    now live at the top-level web/setup/ canonical location."""
    repo_root = WEBAPP_DIR.parent.parent
    legacy_root = repo_root / "src" / "ados" / "webapp"
    assert not legacy_root.exists(), "legacy src/ados/webapp/ should be removed"
    parent = WEBAPP_DIR.parent
    assert not (parent / "static").exists(), "no legacy web/static/ should exist"
    assert not (parent / "static-ground").exists(), "no legacy web/static-ground/ should exist"


def test_app_js_dispatches_by_data_page() -> None:
    text = (WEBAPP_DIR / "app.js").read_text(encoding="utf-8")
    assert "document.body" in text and "dataset" in text, (
        "app.js must dispatch by document.body.dataset.page"
    )
    for page in (
        "dashboard",
        "setup",
        "mavlink",
        "video",
        "network",
        "remote",
        "ground",
        "system",
        "advanced",
    ):
        assert page in text, f"app.js missing renderer reference for page: {page}"

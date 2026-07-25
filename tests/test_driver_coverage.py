"""Keep the prebuilt-driver matrix and the board profiles in step.

`.github/driver-matrix.json` decides which kernels CI builds an RTL8812EU
module for; `.github/driver-coverage.json` records which board profile each
of those kernels serves, and which boards deliberately have none.

Nothing used to check that relationship — it lived in a prose comment in a
single board YAML. So a new board profile, a renamed flavor or a deleted
matrix row all passed CI silently, and the cost only showed up later as an
unexplained multi-minute driver build during a bring-up.

These tests make coverage a declared, checked fact. They read repo files
only: no network, no hardware, no kernel.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

_REPO = Path(__file__).resolve().parents[1]
_MATRIX_PATH = _REPO / ".github" / "driver-matrix.json"
_COVERAGE_PATH = _REPO / ".github" / "driver-coverage.json"
_BOARDS_DIR = _REPO / "src" / "ados" / "hal" / "boards"

# Group A installs headers from an apt repo, group B from a pinned .deb.
_HEADERS_SOURCES = {"apt", "deb"}


def _matrix() -> list[dict]:
    return json.loads(_MATRIX_PATH.read_text())


def _coverage() -> dict:
    return json.loads(_COVERAGE_PATH.read_text())


def _board_stems() -> set[str]:
    """Every shipped board profile, by file stem (the id used in coverage)."""
    return {p.stem for p in _BOARDS_DIR.glob("*.yaml")}


def _classified() -> list[str]:
    """Every board id named in coverage, WITH duplicates preserved.

    Returned as a list rather than a set so the double-classification check
    below can actually see a board listed twice.
    """
    cov = _coverage()
    named: list[str] = []
    for boards in cov["flavors"].values():
        named.extend(boards)
    named.extend(cov["builds_on_device"])
    return named


def test_every_board_profile_is_classified_exactly_once() -> None:
    """A new board profile must declare its driver story before it can merge.

    Either it names the matrix flavor that builds its kernel, or it is
    listed under `builds_on_device` — an explicit "no prebuilt, compiles
    locally", which is a valid answer for hardware we cannot test on.
    """
    stems = _board_stems()
    assert stems, f"no board profiles found under {_BOARDS_DIR}"

    named = _classified()

    missing = sorted(stems - set(named))
    assert not missing, (
        f"board profile(s) {missing} are not classified in {_COVERAGE_PATH.name}. "
        "Add each to the `flavors` entry for the matrix row that builds its "
        "kernel, or to `builds_on_device` if it has no prebuilt coverage."
    )

    unknown = sorted(set(named) - stems)
    assert not unknown, (
        f"{_COVERAGE_PATH.name} lists board(s) {unknown} with no matching "
        f"{_BOARDS_DIR.name}/<id>.yaml. Remove them or fix the id."
    )

    duplicated = sorted({b for b in named if named.count(b) > 1})
    assert not duplicated, (
        f"board(s) {duplicated} are classified more than once in "
        f"{_COVERAGE_PATH.name}. A board runs one kernel line, so it belongs "
        "to exactly one flavor (or to builds_on_device)."
    )


def test_every_declared_flavor_exists_in_the_matrix() -> None:
    """Renaming or deleting a matrix row must not silently orphan boards."""
    matrix_flavors = {row["flavor"] for row in _matrix()}
    declared = set(_coverage()["flavors"])

    orphaned = sorted(declared - matrix_flavors)
    assert not orphaned, (
        f"{_COVERAGE_PATH.name} maps boards to flavor(s) {orphaned} that do "
        f"not exist in {_MATRIX_PATH.name}. Those boards would get no prebuilt "
        "module. Restore the row, or move the boards to builds_on_device."
    )


def test_every_matrix_row_declares_which_boards_it_serves() -> None:
    """A new matrix row must say what it is for, so coverage stays readable."""
    matrix_flavors = {row["flavor"] for row in _matrix()}
    declared = set(_coverage()["flavors"])

    undeclared = sorted(matrix_flavors - declared)
    assert not undeclared, (
        f"{_MATRIX_PATH.name} builds flavor(s) {undeclared} that "
        f"{_COVERAGE_PATH.name} does not describe. Add a `flavors` entry "
        "listing the board profiles each one serves (an empty list is fine "
        "for a kernel line we build ahead of owning the hardware)."
    )


def test_matrix_flavors_are_unique() -> None:
    """Two rows sharing a flavor would collide on the row-<flavor>.json artifact."""
    flavors = [row["flavor"] for row in _matrix()]
    dupes = sorted({f for f in flavors if flavors.count(f) > 1})
    assert not dupes, f"duplicate flavor(s) in {_MATRIX_PATH.name}: {dupes}"


@pytest.mark.parametrize("row", _matrix(), ids=lambda r: r["flavor"])
def test_matrix_row_is_well_formed(row: dict) -> None:
    """Catch a malformed row here rather than in a fanned-out build job."""
    for key in ("flavor", "karch", "container", "headers_source", "serves", "kver_hint"):
        assert row.get(key), f"row {row.get('flavor')!r} has an empty `{key}`"

    source = row["headers_source"]
    assert source in _HEADERS_SOURCES, (
        f"row {row['flavor']!r} has headers_source={source!r}; "
        f"expected one of {sorted(_HEADERS_SOURCES)}"
    )

    if source == "apt":
        # Group A resolves headers from a vendor apt repo.
        assert row.get("apt_list"), (
            f"apt row {row['flavor']!r} needs an `apt_list` repo line"
        )
        assert row.get("headers_pkg"), (
            f"apt row {row['flavor']!r} needs a `headers_pkg` to install"
        )
    else:
        # Group B pins a captured .deb so CI reproduces a flashed kernel
        # whose headers the vendor repo no longer serves.
        assert row.get("headers_deb_url"), (
            f"deb row {row['flavor']!r} needs a `headers_deb_url` "
            "(a full URL, or '<release-tag>/<asset>' on this repo's releases)"
        )

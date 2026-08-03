"""The access-point passphrase is per unit, not one default on every box.

A single built-in string shipped on every unit is not a secret: anyone within
radio range of any ADOS ground station could join the network of any other.

Generating one instead is only safe because it is now displayed — on the
installer's completion card and in the on-box status view. Nothing showed it
before, so a generated value would have been undiscoverable and the unit
unjoinable. These tests pin the generation contract; the Rust side pins the
display.
"""

from __future__ import annotations

from ados.services.ground_station.hostapd_manager import generate_ap_passphrase


def test_a_generated_passphrase_is_legal_for_wpa2() -> None:
    # hostapd refuses the whole configuration on an illegal passphrase rather
    # than correcting it, which takes the access point down entirely.
    p = generate_ap_passphrase()
    assert 8 <= len(p) <= 63
    assert all(c.isprintable() and c.isascii() for c in p)


def test_the_shared_builtin_passphrase_no_longer_exists() -> None:
    # There used to be a single passphrase compiled into every unit, reached
    # both as the entropy-failure fallback and -- more damagingly -- as the
    # shipped config default, which took precedence over generation and so gave
    # every ground station the same key. Neither path may come back.
    import ados.services.ground_station.hostapd_manager as hm
    from ados.core.config import HotspotConfig

    assert not hasattr(hm, "BUILTIN_PASSPHRASE")
    assert generate_ap_passphrase() != "altnautica"
    assert HotspotConfig().password == "", (
        "a non-empty configured default silently disables per-unit generation"
    )


def test_successive_draws_differ() -> None:
    seen = {generate_ap_passphrase() for _ in range(50)}
    assert len(seen) == 50, "a repeated passphrase means the draw is not random"


def test_only_unambiguous_characters_are_used() -> None:
    # These are read off a small display and typed into a phone; a mistyped
    # passphrase is indistinguishable from a broken radio to whoever typed it.
    ambiguous = set("0O1IL")
    for _ in range(200):
        assert not (set(generate_ap_passphrase()) & ambiguous)


def test_every_charset_position_is_reachable() -> None:
    # A biased draw would leave part of the alphabet unreachable.
    from ados.services.ground_station.hostapd_manager import _UNAMBIGUOUS_CHARSET

    seen: set[str] = set()
    for _ in range(400):
        seen.update(generate_ap_passphrase())
    assert seen == set(_UNAMBIGUOUS_CHARSET)


def test_a_status_read_never_creates_a_passphrase(tmp_path, monkeypatch) -> None:
    """A GET must not mint a credential.

    The status route called `ensure_passphrase`, which generates when the file
    is absent — so merely reading status created and persisted a secret, and
    before the create was made exclusive it could be a different one from the
    value hostapd had loaded.
    """
    import ados.services.ground_station.hostapd_manager as hm

    path = tmp_path / "ap-passphrase"
    monkeypatch.setattr(hm, "_PASSPHRASE_PATH", path)

    assert hm.read_ap_passphrase() == ""
    assert not path.exists(), "a read must not create the file"


def test_a_read_returns_what_is_on_disk(tmp_path, monkeypatch) -> None:
    import ados.services.ground_station.hostapd_manager as hm

    path = tmp_path / "ap-passphrase"
    path.write_text("KM7QRT4XPN29\n", encoding="utf-8")
    monkeypatch.setattr(hm, "_PASSPHRASE_PATH", path)
    assert hm.read_ap_passphrase() == "KM7QRT4XPN29"


def test_the_python_and_rust_halves_agree_on_the_contract() -> None:
    """Both halves resolve this passphrase and must not disagree about it.

    The Rust access-point manager owns the native path and the Python one is
    its mirror; a divergence here means two processes on the same box render
    different credentials into the same config file.
    """
    from pathlib import Path

    rust = Path(__file__).resolve().parents[1] / "crates/ados-protocol/src/secret_gen.rs"
    body = rust.read_text(encoding="utf-8")
    assert 'b"ABCDEFGHJKMNPQRSTUVWXYZ23456789"' in body, "charset drifted"
    assert "AP_PASSPHRASE_LEN: usize = 12" in body, "length drifted"

    from ados.services.ground_station.hostapd_manager import (
        _AP_PASSPHRASE_LEN,
        _UNAMBIGUOUS_CHARSET,
    )

    assert _UNAMBIGUOUS_CHARSET == "ABCDEFGHJKMNPQRSTUVWXYZ23456789"
    assert _AP_PASSPHRASE_LEN == 12

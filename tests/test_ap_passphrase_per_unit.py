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

from ados.services.ground_station.hostapd_manager import (
    BUILTIN_PASSPHRASE,
    generate_ap_passphrase,
)


def test_a_generated_passphrase_is_legal_for_wpa2() -> None:
    # hostapd refuses the whole configuration on an illegal passphrase rather
    # than correcting it, which takes the access point down entirely.
    p = generate_ap_passphrase()
    assert 8 <= len(p) <= 63
    assert all(c.isprintable() and c.isascii() for c in p)


def test_it_is_not_the_shared_builtin() -> None:
    assert generate_ap_passphrase() != BUILTIN_PASSPHRASE


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

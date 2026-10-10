"""The pairing code never reaches the log."""

from __future__ import annotations

import hmac

from ados.core import pairing


def test_generating_a_code_logs_the_event_without_the_code(tmp_path, monkeypatch) -> None:
    events: list[tuple[str, dict]] = []
    monkeypatch.setattr(
        pairing.log, "info", lambda event, **kw: events.append((event, kw)), raising=False
    )
    pm = pairing.PairingManager(tmp_path / "pairing.json")
    code = pm.get_or_create_code()
    generated = [kw for event, kw in events if event == "pairing_code_generated"]
    assert generated, events
    for kw in generated:
        assert "code" not in kw
        assert code not in repr(kw)


def test_validate_key_uses_a_constant_time_compare(monkeypatch) -> None:
    seen: list[bytes] = []
    real = hmac.compare_digest

    def spy(a, b):
        seen.append(a)
        return real(a, b)

    monkeypatch.setattr(pairing.hmac, "compare_digest", spy)
    pm = pairing.PairingManager.__new__(pairing.PairingManager)
    pm._state = {"paired": True, "api_key": "ados_key"}
    pm._maybe_reload = lambda: None
    assert pm.validate_key("ados_key")
    assert not pm.validate_key("other")
    assert seen == [b"ados_key", b"other"]

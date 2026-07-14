"""Tests for the ``ados mcp`` token-management CLI."""

from __future__ import annotations

from unittest.mock import patch

from click.testing import CliRunner

from ados.cli.mcp import mcp_group


def test_status_renders_posture_and_tokens():
    runner = CliRunner()

    def fake_request(_method: str, path: str, *_a, **_k):
        assert path == "/api/mcp/status"
        return (
            200,
            {
                "accept_enabled": True,
                "any_minted": True,
                "tokens": [
                    {
                        "token_id": "mct_abc",
                        "label": "claude",
                        "scopes": ["read", "admin"],
                        "revoked": False,
                        "expired": False,
                    },
                    {
                        "token_id": "mct_old",
                        "label": "",
                        "scopes": ["read"],
                        "revoked": True,
                        "expired": False,
                    },
                ],
            },
        )

    with patch("ados.cli.mcp._request", side_effect=fake_request):
        out = runner.invoke(mcp_group, ["status"]).output
    assert "token acceptance: enabled" in out
    assert "mct_abc" in out
    assert "scopes=read,admin" in out
    # A revoked token is flagged.
    assert "revoked" in out


def test_status_reports_disabled_and_no_tokens():
    runner = CliRunner()
    payload = {"accept_enabled": False, "any_minted": False, "tokens": []}
    with patch("ados.cli.mcp._request", return_value=(200, payload)):
        out = runner.invoke(mcp_group, ["status"]).output
    assert "disabled" in out
    assert "no tokens minted" in out


def test_mint_prints_the_token_once():
    runner = CliRunner()
    captured: dict = {}

    def fake_request(_method: str, path: str, *_a, **kwargs):
        captured["path"] = path
        captured["json"] = kwargs.get("json")
        return (200, {"token": "blob.sig", "expires_at": 123})

    with patch("ados.cli.mcp._request", side_effect=fake_request):
        result = runner.invoke(mcp_group, ["mint", "--scope", "read", "--label", "claude"])
    assert result.exit_code == 0
    assert "blob.sig" in result.output
    assert captured["path"] == "/api/mcp/tokens"
    assert captured["json"]["scopes"] == ["read"]
    assert captured["json"]["label"] == "claude"
    # Default 30-day TTL in ms.
    assert captured["json"]["ttl_ms"] == 30 * 24 * 60 * 60 * 1000


def test_mint_requires_a_scope():
    runner = CliRunner()
    with patch("ados.cli.mcp._request") as req:
        result = runner.invoke(mcp_group, ["mint", "--label", "x"])
    # Missing the required --scope option: click exits non-zero, no request made.
    assert result.exit_code != 0
    req.assert_not_called()


def test_mint_rejects_an_unknown_scope():
    runner = CliRunner()
    with patch("ados.cli.mcp._request") as req:
        result = runner.invoke(mcp_group, ["mint", "--scope", "root"])
    assert result.exit_code != 0
    req.assert_not_called()


def test_revoke_one_and_all():
    runner = CliRunner()
    calls: list = []

    def fake_request(_method: str, _path: str, *_a, **kwargs):
        calls.append(kwargs.get("json"))
        return (200, {"ok": True})

    with patch("ados.cli.mcp._request", side_effect=fake_request):
        one = runner.invoke(mcp_group, ["revoke", "mct_abc"])
        every = runner.invoke(mcp_group, ["revoke", "--all"])
    assert one.exit_code == 0 and "Revoked." in one.output
    assert every.exit_code == 0 and "All MCP tokens revoked." in every.output
    assert calls[0] == {"token_id": "mct_abc"}
    assert calls[1] == {"all": True}


def test_revoke_without_target_errors():
    runner = CliRunner()
    with patch("ados.cli.mcp._request") as req:
        result = runner.invoke(mcp_group, ["revoke"])
    assert result.exit_code != 0
    req.assert_not_called()

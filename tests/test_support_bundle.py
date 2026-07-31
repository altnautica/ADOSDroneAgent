"""Tests for ``ados support-bundle``.

Weighted toward redaction, because that is the part where being wrong hands a
key to whoever the operator sends the archive to, and the failure is silent:
nothing errors, the bundle looks fine, and the secret is simply in it.
"""

from __future__ import annotations

import json
import tarfile
from pathlib import Path

from click.testing import CliRunner

from ados.cli.support import REDACTED, collect, redact, support_bundle


class TestRedaction:
    def test_a_json_api_key_value_is_removed_and_the_key_kept(self):
        # "there is an api_key configured" is what a support bundle needs to
        # show; the key itself is exactly what it must not carry.
        out = redact('{"api_key": "sk-live-abcdefghijklmnop", "port": 8080}')
        assert "sk-live-abcdefghijklmnop" not in out
        assert "api_key" in out
        assert REDACTED in out
        assert "8080" in out, "non-secret values must survive"

    def test_yaml_and_shell_forms_are_both_covered(self):
        for line in (
            "password: hunter2correcthorse",
            "PASSWORD=hunter2correcthorse",
            'wpa_passphrase = "hunter2correcthorse"',
            "token: hunter2correcthorse",
        ):
            assert "hunter2correcthorse" not in redact(line), line

    def test_matching_is_case_insensitive(self):
        assert "SEKRIT" not in redact("API_KEY: SEKRIT")
        assert "SEKRIT" not in redact("Secret: SEKRIT")

    def test_a_bare_key_blob_is_removed_even_with_no_key_name(self):
        # A wfb-ng keypair is 64 bytes, so its base64 is 88 characters. It can
        # appear in a log line with no adjacent "key:" to match on.
        blob = "A" * 88
        assert blob not in redact(f"installed keypair {blob} ok")

    def test_ordinary_text_and_short_tokens_survive(self):
        # Over-redaction makes the bundle useless, which is its own failure.
        text = "link_state: searching\nchannel: 149\ndevice_id: 40bb1a5a\nrssi: -51"
        out = redact(text)
        assert "searching" in out
        assert "149" in out
        assert "40bb1a5a" in out
        assert "-51" in out

    def test_a_realistic_config_blob_is_clean(self):
        cfg = json.dumps(
            {
                "security": {"api_key": "k" * 40, "hmac_secret": "s" * 44},
                "network": {"hotspot": {"password": "5KBZ66T4BR9B", "ssid": "ados-gs"}},
                "video": {"wfb": {"channel": 149, "fleet_slot": 2}},
            }
        )
        out = redact(cfg)
        assert "k" * 40 not in out
        assert "s" * 44 not in out
        assert "5KBZ66T4BR9B" not in out
        # Structure and non-secrets must remain legible.
        assert "ados-gs" in out
        assert "149" in out
        assert "fleet_slot" in out


class TestCollection:
    def test_a_failing_collector_does_not_abort_the_bundle(self, tmp_path, monkeypatch):
        # A box with a dead service is exactly the box someone is asking about,
        # so the failure is content, not a reason to give up on the rest.
        import ados.cli.support as sup

        def boom() -> str:
            raise RuntimeError("service is not running")

        monkeypatch.setattr(
            sup, "_collectors", lambda: [("ok.txt", lambda: "fine"), ("bad.txt", boom)]
        )
        manifest = collect(tmp_path)

        assert "ok.txt" in manifest["included"]
        assert (tmp_path / "ok.txt").read_text() == "fine"
        assert manifest["failed"] and manifest["failed"][0]["file"] == "bad.txt"
        # The failure is written into the archive, so the reader sees it.
        assert "service is not running" in (tmp_path / "bad.txt").read_text()

    def test_an_absent_source_is_skipped_not_failed(self, tmp_path, monkeypatch):
        # "this node has no fleet registry" and "reading it broke" are different
        # facts, and conflating them sends the reader looking for a fault that
        # is not there.
        import ados.cli.support as sup

        monkeypatch.setattr(sup, "_collectors", lambda: [("absent.json", lambda: None)])
        manifest = collect(tmp_path)

        assert manifest["skipped"] and manifest["skipped"][0]["file"] == "absent.json"
        assert not manifest["failed"]
        assert not (tmp_path / "absent.json").exists()

    def test_every_written_file_passes_through_redaction(self, tmp_path, monkeypatch):
        # Belt and braces: a collector added later must not be able to leak by
        # forgetting to redact its own output.
        import ados.cli.support as sup

        monkeypatch.setattr(
            sup,
            "_collectors",
            lambda: [("raw.txt", lambda: 'api_key: "leaked-secret-value"')],
        )
        collect(tmp_path)
        body = (tmp_path / "raw.txt").read_text()
        assert "leaked-secret-value" not in body
        assert REDACTED in body

    def test_the_manifest_records_what_is_in_the_archive(self, tmp_path, monkeypatch):
        import ados.cli.support as sup

        monkeypatch.setattr(sup, "_collectors", lambda: [("a.txt", lambda: "x")])
        collect(tmp_path)
        m = json.loads((tmp_path / "MANIFEST.json").read_text())
        assert m["included"] == ["a.txt"]
        assert "created_at" in m and "redaction" in m


class TestCommand:
    def test_it_writes_one_readable_archive_and_sends_nothing(self, tmp_path, monkeypatch):
        import ados.cli.support as sup

        monkeypatch.setattr(sup, "_collectors", lambda: [("a.txt", lambda: "hello")])
        monkeypatch.setattr(sup, "_get", lambda path: {"device_id": "testdev"})

        out = tmp_path / "bundle.tar.gz"
        res = CliRunner().invoke(support_bundle, ["--output", str(out)])

        assert res.exit_code == 0, res.output
        assert out.exists()
        with tarfile.open(out) as tar:
            names = [Path(n).name for n in tar.getnames()]
        assert "a.txt" in names
        assert "MANIFEST.json" in names
        # The operator must be told it stayed local; a bundle that might have
        # phoned home is one they cannot use on a restricted site.
        assert "sent anywhere" in res.output

    def test_an_unreachable_agent_still_produces_a_bundle(self, tmp_path, monkeypatch):
        # The agent being down is the most likely reason for needing one.
        import ados.cli.support as sup

        def unreachable(path):
            raise RuntimeError("connection refused")

        monkeypatch.setattr(sup, "_get", unreachable)
        monkeypatch.setattr(sup, "_collectors", lambda: [("a.txt", lambda: "hello")])

        out = tmp_path / "b.tar.gz"
        res = CliRunner().invoke(support_bundle, ["--output", str(out)])
        assert res.exit_code == 0, res.output
        assert out.exists()

    def test_the_archive_is_not_world_readable(self, tmp_path, monkeypatch):
        # Redacted is not the same as public: redaction removes what we can
        # name, and is not a promise that nothing sensitive remains.
        import ados.cli.support as sup

        monkeypatch.setattr(sup, "_collectors", lambda: [("a.txt", lambda: "hello")])
        monkeypatch.setattr(sup, "_get", lambda path: {"device_id": "d"})

        out = tmp_path / "b.tar.gz"
        CliRunner().invoke(support_bundle, ["--output", str(out)])
        assert oct(out.stat().st_mode)[-3:] == "600"

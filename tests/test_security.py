"""Tests for the ADOS security module: certs and firewall."""

from __future__ import annotations

import platform
import ssl
import tempfile
from pathlib import Path
from unittest.mock import patch

from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID

from ados.security.certs import (
    create_tls_context,
    generate_csr,
    generate_self_signed_cert,
    load_cert,
)
from ados.security.firewall import (
    FirewallConfig,
    apply_firewall_rules,
    generate_firewall_rules,
    save_firewall_rules,
)


# ---------------------------------------------------------------------------
# Certificate tests
# ---------------------------------------------------------------------------


class TestSelfSignedCert:
    def test_returns_pem_bytes(self):
        cert_pem, key_pem = generate_self_signed_cert("test.ados.local")
        assert isinstance(cert_pem, bytes)
        assert isinstance(key_pem, bytes)
        assert cert_pem.startswith(b"-----BEGIN CERTIFICATE-----")
        assert key_pem.startswith(b"-----BEGIN EC PRIVATE KEY-----")

    def test_valid_x509_with_correct_cn(self):
        cert_pem, _key_pem = generate_self_signed_cert("agent.ados.local")
        cert = load_cert(cert_pem)

        cn = cert.subject.get_attributes_for_oid(NameOID.COMMON_NAME)[0].value
        assert cn == "agent.ados.local"

    def test_uses_ecdsa_p256(self):
        cert_pem, _key_pem = generate_self_signed_cert("ec.test")
        cert = load_cert(cert_pem)

        pub_key = cert.public_key()
        assert isinstance(pub_key, ec.EllipticCurvePublicKey)
        assert isinstance(pub_key.curve, ec.SECP256R1)

    def test_self_signed_issuer_equals_subject(self):
        cert_pem, _key_pem = generate_self_signed_cert("self.signed")
        cert = load_cert(cert_pem)
        assert cert.issuer == cert.subject

    def test_saves_to_disk_when_cert_dir_provided(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            cert_pem, key_pem = generate_self_signed_cert("disk.test", cert_dir=tmpdir)

            cert_file = Path(tmpdir) / "cert.pem"
            key_file = Path(tmpdir) / "key.pem"

            assert cert_file.exists()
            assert key_file.exists()
            assert cert_file.read_bytes() == cert_pem
            assert key_file.read_bytes() == key_pem

            # Key file should have restrictive permissions
            mode = key_file.stat().st_mode & 0o777
            assert mode == 0o600


class TestCSR:
    def test_generates_valid_csr(self):
        _cert_pem, key_pem = generate_self_signed_cert("csr.test")
        csr_pem = generate_csr("csr.test", key_pem)

        assert isinstance(csr_pem, bytes)
        assert csr_pem.startswith(b"-----BEGIN CERTIFICATE REQUEST-----")

    def test_csr_has_correct_cn(self):
        from cryptography import x509 as x509_mod

        _cert_pem, key_pem = generate_self_signed_cert("my.csr")
        csr_pem = generate_csr("my.csr", key_pem)

        csr = x509_mod.load_pem_x509_csr(csr_pem)
        cn = csr.subject.get_attributes_for_oid(NameOID.COMMON_NAME)[0].value
        assert cn == "my.csr"


class TestTLSContext:
    def test_creates_tls_context_with_temp_files(self):
        cert_pem, key_pem = generate_self_signed_cert("tls.test")

        with tempfile.NamedTemporaryFile(suffix=".pem", delete=False) as cf:
            cf.write(cert_pem)
            cert_path = cf.name

        with tempfile.NamedTemporaryFile(suffix=".pem", delete=False) as kf:
            kf.write(key_pem)
            key_path = kf.name

        ctx = create_tls_context(cert_path, key_path)
        assert isinstance(ctx, ssl.SSLContext)
        assert ctx.minimum_version == ssl.TLSVersion.TLSv1_3

        # Cleanup
        Path(cert_path).unlink()
        Path(key_path).unlink()

    def test_context_with_ca_sets_verify_required(self):
        cert_pem, key_pem = generate_self_signed_cert("ca.test")

        with tempfile.NamedTemporaryFile(suffix=".pem", delete=False) as cf:
            cf.write(cert_pem)
            cert_path = cf.name

        with tempfile.NamedTemporaryFile(suffix=".pem", delete=False) as kf:
            kf.write(key_pem)
            key_path = kf.name

        # Use the same cert as CA (self-signed)
        ctx = create_tls_context(cert_path, key_path, ca_path=cert_path)
        assert ctx.verify_mode == ssl.CERT_REQUIRED

        Path(cert_path).unlink()
        Path(key_path).unlink()

    def test_context_without_ca_disables_verify(self):
        cert_pem, key_pem = generate_self_signed_cert("noca.test")

        with tempfile.NamedTemporaryFile(suffix=".pem", delete=False) as cf:
            cf.write(cert_pem)
            cert_path = cf.name

        with tempfile.NamedTemporaryFile(suffix=".pem", delete=False) as kf:
            kf.write(key_pem)
            key_path = kf.name

        ctx = create_tls_context(cert_path, key_path)
        assert ctx.verify_mode == ssl.CERT_NONE

        Path(cert_path).unlink()
        Path(key_path).unlink()


# ---------------------------------------------------------------------------
# Firewall tests
# ---------------------------------------------------------------------------


class TestFirewallRuleGeneration:
    def test_default_rules_contain_standard_ports(self):
        rules = generate_firewall_rules()
        joined = "\n".join(rules)

        # Standard ports
        assert "--dport 22" in joined      # SSH
        assert "--dport 8080" in joined     # API
        assert "--dport 8765" in joined     # WebSocket
        assert "--dport 5760" in joined     # TCP proxy
        assert "--dport 14550" in joined    # UDP proxy
        assert "--dport 14551" in joined    # UDP proxy

    def test_default_drop_policy(self):
        rules = generate_firewall_rules()
        assert "iptables -P INPUT DROP" in rules

    def test_loopback_and_established(self):
        rules = generate_firewall_rules()
        assert "iptables -A INPUT -i lo -j ACCEPT" in rules
        assert "iptables -A INPUT -m state --state ESTABLISHED,RELATED -j ACCEPT" in rules

    def test_mqtt_not_included_by_default(self):
        rules = generate_firewall_rules()
        joined = "\n".join(rules)
        assert "--dport 8883" not in joined

    def test_mqtt_included_when_enabled(self):
        config = FirewallConfig(allow_mqtt=True)
        rules = generate_firewall_rules(config)
        joined = "\n".join(rules)
        assert "--dport 8883" in joined

    def test_wireguard_included_when_enabled(self):
        config = FirewallConfig(allow_wireguard=True)
        rules = generate_firewall_rules(config)
        joined = "\n".join(rules)
        assert "--dport 51820" in joined

    def test_extra_ports(self):
        config = FirewallConfig(extra_tcp_ports=[9090], extra_udp_ports=[9999])
        rules = generate_firewall_rules(config)
        joined = "\n".join(rules)
        assert "--dport 9090" in joined
        assert "--dport 9999" in joined

    def test_custom_port_numbers(self):
        config = FirewallConfig(ssh_port=2222, api_port=9000)
        rules = generate_firewall_rules(config)
        joined = "\n".join(rules)
        assert "--dport 2222" in joined
        assert "--dport 9000" in joined


class TestFirewallApply:
    def test_skips_on_macos(self):
        """apply_firewall_rules should return True and skip on non-Linux."""
        rules = generate_firewall_rules()
        result = apply_firewall_rules(rules)
        if platform.system() != "Linux":
            assert result is True

    @patch("ados.security.firewall.platform.system", return_value="Darwin")
    def test_skips_when_platform_is_darwin(self, mock_system):
        rules = ["iptables -P INPUT DROP"]
        result = apply_firewall_rules(rules)
        assert result is True


class TestFirewallSave:
    def test_saves_rules_to_file(self):
        rules = generate_firewall_rules()
        with tempfile.TemporaryDirectory() as tmpdir:
            path = str(Path(tmpdir) / "test.rules")
            save_firewall_rules(rules, path=path)

            saved = Path(path).read_text()
            for rule in rules:
                assert rule in saved

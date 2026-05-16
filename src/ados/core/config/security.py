"""TLS, WireGuard, API security configuration."""

from __future__ import annotations

import os

from pydantic import BaseModel, Field

from ados.core.paths import CA_CERT_PATH, DEVICE_CERT_PATH, DEVICE_KEY_PATH


class TlsConfig(BaseModel):
    enabled: bool = True
    cert_path: str = str(DEVICE_CERT_PATH)
    key_path: str = str(DEVICE_KEY_PATH)
    ca_path: str = str(CA_CERT_PATH)


class WireguardConfig(BaseModel):
    enabled: bool = False
    config_path: str = "/etc/wireguard/ados.conf"


DEFAULT_CORS_ORIGINS: list[str] = [
    "http://localhost:4000",
    "http://127.0.0.1:4000",
    "http://localhost:4001",
    "http://127.0.0.1:4001",
]


class ApiSecurityConfig(BaseModel):
    api_key: str = ""
    cors_enabled: bool = True
    # Default origins ALWAYS apply unless an explicit override env var
    # is set. Ops files (/etc/ados/security.yaml) only need to populate
    # `cors_origins_extra` to allow additional origins; the defaults
    # are preserved automatically. This avoids the common
    # foot-gun where a deployment override drops localhost:4000 and
    # the local-dev Mission Control can no longer reach the agent.
    cors_origins: list[str] = Field(default_factory=lambda: list(DEFAULT_CORS_ORIGINS))
    # Additional origins added on top of the defaults. Empty by
    # default. The effective allowlist is `defaults | extras`.
    cors_origins_extra: list[str] = Field(default_factory=list)

    @property
    def effective_cors_origins(self) -> list[str]:
        """Return the deduped union of defaults + configured + extras.

        Defaults are ALWAYS merged in so a deployment yaml that sets
        `cors_origins:` to a custom list does not accidentally drop
        the local-dev Mission Control origins. To truly replace the
        allowlist (rare), set the `ADOS_CORS_ORIGINS_OVERRIDE` env
        var to a comma-separated list — that fully replaces.
        """
        override = os.environ.get("ADOS_CORS_ORIGINS_OVERRIDE", "").strip()
        if override:
            return [o.strip() for o in override.split(",") if o.strip()]
        seen: set[str] = set()
        merged: list[str] = []
        for origin in (
            *DEFAULT_CORS_ORIGINS,
            *self.cors_origins,
            *self.cors_origins_extra,
        ):
            if origin and origin not in seen:
                seen.add(origin)
                merged.append(origin)
        return merged


class SecurityConfig(BaseModel):
    tls: TlsConfig = TlsConfig()
    wireguard: WireguardConfig = WireguardConfig()
    api: ApiSecurityConfig = ApiSecurityConfig()
    hmac_enabled: bool = False
    hmac_secret: str = ""
    # Setup-webapp auth posture. False (default) trusts any browser served
    # the static webapp from this agent's own listening port (same-origin).
    # True requires an X-ADOS-Setup-Token header on every setup mutation;
    # the token is generated at first boot and printed by the CLI.
    setup_token_required: bool = False

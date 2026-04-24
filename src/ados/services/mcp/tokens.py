"""MCP session token store.

Tokens are JSON files at /etc/ados/mcp/tokens/{token_id}.json.
Pairing generates a 6-word BIP-39-style mnemonic the operator copies
into their external MCP client or the GCS pairing view.

Token lifecycle:
  pair → mint (returns mnemonic) → stored to disk → used (auth header)
  revoke → file marked revoked → refused at gate

The mnemonic is a 6-word phrase derived from the token secret using
a deterministic word list. The phrase is human-readable but not
cryptographically reversible — the secret is HMAC-derived, not the
phrase itself.
"""

from __future__ import annotations

import hashlib
import hmac
import json
import os
import secrets
import time
from pathlib import Path
from typing import Any

import structlog

log = structlog.get_logger()

# Simple 2048-word BIP-39-compatible list (first 2048 words).
# Loaded from package data on first access.
_WORDLIST: list[str] | None = None

# Fallback minimal wordlist if the package file is absent.
_FALLBACK_WORDS = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    "india", "juliet", "kilo", "lima", "mike", "november", "oscar", "papa",
    "quebec", "romeo", "sierra", "tango", "uniform", "victor", "whiskey",
    "xray", "yankee", "zulu", "able", "baker", "care", "dance", "enter",
    "first", "give", "have", "inner", "joint", "known", "light", "money",
    "never", "often", "place", "quite", "river", "since", "table", "under",
    "valid", "while", "years", "zebra", "amber", "basic", "cause", "depth",
    "early", "field", "grain", "heavy", "ideal", "judge", "knife", "large",
    "major", "night", "order", "paper", "quick", "range", "small", "trust",
    "upper", "value", "water", "exact", "young", "zilch", "crown", "drive",
    "event", "fixed", "guide", "heard", "image", "magic", "noble", "ocean",
    "point", "raise", "sharp", "tiger", "ultra", "vapor", "width", "yield",
    "armor", "black", "cloth", "dance", "equal", "flame", "grace", "harps",
    "ichor", "jewel", "lance", "maple", "north", "opera", "peace", "quest",
    "racer", "scout", "trace", "unity", "venom", "wagon", "xenon", "youth",
    "azure", "beach", "cedar", "denim", "eagle", "flute", "glass", "haven",
    "ivory", "joust", "karma", "lemon", "moose", "nexus", "onyx", "prism",
    "quasar", "relay", "slate", "tempo", "ultra", "visor", "world", "xenith",
    "yield", "zonal", "canal", "debut", "epoch", "flood", "grail", "honor",
]


def _wordlist() -> list[str]:
    global _WORDLIST
    if _WORDLIST is None:
        try:
            # Try to load a proper BIP-39 English wordlist from the package.
            wl_path = Path(__file__).parent / "data" / "bip39-english.txt"
            if wl_path.exists():
                _WORDLIST = wl_path.read_text().splitlines()[:2048]
            else:
                _WORDLIST = _FALLBACK_WORDS
        except Exception:
            _WORDLIST = _FALLBACK_WORDS
    return _WORDLIST


def _mnemonic_from_secret(secret: str) -> str:
    """Derive a 6-word mnemonic from a token secret."""
    wl = _wordlist()
    # Use SHA-256 of the secret as entropy source.
    digest = hashlib.sha256(secret.encode()).digest()
    words = []
    for i in range(6):
        chunk = int.from_bytes(digest[i * 4 : (i + 1) * 4], "big")
        words.append(wl[chunk % len(wl)])
    return " ".join(words)


class McpToken:
    """A session token granting scoped access to the MCP server."""

    SCOPES = {"read", "safe_write", "flight_action", "destructive", "secret_read", "assist"}

    def __init__(
        self,
        token_id: str,
        secret_hash: str,
        scopes: list[str],
        allowed_roots: list[str],
        client_hint: str,
        created_at: float,
        expires_at: float,
        revoked: bool = False,
        last_used_at: float | None = None,
    ) -> None:
        self.token_id = token_id
        self.secret_hash = secret_hash
        self.scopes = scopes
        self.allowed_roots = allowed_roots
        self.client_hint = client_hint
        self.created_at = created_at
        self.expires_at = expires_at
        self.revoked = revoked
        self.last_used_at = last_used_at

    @property
    def expired(self) -> bool:
        return time.time() > self.expires_at

    @property
    def active(self) -> bool:
        return not self.revoked and not self.expired

    def has_scope(self, scope: str) -> bool:
        return scope in self.scopes

    def to_dict(self) -> dict[str, Any]:
        return {
            "token_id": self.token_id,
            "secret_hash": self.secret_hash,
            "scopes": self.scopes,
            "allowed_roots": self.allowed_roots,
            "client_hint": self.client_hint,
            "created_at": self.created_at,
            "expires_at": self.expires_at,
            "revoked": self.revoked,
            "last_used_at": self.last_used_at,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "McpToken":
        return cls(
            token_id=d["token_id"],
            secret_hash=d["secret_hash"],
            scopes=d.get("scopes", ["read"]),
            allowed_roots=d.get("allowed_roots", []),
            client_hint=d.get("client_hint", "unknown"),
            created_at=d["created_at"],
            expires_at=d["expires_at"],
            revoked=d.get("revoked", False),
            last_used_at=d.get("last_used_at"),
        )


class TokenStore:
    """Manages MCP session tokens on disk at /etc/ados/mcp/tokens/."""

    def __init__(self, token_dir: str, default_ttl_days: int = 7) -> None:
        self._dir = Path(token_dir)
        self._default_ttl = default_ttl_days * 86400
        self._cache: dict[str, McpToken] = {}
        self._loaded = False

    def _ensure_dir(self) -> None:
        self._dir.mkdir(parents=True, exist_ok=True)
        self._dir.chmod(0o700)

    def load_all(self) -> None:
        """Load all token files from disk into memory cache."""
        self._ensure_dir()
        self._cache.clear()
        for f in self._dir.glob("*.json"):
            try:
                data = json.loads(f.read_text())
                t = McpToken.from_dict(data)
                self._cache[t.token_id] = t
            except Exception as e:
                log.warning("mcp_token_load_failed", file=str(f), error=str(e))
        self._loaded = True
        log.info("mcp_token_store_loaded", count=len(self._cache))

    def mint(
        self,
        client_hint: str = "unknown",
        scopes: list[str] | None = None,
        allowed_roots: list[str] | None = None,
        ttl_seconds: int | None = None,
    ) -> tuple[McpToken, str, str]:
        """Create a new token and return (token, mnemonic, hex_secret).

        The hex_secret is the actual bearer token MCP clients must present
        in their Authorization: Bearer header. It is returned ONCE at mint
        time and never again — only its SHA-256 hash is persisted.

        The mnemonic is a human-readable label derived one-way from the
        secret; it is not usable for authentication.
        """
        if not self._loaded:
            self.load_all()

        secret = secrets.token_hex(32)
        secret_hash = hashlib.sha256(secret.encode()).hexdigest()
        token_id = secrets.token_hex(8)
        now = time.time()
        ttl = ttl_seconds or self._default_ttl

        token = McpToken(
            token_id=token_id,
            secret_hash=secret_hash,
            scopes=scopes or ["read", "safe_write", "assist"],
            allowed_roots=allowed_roots or [],
            client_hint=client_hint,
            created_at=now,
            expires_at=now + ttl,
            revoked=False,
        )
        self._cache[token_id] = token
        self._persist(token)

        mnemonic = _mnemonic_from_secret(secret)
        log.info("mcp_token_minted", token_id=token_id, client_hint=client_hint)
        return token, mnemonic, secret

    def get(self, token_id: str) -> McpToken | None:
        if not self._loaded:
            self.load_all()
        return self._cache.get(token_id)

    def authenticate(self, bearer: str) -> McpToken | None:
        """Find an active token whose secret_hash matches the bearer value.
        The bearer is the hex secret itself (not the mnemonic).
        Returns None if not found, revoked, or expired.
        """
        if not self._loaded:
            self.load_all()
        bearer_hash = hashlib.sha256(bearer.encode()).hexdigest()
        for token in self._cache.values():
            if token.secret_hash == bearer_hash and token.active:
                token.last_used_at = time.time()
                self._persist(token)
                return token
        return None

    def revoke(self, token_id: str) -> bool:
        if not self._loaded:
            self.load_all()
        token = self._cache.get(token_id)
        if not token:
            return False
        token.revoked = True
        self._persist(token)
        log.info("mcp_token_revoked", token_id=token_id)
        return True

    def list_active(self) -> list[McpToken]:
        if not self._loaded:
            self.load_all()
        return [t for t in self._cache.values() if t.active]

    def list_all(self) -> list[McpToken]:
        if not self._loaded:
            self.load_all()
        return list(self._cache.values())

    def _persist(self, token: McpToken) -> None:
        self._ensure_dir()
        path = self._dir / f"{token.token_id}.json"
        tmp = path.with_suffix(".tmp")
        try:
            tmp.write_text(json.dumps(token.to_dict(), indent=2))
            tmp.chmod(0o600)
            os.replace(tmp, path)
        except OSError as e:
            log.warning("mcp_token_persist_failed", token_id=token.token_id, error=str(e))

    def hmac_verify(self, token_id: str, message: bytes, sig: str) -> bool:
        """Verify an HMAC-SHA256 signature using the token's secret_hash as key."""
        token = self._cache.get(token_id)
        if not token or not token.active:
            return False
        expected = hmac.new(token.secret_hash.encode(), message, hashlib.sha256).hexdigest()
        return hmac.compare_digest(expected, sig)

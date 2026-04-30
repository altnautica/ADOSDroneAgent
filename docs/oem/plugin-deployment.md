# Plugin Deployment Guide

The plugin system lets an OEM extend the agent without forking the source tree. Partner-specific behaviors, custom telemetry exporters, vendor camera drivers, and bespoke mission steps all ship as signed plugin archives that install on top of a stock agent image.

This document covers how an OEM gets a signing key registered, how to bake plugins into a factory image, how to revoke a compromised key, and how to debug a plugin that fails on the bench.

---

## 1. Overview

A plugin is a self-contained archive that the agent loads at runtime under a per-plugin systemd service. The agent enforces a sandbox boundary: each plugin runs as a child of the `ados-plugins.slice` cgroup tree with its own memory cap, CPU weight, and PID limit. The supervisor mints a short-lived capability token at start time, and the plugin can only call host services it has been granted permission for.

What an OEM gets out of this:

- Custom behaviors ship as plugin archives, not as patches against the agent source. Stock agent stays stock.
- Permission boundaries are explicit. A telemetry exporter cannot accidentally arm the vehicle.
- Plugins survive agent upgrades. Reinstalling the agent does not wipe installed plugins; the supervisor rediscovers them on next boot.
- Crash isolation. A plugin that segfaults takes down its own service, not the supervisor.

---

## 2. Distribution Model

Plugins ship as signed `.adosplug` archives. The format is a tar archive containing a `manifest.yaml` at the root, the agent half (Python module tree) and/or the GCS half (compiled JS bundle), plus any data files the plugin needs at runtime.

| Channel | Status | When to use |
|---------|--------|-------------|
| Local file (`ados plugin install /path/to/plugin.adosplug`) | Available now | Factory provisioning, bench testing, air-gapped fleets |
| Git URL (`ados plugin install https://git.example.com/.../plugin.adosplug`) | Roadmap | Internal OEM distribution from a private git host |
| Hosted registry (`ados plugin install com.partner.cool-feature`) | Roadmap | Public discovery and one-click install from the GCS |

An OEM building a factory image today uses the local-file path. Hosted registry submissions come later.

---

## 3. Signing Setup

The agent rejects unsigned plugins by default. To install plugins built by your team, the agent needs to trust a public key you control.

### Generating a Signing Keypair

Use Ed25519. The signing toolchain in the agent expects PEM-encoded keys.

```bash
# Generate the keypair on a workstation that is NOT a production device.
openssl genpkey -algorithm Ed25519 -out partner-signing.key
openssl pkey -in partner-signing.key -pubout -out partner-signing.pub
```

Keep the private key in a hardware security module or an encrypted vault. Never copy the private key to a production device or a CI runner that ships images.

### Registering the Public Key on the Device

Public keys live at `/etc/ados/plugin-keys/<key-id>.pem`. The `<key-id>` is a short slug naming the trust root, for example `partner-2026a` or `factory-line-3`. Use kebab-case, no whitespace, no path separators.

```bash
sudo install -m 0644 -o root -g root \
  partner-signing.pub /etc/ados/plugin-keys/partner-2026a.pem
```

Pair every key file with a metadata file that records validity windows:

```yaml
# /etc/ados/plugin-keys/partner-2026a.meta.yaml
key_id: partner-2026a
valid_from: "2026-01-01T00:00:00Z"
valid_to: "2027-12-31T23:59:59Z"
issuer: "Example OEM Plugin Signing Root"
contact: "security@example-oem.com"
```

The agent treats `valid_from` and `valid_to` as inclusive. A plugin signed with a key outside its window fails verification with a `signature_invalid` exit code.

### Signing a Plugin Archive

Use the bundled signing tool from the SDK. The signer reads the private key from disk, hashes the archive contents, and writes the detached signature into the archive footer.

```bash
ados plugin sign \
  --key partner-signing.key \
  --key-id partner-2026a \
  --in build/com.partner.cool-feature-1.0.0.unsigned.adosplug \
  --out dist/com.partner.cool-feature-1.0.0.adosplug
```

The signed `.adosplug` is what ships to factory provisioning and to the field.

---

## 4. Installing a Plugin in Factory Provisioning

Ship the signed archive alongside the production image and call the agent's CLI during the post-flash provisioning step.

```bash
# Drop the archive on the device under a path the provisioning script knows.
install -m 0644 \
  com.partner.cool-feature-1.0.0.adosplug \
  /var/ados/factory/plugins/

# Install during first boot or as part of the factory script.
ados plugin install /var/ados/factory/plugins/com.partner.cool-feature-1.0.0.adosplug \
  --yes

ados plugin enable com.partner.cool-feature
```

`--yes` skips the interactive permission prompt and refuses any plugin that requests `high` or `critical` risk permissions. If your plugin needs elevated permissions, run the install interactively on a development device first, confirm the permission request matches the manifest, and then either:

- accept the risk explicitly with the GCS install dialog on every device, or
- preload an explicit grant in the agent's plugin state file before calling `ados plugin install --yes`.

After enable, the agent starts the plugin service and the supervisor monitors it like any other agent unit.

### Verifying the Install

```bash
ados plugin list
ados plugin info com.partner.cool-feature
ados plugin perms com.partner.cool-feature
journalctl -u "ados-plugin@$(systemd-escape com.partner.cool-feature)" -n 50
```

The `info` output prints version, status, signer key id, and the granted permission set. The journal tail shows the plugin's structured log lines.

---

## 5. Revocation and Key Rotation

If a signing key is compromised, or if a plugin version turns out to be malicious or buggy, both the key and any plugin archives signed by it must be marked untrusted on every device in the fleet.

### Revocation File

The agent reads `/etc/ados/plugin-revocations.json` on supervisor start and on every install attempt. Any plugin or key listed there is rejected.

```json
{
  "revoked_keys": [
    {
      "key_id": "partner-2026a",
      "revoked_at": "2026-04-30T12:00:00Z",
      "reason": "signing key compromise reported 2026-04-29"
    }
  ],
  "revoked_plugins": [
    {
      "plugin_id": "com.partner.cool-feature",
      "version": "1.0.0",
      "revoked_at": "2026-04-30T12:00:00Z",
      "reason": "leaks operator GPS to third-party endpoint"
    }
  ]
}
```

A plugin signed by a revoked key fails install with exit code 3 (`signature_invalid`). A plugin matching a revoked `(plugin_id, version)` tuple fails install with exit code 4 (`permission_denied`). Already-installed plugins matching a revocation entry are stopped on the next supervisor reload and refuse to start until the revocation is cleared or the plugin is removed.

### Pushing a Revocation to the Fleet

The revocation file is plain JSON; any fleet config delivery channel works. Common patterns:

- Bundled with the next signed image release (revocations stack across releases).
- Pushed via the cloud config channel as a one-off `update_plugin_revocations` command.
- Manually placed by support engineers for a single device under repair.

After updating the file, restart the supervisor so it re-reads the revocation set:

```bash
sudo systemctl restart ados-supervisor
```

Already-running plugin services receive a clean stop signal during reload; the supervisor refuses to restart any service whose plugin is now revoked.

### Rotating a Signing Key

When a key reaches the end of its validity window, generate a new keypair, register the new public key alongside the old one, sign new releases with the new key, and add the old `key_id` to the revocation list once the field has migrated. The agent accepts multiple trust-root keys at once, so a transition window is supported without an outage.

---

## 6. CLI Quick Reference

| Command | Purpose |
|---------|---------|
| `ados plugin list` | Show installed plugins, version, status, signer key id. |
| `ados plugin install <archive>` | Verify signature, unpack, and register a `.adosplug`. |
| `ados plugin enable <id>` | Start the plugin service. |
| `ados plugin disable <id>` | Stop the plugin service, keep the install. |
| `ados plugin remove <id>` | Stop, uninstall, and forget a plugin. `--keep-data` preserves the data dir. |
| `ados plugin info <id>` | Manifest summary, runtime state, signer, permission grants. |
| `ados plugin perms <id>` | List recorded permissions. `--revoke <perm-id>` rescinds one (prompts; `--yes` skips). |
| `ados plugin logs <id>` | Tail the plugin's stdout/stderr log file. `--follow` streams. |
| `ados plugin lint <archive>` | Static-analyze a `.adosplug` before submission. |

All subcommands accept `--json` for machine-readable output. Exit codes follow the documented map: 0 ok, 1 generic, 2 manifest invalid, 3 signature invalid, 4 permission denied, 5 not found, 6 wrong state, 7 resource limit, 8 compatibility failed.

---

## 7. Resource Limits

Every plugin service runs under `ados-plugins.slice`. The slice itself is a cgroup with accounting on for CPU, memory, IO, and tasks. Per-plugin caps come from the manifest:

```yaml
agent:
  resources:
    memory_max: 128M
    cpu_weight: 50
    tasks_max: 64
```

The supervisor renders these into systemd directives on the per-plugin service unit. A plugin that exceeds `memory_max` is OOM-killed by the kernel and restarted by the supervisor with backoff. A plugin that hits `tasks_max` cannot fork further; the supervisor logs the breach and continues.

Default caps when the manifest does not specify any:

| Field | Default |
|-------|---------|
| `memory_max` | 64M |
| `cpu_weight` | 100 (default systemd weight) |
| `tasks_max` | 32 |

OEMs shipping a plugin that processes camera frames or runs an inference loop should declare realistic caps in the manifest. The supervisor will reject install if the requested cap exceeds a per-board ceiling configured at `/etc/ados/plugin-limits.yaml`.

---

## 8. Troubleshooting

### `signature_invalid` on install

The signing key is unknown, the signature does not match the archive contents, or the key is outside its `valid_from` / `valid_to` window.

```bash
ls /etc/ados/plugin-keys/
cat /etc/ados/plugin-keys/<key-id>.meta.yaml
ados plugin lint <archive>
```

If the key is missing from `/etc/ados/plugin-keys/`, install it first. If `lint` reports "key not trusted", the signer key id in the archive footer does not match any registered key. If the archive was tampered with after signing, re-sign from the original build artifact.

### `compatibility_failed` on install

The manifest declares a board, agent version, or capability set that the host does not satisfy.

```bash
ados plugin info <id>     # shows the manifest's compatibility block
ados version
cat /etc/ados/profile.conf
```

Plugins with `requires.board: example-board-v2` fail on a board reporting `example-board-v1`. Plugins with `requires.agent: ">=0.9"` fail on a 0.8.x agent.

### Manifest hash mismatch on supervisor start

The plugin's on-disk content has drifted from what the manifest declares. Either an upgrade was interrupted mid-write, or someone edited the install directory by hand. The supervisor refuses to start the plugin in this state to avoid running tampered code.

```bash
sudo journalctl -u ados-supervisor | grep manifest_hash_mismatch
sudo ados plugin remove <id>
sudo ados plugin install /path/to/known-good.adosplug
```

### Plugin crashed on enable

```bash
journalctl -u "ados-plugin@$(systemd-escape <plugin-id>)" -n 200
ados plugin logs <plugin-id> --lines 200
```

The first surface (`journalctl`) shows the systemd-level events: process start, exit code, watchdog timeouts. The second (`ados plugin logs`) shows the plugin's own stdout/stderr stream. Most failures come from a missing dependency on the host (camera not connected, FC not paired) or from a permission the manifest forgot to declare.

### `token_expired` errors in plugin logs

The capability token's TTL is 10 minutes. The supervisor mints a fresh token on every plugin restart and on every permission change. Plugins that hold a single token for hours hit this on the next request after expiry. The plugin SDK handles refresh automatically; if you see this error from a plugin you wrote yourself, upgrade to the current `ados-sdk` release or call `ctx.refresh_token()` before each long-lived operation.

### Plugin starts but cannot reach a host service

The most common cause is a missing permission grant. The manifest declared the permission, but the operator did not approve it during install. Check:

```bash
ados plugin perms <plugin-id>
```

A permission shown as `DENIED` blocks every call to that capability. Either grant it interactively (`ados plugin install --reinstall`, then approve at the prompt) or, for factory provisioning, add the grant to the plugin state file before first start.

### Recovery: full plugin reset

If the plugin state file is corrupted (rare; the supervisor uses file locking):

```bash
sudo systemctl stop ados-supervisor
sudo rm /var/ados/plugin-state.json
sudo systemctl start ados-supervisor
```

The supervisor rebuilds the state file by rescanning `/var/ados/plugins/` and re-registers every install dir it finds. Permission grants are preserved if the per-plugin `permissions.json` files survive; they are wiped only by `ados plugin remove`.

---

## Questions?

Contact: team@altnautica.com

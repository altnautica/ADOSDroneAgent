# Pairing Protocol (Field Tap-to-Pair)

How a relay gets admitted to a receiver's mesh, entirely from the OLED on each node. No laptop, no QR code, no app.

## Goals

1. No cable, no phone, no IP configuration.
2. Cryptographic guarantee that only the node approved on the receiver OLED can decrypt the invite bundle.
3. One receiver, many relays, in any order.
4. Works under packet loss. Works without NTP. Works under partitions.

## Transport

- **Interface:** `bat0` (the batman-adv virtual interface). Pairing only runs once the mesh carrier is up.
- **Port:** UDP 5801.
- **Listener:** the receiver binds UDP 5801 on `bat0` whenever the ground-station REST API is running in `receiver` role.
- **Retransmit:** the receiver sends the approval invite bundle twice with a 100 ms gap to survive a single dropped packet. The relay ignores duplicates that fail to decrypt.

## Crypto

- **Key exchange:** X25519 (Curve25519 ECDH). Every node generates a fresh key pair on first boot at `/etc/ados/mesh/keypair.json`.
- **Key derivation:** HKDF-SHA256 with a fixed context string to derive a 32-byte ChaCha20-Poly1305 key from the ECDH shared secret.
- **Bundle encryption:** ChaCha20-Poly1305 AEAD. The plaintext payload is JSON.
- **Revocations:** `/etc/ados/mesh/revocations.json` on the receiver. Each entry is a device id; any invite request from a revoked device id is dropped before even attempting decryption.

## State machine

```
[relay]                                    [receiver]
  |                                           |
  |-- OLED: Mesh > Set role -> Relay          |
  |-- OLED: Mesh > Join mesh                  |
  |     (mDNS resolves receiver)              |
  |                                           |
  |--- UDP 5801 join request ---------------->|
  |    {device_id, relay_pubkey}              |
  |                                           |
  |                                           |-- receiver OLED shows pending entry
  |                                           |-- operator scrolls to entry, Select to approve
  |                                           |
  |<-- UDP 5801 invite bundle (tx x 2) -------|
  |    ChaCha20-Poly1305 sealed:              |
  |    {mesh_id, mesh_psk, drone_channel,     |
  |     wfb_rx_key, receiver_mdns_host,       |
  |     receiver_mdns_port, issued_at_ms,     |
  |     expires_at_ms}                        |
  |                                           |
  |-- writes /etc/ados/mesh/{id,psk.key,...}  |
  |-- restarts ados-batman + ados-wfb-relay   |
  |-- OLED status flips to "Mesh: linked"     |
```

## Invite bundle contents

The bundle is a JSON payload sealed with ChaCha20-Poly1305. Field map:

| Field | Meaning | Where it lands on the relay |
|---|---|---|
| `mesh_id` | 16-char stable mesh identifier | `/etc/ados/mesh/id` |
| `mesh_psk` | 32-byte shared key for the mesh carrier | `/etc/ados/mesh/psk.key` (0600) |
| `drone_channel` | WFB-ng channel the drone is using | applied to `wfb_rx -f` |
| `wfb_rx_key` | Drone-paired WFB-ng RX key material | `/etc/ados/wfb.key` |
| `receiver_mdns_host` | mDNS host the receiver publishes on `bat0` | used by `wfb_rx -f` |
| `receiver_mdns_port` | Port the receiver listens on for forwarded WFB fragments | used by `wfb_rx -f` |
| `issued_at_ms` | Epoch ms when the receiver approved | consistency check |
| `expires_at_ms` | `issued_at_ms + 120_000` (2 minutes) | relay refuses stale bundles |

## Accept window

The receiver opens a pairing accept window for a bounded number of seconds (default 60, configurable). During the window:

- Incoming join requests are decoded and appended to a pending list.
- The OLED shows the pending list with an approve / reject action per entry.
- `GET /api/v1/ground-station/pair/pending` returns the same list.

When the window closes:

- The receiver stops accepting new join requests.
- The UDP listener stays bound but any new request is rejected with `410 Gone` behavior (no bundle sent).
- Pending entries that were not approved are discarded.

Reopen a new window whenever more relays need to be added.

## Revocation

To remove a relay from the mesh:

1. `ados gs mesh revoke <device_id>` on the receiver.
2. Agent appends the device id to `/etc/ados/mesh/revocations.json`.
3. The revoked relay's next join request (if any) is dropped before decryption.
4. The revoked relay continues to hold the old mesh PSK until its next factory reset or manual PSK rotation. For mesh PSK rotation, trigger a receiver-side key rollover (covered in the deployment playbook). For field use, revocation plus physical possession of the revoked node is usually enough.

## Factory reset behavior

A factory reset on either side of the pair wipes the crypto identity.

- `/etc/ados/mesh/keypair.json` removed.
- `/etc/ados/mesh/id` removed.
- `/etc/ados/mesh/psk.key` removed.
- `/etc/ados/mesh/revocations.json` removed on the receiver only.
- Role falls back to `direct`.

After factory reset, the node has to be re-paired from scratch. The receiver generates a new mesh id and PSK on first boot in receiver role; existing relays will not recognize the new mesh without a fresh invite.

## Packet-level reliability choices

- **Double-send with 100 ms gap.** UDP is lossy. Two copies with a short gap covers the common single-drop case without the complexity of an ACK protocol.
- **Duplicate tolerance.** The relay decrypts each datagram; the second copy decrypts to the same plaintext and updates no state, so it is effectively a no-op.
- **No NTP dependency.** The `issued_at_ms` and `expires_at_ms` fields are both set on the receiver and compared against the receiver's own clock on the receiver side. The relay only checks expiry against its monotonic clock + a 2-minute tolerance on receipt.
- **Out-of-order tolerance.** Every request contains the relay's pubkey. The receiver does not track sequence numbers; each request is evaluated in isolation.

## Source references

- `ADOSDroneAgent/src/ados/services/ground_station/pairing_manager.py`. receiver-side state machine, UDP 5801 listener, approval flow
- `ADOSDroneAgent/src/ados/services/ground_station/pairing_client.py`. relay-side invite request and bundle unpacking
- `ADOSDroneAgent/src/ados/services/ground_station/mesh_manager.py`. consumes the bundle fields to bring up batman-adv

## Related docs

- [cli-reference-mesh.md](cli-reference-mesh.md). `ados gs mesh accept`, `approve`, `revoke`, `join`
- [rest-api-mesh.md](rest-api-mesh.md). `/api/v1/ground-station/pair/*` routes
- [mesh-networking.md](mesh-networking.md). batman-adv carrier that pairing sits on top of

# Ground Station Role And Mesh Actions

Ground-station role, mesh, pairing, and WFB-ng actions are managed from the
setup webapp, Mission Control Hardware tab, OLED, and REST API.

The public operator CLI stays small:

```bash
ados
ados status
ados update
ados uninstall
```

## REST API

Every action talks to the local REST API at
`http://localhost:8080/api/v1/ground-station/*` and requires the node API key
when the endpoint is protected.

Common endpoints:

| Area | Endpoint |
|---|---|
| Role | `/api/v1/ground-station/role` |
| Mesh health | `/api/v1/ground-station/mesh` |
| Mesh neighbors | `/api/v1/ground-station/mesh/neighbors` |
| Mesh gateways | `/api/v1/ground-station/mesh/gateways` |
| Pair accept | `/api/v1/ground-station/pair/accept` |
| Pair approve | `/api/v1/ground-station/pair/approve/<device-id>` |
| Pair revoke | `/api/v1/ground-station/pair/revoke/<device-id>` |
| Relay status | `/api/v1/ground-station/wfb/relay/status` |
| Receiver relays | `/api/v1/ground-station/wfb/receiver/relays` |
| Receiver combined stream | `/api/v1/ground-station/wfb/receiver/combined` |

## Operator Surfaces

- **Setup webapp:** local field setup and role changes.
- **Mission Control Hardware tab:** richer monitoring and remote operator
  controls.
- **OLED/buttons:** no-laptop field pairing and basic role changes.
- **REST API:** automation and integration.

## Troubleshooting

| Symptom | Likely cause | Recovery |
|---|---|---|
| Mesh health shows `up: false` | Mesh dongle not detected or driver not loaded | Check `dmesg`, `iw list`, and confirm a second WiFi adapter is present. |
| Carrier is `none` | 802.11s SAE not available and IBSS fallback failed | Install mesh-capable `wpa_supplicant` package or switch adapter. |
| Pending list stays empty after relay joins | Relay never reached receiver on UDP 5801 | Confirm both nodes are on the same mesh carrier and `bat0` is up on receiver. |
| Approve returns a pubkey mismatch | Stale pending entry after relay rebooted | Ask relay to resend from OLED or setup webapp, then approve the new entry. |
| Gateway selection looks wrong | Local uplink is preferred or no gateway is reachable | Use Hardware tab Mesh view to inspect gateways and uplink priority. |


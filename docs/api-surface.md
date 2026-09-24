# Agent HTTP surface

Every route the agent serves, and who serves it. Generated -- do not
hand-edit. Regenerate with:

```
ADOSDroneAgent/.venv/bin/python scripts/gen-api-surface.py
```

Two guards keep it true. `crates/ados-control/tests/api_surface.rs`
fails when the native section drifts from `routing.rs`'s
`native_route_table()`. `scripts/check-api-surface.py` fails when any
client calls a path this table does not carry, which is what turns a
one-sided route rename from a silent 404 into a build failure.

A `{name}` segment matches one path segment; `{*name}` swallows the
tail. Method `WS` marks a route that answers only a WebSocket upgrade;
a plain HTTP request to it is not served.

## Native — `ados-control` on :8080

The front answers these itself. They take its own auth lane: rate
limiter, pairing gate, MCP-scope admission.

| Method | Path | Notes |
| --- | --- | --- |
| POST | `/api/_ws/ticket` |  |
| POST | `/api/atlas/capture/pause` |  |
| POST | `/api/atlas/capture/resume` |  |
| POST | `/api/atlas/capture/start` |  |
| POST | `/api/atlas/capture/stop` |  |
| PUT | `/api/atlas/config` |  |
| GET | `/api/atlas/readiness` |  |
| POST | `/api/can/passthrough` |  |
| GET | `/api/cloud/link` |  |
| POST | `/api/command` |  |
| GET | `/api/commands` |  |
| GET | `/api/compute/status` |  |
| GET | `/api/compute/workstation-credential` |  |
| POST | `/api/compute/workstation-credential` |  |
| GET | `/api/config` |  |
| PUT | `/api/config` |  |
| GET | `/api/config/schema` |  |
| POST | `/api/dashboard/pin/clear` | relay-forbidden |
| POST | `/api/dashboard/pin/set` | unauthenticated by design; relay-forbidden |
| GET | `/api/dashboard/pin/status` | unauthenticated by design |
| POST | `/api/dashboard/pin/verify` | unauthenticated by design |
| GET | `/api/diag/storage` |  |
| GET | `/api/diag/video` |  |
| GET | `/api/fleet/enrollment` |  |
| GET | `/api/fleet/peers` |  |
| GET | `/api/logs` |  |
| POST | `/api/logs/push` |  |
| GET | `/api/logs/stream` |  |
| GET | `/api/mavlink/ports` |  |
| GET | `/api/mavlink/signing/capability` |  |
| GET | `/api/mavlink/signing/counters` |  |
| POST | `/api/mavlink/signing/disable-on-fc` | relay-forbidden |
| POST | `/api/mavlink/signing/enroll-fc` |  |
| POST | `/api/mcp/revoke` | relay-forbidden |
| GET | `/api/mcp/status` |  |
| POST | `/api/mcp/tokens` | relay-forbidden |
| POST | `/api/pairing/claim` | unauthenticated by design |
| GET | `/api/pairing/code` | unauthenticated by design |
| GET | `/api/pairing/info` | unauthenticated by design |
| POST | `/api/pairing/unpair` | relay-forbidden |
| GET | `/api/params` |  |
| GET | `/api/params/{name}` |  |
| POST | `/api/params/{name}` |  |
| GET | `/api/ping` | unauthenticated by design |
| GET | `/api/plugins` |  |
| POST | `/api/plugins/capability-token` | relay-forbidden |
| POST | `/api/plugins/install` | relay-forbidden |
| POST | `/api/plugins/install_builtin` |  |
| POST | `/api/plugins/install_from_url` | relay-forbidden |
| WS | `/api/plugins/jobs/{job_id}` |  |
| POST | `/api/plugins/parse` |  |
| POST | `/api/plugins/parse_from_url` |  |
| DELETE | `/api/plugins/{plugin_id}` |  |
| GET | `/api/plugins/{plugin_id}` |  |
| GET | `/api/plugins/{plugin_id}/attestation` |  |
| POST | `/api/plugins/{plugin_id}/auto-update` |  |
| GET | `/api/plugins/{plugin_id}/config` |  |
| PUT | `/api/plugins/{plugin_id}/config` |  |
| POST | `/api/plugins/{plugin_id}/disable` |  |
| POST | `/api/plugins/{plugin_id}/enable` | relay-forbidden |
| GET | `/api/plugins/{plugin_id}/gcs/{*asset_path}` |  |
| POST | `/api/plugins/{plugin_id}/grant` | relay-forbidden |
| GET | `/api/plugins/{plugin_id}/manifest` |  |
| DELETE | `/api/plugins/{plugin_id}/perms/{permission_id}` |  |
| POST | `/api/plugins/{plugin_id}/pin` |  |
| GET | `/api/plugins/{plugin_id}/readiness` |  |
| GET | `/api/plugins/{plugin_id}/state` |  |
| POST | `/api/plugins/{plugin_id}/tools/{tool}/invoke` |  |
| POST | `/api/plugins/{plugin_id}/unpin` |  |
| DELETE | `/api/plugins/{plugin_id}/x/{*rest}` |  |
| GET | `/api/plugins/{plugin_id}/x/{*rest}` |  |
| PATCH | `/api/plugins/{plugin_id}/x/{*rest}` |  |
| POST | `/api/plugins/{plugin_id}/x/{*rest}` |  |
| PUT | `/api/plugins/{plugin_id}/x/{*rest}` |  |
| POST | `/api/relay/peer-secret` |  |
| GET | `/api/services` |  |
| POST | `/api/services/{name}/restart` | relay-forbidden |
| GET | `/api/status` |  |
| GET | `/api/status/full` |  |
| GET | `/api/swarm/neighbors` |  |
| GET | `/api/system` |  |
| GET | `/api/telemetry` |  |
| GET | `/api/time` |  |
| GET | `/api/v1/battery` |  |
| GET | `/api/v1/diagnostics` |  |
| POST | `/api/v1/ground-station/bluetooth/pair` |  |
| GET | `/api/v1/ground-station/bluetooth/paired` |  |
| POST | `/api/v1/ground-station/bluetooth/scan` |  |
| DELETE | `/api/v1/ground-station/bluetooth/{mac}` |  |
| POST | `/api/v1/ground-station/camera/switch` |  |
| GET | `/api/v1/ground-station/captive-token` |  |
| GET | `/api/v1/ground-station/crsf` |  |
| POST | `/api/v1/ground-station/crsf/channels` |  |
| POST | `/api/v1/ground-station/crsf/params` |  |
| GET | `/api/v1/ground-station/display` |  |
| PUT | `/api/v1/ground-station/display` |  |
| POST | `/api/v1/ground-station/fleet/hero` |  |
| GET | `/api/v1/ground-station/gamepads` |  |
| PUT | `/api/v1/ground-station/gamepads/primary` |  |
| GET | `/api/v1/ground-station/mesh` |  |
| GET | `/api/v1/ground-station/mesh/config` |  |
| PUT | `/api/v1/ground-station/mesh/config` |  |
| PUT | `/api/v1/ground-station/mesh/gateway_preference` |  |
| GET | `/api/v1/ground-station/mesh/gateways` |  |
| GET | `/api/v1/ground-station/mesh/neighbors` |  |
| GET | `/api/v1/ground-station/mesh/routes` |  |
| GET | `/api/v1/ground-station/modem-status` |  |
| GET | `/api/v1/ground-station/network` |  |
| PUT | `/api/v1/ground-station/network/ap` |  |
| DELETE | `/api/v1/ground-station/network/client` |  |
| PUT | `/api/v1/ground-station/network/client/join` |  |
| GET | `/api/v1/ground-station/network/client/scan` |  |
| GET | `/api/v1/ground-station/network/ethernet` |  |
| PUT | `/api/v1/ground-station/network/ethernet` |  |
| GET | `/api/v1/ground-station/network/modem` |  |
| PUT | `/api/v1/ground-station/network/modem` |  |
| GET | `/api/v1/ground-station/network/priority` |  |
| PUT | `/api/v1/ground-station/network/priority` |  |
| PUT | `/api/v1/ground-station/network/share_uplink` |  |
| POST | `/api/v1/ground-station/pair/accept` |  |
| POST | `/api/v1/ground-station/pair/approve/{device_id}` |  |
| POST | `/api/v1/ground-station/pair/close` |  |
| POST | `/api/v1/ground-station/pair/join` |  |
| GET | `/api/v1/ground-station/pair/pending` |  |
| POST | `/api/v1/ground-station/pair/revoke/{device_id}` |  |
| GET | `/api/v1/ground-station/pic` |  |
| POST | `/api/v1/ground-station/pic/claim` |  |
| POST | `/api/v1/ground-station/pic/confirm-token` |  |
| WS | `/api/v1/ground-station/pic/events` | unauthenticated by design |
| POST | `/api/v1/ground-station/pic/heartbeat` |  |
| POST | `/api/v1/ground-station/pic/release` |  |
| GET | `/api/v1/ground-station/recording/clip` |  |
| GET | `/api/v1/ground-station/recording/list` |  |
| GET | `/api/v1/ground-station/recording/segments` |  |
| POST | `/api/v1/ground-station/recording/start` |  |
| POST | `/api/v1/ground-station/recording/stop` |  |
| DELETE | `/api/v1/ground-station/recording/{segment}` |  |
| DELETE | `/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}` |  |
| GET | `/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}` |  |
| POST | `/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}` |  |
| PUT | `/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}` |  |
| GET | `/api/v1/ground-station/relayed/config` |  |
| POST | `/api/v1/ground-station/relayed/config` |  |
| GET | `/api/v1/ground-station/relayed/status` |  |
| GET | `/api/v1/ground-station/role` |  |
| PUT | `/api/v1/ground-station/role` |  |
| GET | `/api/v1/ground-station/status` |  |
| GET | `/api/v1/ground-station/ui` |  |
| PUT | `/api/v1/ground-station/ui/buttons` |  |
| PUT | `/api/v1/ground-station/ui/oled` |  |
| PUT | `/api/v1/ground-station/ui/screens` |  |
| GET | `/api/v1/ground-station/wfb` |  |
| PUT | `/api/v1/ground-station/wfb` |  |
| GET | `/api/v1/ground-station/wfb/atlas-relay/status` |  |
| DELETE | `/api/v1/ground-station/wfb/pair` | relay-forbidden |
| POST | `/api/v1/ground-station/wfb/pair` | relay-forbidden |
| DELETE | `/api/v1/ground-station/wfb/pair/{device_id}` |  |
| GET | `/api/v1/ground-station/wfb/receiver/combined` |  |
| GET | `/api/v1/ground-station/wfb/receiver/relays` |  |
| GET | `/api/v1/ground-station/wfb/relay/status` |  |
| WS | `/api/v1/ground-station/ws/buttons` | unauthenticated by design |
| WS | `/api/v1/ground-station/ws/mesh` | unauthenticated by design |
| WS | `/api/v1/ground-station/ws/uplink` | unauthenticated by design |
| DELETE | `/api/v1/network/client` |  |
| GET | `/api/v1/network/client/configured` |  |
| DELETE | `/api/v1/network/client/configured/{name}` |  |
| PUT | `/api/v1/network/client/configured/{name}/autoconnect` |  |
| PUT | `/api/v1/network/client/join` |  |
| GET | `/api/v1/network/client/status` |  |
| GET | `/api/v1/network/mac/adapters` |  |
| POST | `/api/v1/network/mac/pin` |  |
| DELETE | `/api/v1/network/mac/{iface}` |  |
| GET | `/api/v1/plugins/catalog` |  |
| POST | `/api/v1/system/restart-supervisor` | relay-forbidden |
| GET | `/api/v2/observability/{*upstream_path}` |  |
| GET | `/api/version` | unauthenticated by design |
| GET | `/api/video/config` |  |
| GET | `/api/video/latency` |  |
| POST | `/api/video/profile` |  |
| POST | `/api/video/record/start` |  |
| POST | `/api/video/record/stop` |  |
| GET | `/api/video/roster` |  |
| PUT | `/api/video/roster` |  |
| GET | `/api/vision/capabilities` |  |
| POST | `/api/vision/designate` |  |
| DELETE | `/api/vision/detector` |  |
| PUT | `/api/vision/detector` |  |
| POST | `/api/vision/models/upload` |  |
| GET | `/api/vision/status` |  |
| GET | `/api/wfb` |  |
| POST | `/api/wfb/channel` |  |
| GET | `/api/wfb/history` |  |
| GET | `/api/wfb/pair` |  |
| PUT | `/api/wfb/pair/auto-pair` |  |
| GET | `/api/wfb/pair/failover-status` |  |
| GET | `/api/wfb/pair/local-bind` | relay-forbidden |
| POST | `/api/wfb/pair/local-bind` | relay-forbidden |
| POST | `/api/wfb/pair/unpair` | relay-forbidden |
| PUT | `/api/wfb/tx-power` |  |
| GET | `/healthz` | unauthenticated by design |

200 native routes.

## Residual — FastAPI behind the front's proxy, same :8080

The front forwards these to the residual Python over its internal Unix
socket, authenticating them on the proxied lane first. A path under a
`PERMANENT_PYTHON_PREFIXES` prefix answers `501` when the residual is
absent (a known feature, not on this profile) rather than `404`.

| Method | Path | Notes |
| --- | --- | --- |
| POST | `/api/pairing/accept` | relay-forbidden |
| GET | `/api/peripherals` |  |
| POST | `/api/peripherals/scan` |  |
| GET | `/api/v1/dashboard/snapshot` |  |
| POST | `/api/v1/display/calibrate/start` |  |
| GET | `/api/v1/display/calibrate/status` |  |
| GET | `/api/v1/display/page` |  |
| POST | `/api/v1/display/page` |  |
| GET | `/api/v1/display/snapshot` |  |
| POST | `/api/v1/ground-station/factory-reset` | relay-forbidden |
| GET | `/api/v1/network/client/scan` |  |
| GET | `/api/v1/peripherals` |  |
| GET | `/api/v1/peripherals/{peripheral_id}` |  |
| POST | `/api/v1/peripherals/{peripheral_id}/action` |  |
| POST | `/api/v1/peripherals/{peripheral_id}/config` |  |
| POST | `/api/v1/setup/apply` |  |
| POST | `/api/v1/setup/cloud-choice` | relay-forbidden |
| WS | `/api/v1/setup/cloudflare/logs` |  |
| GET | `/api/v1/setup/cloudflare/verify` |  |
| POST | `/api/v1/setup/display/calibrate/start` |  |
| POST | `/api/v1/setup/display/install` |  |
| GET | `/api/v1/setup/display/job/{job_id}` |  |
| GET | `/api/v1/setup/display/options` |  |
| POST | `/api/v1/setup/finish` |  |
| GET | `/api/v1/setup/hardware-check` |  |
| POST | `/api/v1/setup/hardware-check/refresh` |  |
| GET | `/api/v1/setup/nudges` |  |
| POST | `/api/v1/setup/nudges/{nudge_id}/ack` |  |
| POST | `/api/v1/setup/profile` |  |
| POST | `/api/v1/setup/reboot` | relay-forbidden |
| POST | `/api/v1/setup/remote-access/cloudflare` | relay-forbidden |
| POST | `/api/v1/setup/reset` | relay-forbidden |
| POST | `/api/v1/setup/skip` |  |
| GET | `/api/v1/setup/status` |  |
| POST | `/api/v1/setup/step/{step_id}/skip` |  |
| GET | `/api/video` |  |
| POST | `/api/video/camera/switch` |  |
| GET | `/api/video/cameras` |  |
| POST | `/api/video/config` |  |
| POST | `/api/video/snapshot` |  |
| GET | `/api/video/snapshot.jpg` |  |
| GET | `/api/vision/detections/latest` |  |
| WS | `/api/vision/detections/ws` |  |
| GET | `/api/vision/models` |  |
| POST | `/api/vision/models/{model_id}/download` |  |
| GET | `/api/vision/models/{model_id}/status` |  |
| POST | `/api/vision/plugin-models/{plugin_id}/deliver` |  |
| GET | `/hls/{*path}` |  |
| POST | `/whep` |  |
| DELETE | `/whep/{session_id}` |  |
| PATCH | `/whep/{session_id}` |  |

51 residual routes.

## Logging store — `ados-logd` on :8090

A separate listener on its own port, dialled directly by Mission
Control's `direct` log tier and by `ados logs`. Not reachable through
either HTTP front.

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/v1/query` |  |
| GET | `/v1/tail` |  |
| GET | `/v1/aggregate` |  |
| GET | `/v1/export` |  |
| GET | `/v1/sessions` |  |
| GET | `/v1/stats` |  |
| GET | `/v1/healthz` |  |
| GET | `/v1/openapi.json` |  |

## Not an HTTP route

| Surface | Where |
| --- | --- |
| MAVLink WebSocket | `ws://<host>:8765/` — `ados-mavlink-router`, ticket or `X-ADOS-Key` |
| Relayed drone surface | `/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}` — the `{*path}` is the drone's own path from this table |


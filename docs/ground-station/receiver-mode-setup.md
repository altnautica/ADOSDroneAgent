# Receiver Mode Setup

A receiver is the hub of a distributed-receive deployment. It accepts relay
fragments over the local mesh, runs WFB-ng FEC combine, and republishes clean
video to the normal Ground Agent output pipeline.

Use the setup webapp, OLED, Mission Control Hardware tab, or REST API for
receiver setup.

## Flow

1. Install or upgrade the Ground Agent.
2. Confirm the node is mesh capable.
3. Switch role to receiver from OLED, setup webapp, Hardware tab, or
   `/api/v1/ground-station/role`.
4. Open the Accept relay window.
5. Approve pending relays as they join.
6. Watch combined stream stats from setup webapp, Hardware tab, or
   `/api/v1/ground-station/wfb/receiver/combined`.

## Recovery

| Symptom | Recovery |
|---|---|
| Receiver role starts but mesh is down | Confirm the second WiFi adapter is present and `ados-batman.service` is active. |
| No pending relay appears | Open Accept before the relay sends a request and confirm both nodes share the same mesh carrier. |
| Combined stream is empty | Confirm at least one local or relay radio is receiving drone fragments. |
| Need direct mode again | Use setup webapp or OLED role controls to switch back to direct. |

## References

- [Field pairing protocol](pairing-protocol.md).
- [Ground station role and mesh actions](cli-reference-mesh.md).
- [Mesh networking](mesh-networking.md).

